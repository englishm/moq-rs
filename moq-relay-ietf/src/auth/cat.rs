// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Common Access Token authorization (draft-ietf-moq-c4m).
//!
//! Verification happens once, at session setup: the decoded token is carried
//! in the session's [`Principal`], and per-request authorization is a pure
//! claims check with no cryptography. That keeps SUBSCRIBE off the signature
//! path while still re-checking expiry on every request.
//!
//! # What this hook enforces
//!
//! The signature, `exp`, `nbf`, `iss`, `aud`, and the `moqt` claim's actions,
//! namespace matches and track match. A token is required to carry `exp`, and
//! its lifetime is capped, because the relay has no revocation mechanism.
//!
//! Any *other* claim causes the token to be **refused**: honouring a token
//! while ignoring a constraint its issuer attached would grant more than was
//! authorized. The check is an allowlist over the claim keys present in the
//! raw payload, not a walk over the decoded token — see
//! [`unenforceable_claim`] for why that distinction is load-bearing.
//!
//! Composite claims (`and` / `or` / `nor`) are refused by the same rule.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use cat_token::{
    decode_token, CatError, CatToken, CatTokenValidator, Es256Algorithm, MoqtAction, MoqtValidator,
};
use moq_transport::coding::{TrackNamespace, TrackNamespacePrefix};

use super::{
    AuthDecision, AuthError, AuthHook, AuthRequest, AuthToken, AuthzOperation, DenyReason,
    Principal, CAT_TOKEN_TYPE,
};
use crate::{
    AuthKeyAlgorithm, AuthPublicKey, ScopeAuthConfig, SessionContext, MAX_SCOPE_AUTH_KEYS,
};

/// Longest token lifetime the relay will honour.
///
/// Caps how long a leaked token stays usable, and keeps `exp` far enough from
/// `i64::MAX` that adding a skew tolerance to it cannot overflow.
const MAX_TOKEN_LIFETIME_SECS: i64 = 7 * 24 * 60 * 60;

/// Largest clock skew tolerance the relay will honour, whatever a scope asks
/// for. Bounds both how far an expired token remains usable and the magnitude
/// of the `exp + skew` arithmetic.
const MAX_CLOCK_SKEW_SECS: i64 = 300;

/// Signature verifications a single CLIENT_SETUP may cost.
///
/// Verification is the expensive part of admission and it runs inline on the
/// accepting task, while both multipliers — how many tokens are presented and,
/// by choosing a `kid` that matches nothing, how many keys each is tried
/// against — are peer-controlled. Bounding the product rather than either
/// factor keeps the cost of an unauthenticated connection attempt flat as a
/// scope adds keys.
///
/// An ECDSA P-256 verification costs roughly 0.4 ms, so this bounds admission
/// at about 5 ms of CPU however the peer arranges its tokens — against the
/// 17 ms an unbounded [`MAX_SETUP_TOKENS`]-by-[`MAX_SCOPE_AUTH_KEYS`] fan-out
/// would allow.
///
/// Sized above the honest worst case rather than as tight as possible: a
/// client presenting two tokens to a scope mid-rotation, with neither carrying
/// a `kid`, needs ten. Denying that would be a silent, hard-to-diagnose
/// failure, and the CPU saved by refusing it is not worth much.
///
/// This, not [`MAX_SETUP_TOKENS`], is the operative limit on how many tokens
/// are usable: with a full key set and no `kid` to narrow the search, roughly
/// two are reachable before the budget runs out, even though four are decoded.
/// A `kid` that matches costs one verification per token, so a client whose
/// issuer labels its tokens is unaffected.
///
/// [`MAX_SETUP_TOKENS`]: super::MAX_SETUP_TOKENS
const MAX_SETUP_VERIFICATIONS: usize = 12;

// The budget only means something if it binds before the per-factor limits do.
const _: () = assert!(
    MAX_SETUP_VERIFICATIONS < super::MAX_SETUP_TOKENS * MAX_SCOPE_AUTH_KEYS,
    "the verification budget must bind before tokens x keys does"
);

/// Authorizes sessions and requests against Common Access Tokens.
pub struct CatAuthHook {
    /// Verification keys in the order the scope configured them, so a rotation
    /// overlap costs one verification for the common case.
    keys: Vec<CatKey>,

    /// Validates `exp`, `nbf`, `iss`, `aud` and the CAT-specific claims.
    token_validator: CatTokenValidator,

    /// Validates and evaluates the `moqt` claim.
    moqt_validator: MoqtValidator,

    /// Clock skew tolerance, mirrored here for the per-request expiry re-check.
    clock_skew: i64,

    /// Scope identity, for logging only.
    scope: Option<String>,
}

/// One configured verification key.
///
/// The token's own `alg` header is checked against the key by
/// `decode_token` before any signature work, so the key's algorithm does
/// not need to be tracked separately while ES256 is the only variant.
struct CatKey {
    kid: Option<String>,
    verifier: Es256Algorithm,
}

/// Session state established at setup and carried in the [`Principal`].
///
/// [`Principal`] already stores this behind an `Arc`, so the decoded token is
/// shared by every request on the session without a second layer of
/// indirection.
struct CatPrincipal {
    token: CatToken,

    /// `exp` snapshot, so the per-request freshness check is an integer
    /// comparison rather than a re-parse of the token.
    expires_at: i64,

    clock_skew: i64,
}

impl CatAuthHook {
    /// Build a hook from a scope's authorization configuration.
    ///
    /// Fails rather than returning a partially usable hook: a scope whose key
    /// material is wrong must fail closed as a whole, so that a typo in one
    /// key of a rotating set cannot silently degrade into a working
    /// single-key configuration.
    pub fn new(config: &ScopeAuthConfig, scope: Option<&str>) -> Result<Self, AuthError> {
        if config.keys.is_empty() {
            return Err(AuthError::Configuration(
                "no public keys configured".to_string(),
            ));
        }

        if config.keys.len() > MAX_SCOPE_AUTH_KEYS {
            return Err(AuthError::Configuration(format!(
                "{} public keys configured, at most {MAX_SCOPE_AUTH_KEYS} are allowed",
                config.keys.len()
            )));
        }

        // Duplicate identifiers would make key selection depend on ordering,
        // which turns a rotation into a coin flip.
        let mut seen = Vec::new();
        for key in &config.keys {
            if let Some(kid) = &key.kid {
                if seen.contains(&kid.as_str()) {
                    return Err(AuthError::Configuration(format!(
                        "duplicate key identifier {kid:?}"
                    )));
                }
                seen.push(kid.as_str());
            }
        }

        let keys = config
            .keys
            .iter()
            .enumerate()
            .map(|(index, key)| CatKey::build(index, key))
            .collect::<Result<Vec<_>, _>>()?;

        // Clamped rather than trusted: a coordinator returning an enormous
        // skew would otherwise keep expired tokens valid indefinitely, and
        // would keep expired tokens valid indefinitely.
        let clock_skew =
            duration_to_seconds(config.effective_clock_skew()).min(MAX_CLOCK_SKEW_SECS);

        let mut token_validator = CatTokenValidator::new()
            .with_clock_skew_tolerance(clock_skew)
            .map_err(|err| AuthError::Configuration(err.to_string()))?
            .allow_unencrypted_privacy_claims();
        if !config.issuers.is_empty() {
            token_validator = token_validator.with_expected_issuers(config.issuers.clone());
        }
        if !config.audiences.is_empty() {
            token_validator = token_validator.with_expected_audiences(config.audiences.clone());
        }

        // draft-ietf-moq-c4m: "If a recipient is unable to revalidate tokens,
        // it MUST reject all tokens with a 'moqt-reval' claim." This relay does
        // not revalidate mid-session, so saying so makes `validate_moqt_claims`
        // reject such tokens instead of silently ignoring the requirement.
        let moqt_validator = MoqtValidator::new().without_revalidation_support();

        tracing::info!(
            scope = scope.unwrap_or("<unscoped>"),
            keys = keys.len(),
            issuers = config.issuers.len(),
            audiences = config.audiences.len(),
            clock_skew_secs = clock_skew,
            "CAT token authorization enabled"
        );

        Ok(Self {
            keys,
            token_validator,
            moqt_validator,
            clock_skew,
            scope: scope.map(str::to_string),
        })
    }

    /// Verify a token against the configured keys and validate its claims.
    ///
    /// `budget` is the number of signature verifications still available for
    /// this CLIENT_SETUP; it is decremented as keys are tried. Exhausting it
    /// denies, so a peer cannot make admission arbitrarily expensive.
    ///
    /// Returns the decoded token together with its expiry, which is required
    /// and therefore known to be present once this succeeds.
    fn verify(&self, token: &AuthToken, budget: &mut usize) -> Result<(CatToken, i64), DenyReason> {
        if token.expose_value().is_empty() {
            return Err(DenyReason::TokenMalformed);
        }

        // This relay does not implement any application-defined COSE header
        // extensions. Accepting `crit` would claim otherwise and could ignore
        // a restriction required by the issuer.
        if has_critical_header(token.expose_value()) {
            return Err(DenyReason::TokenInvalid);
        }

        // The `kid` header, when present and matchable, narrows which keys are
        // worth trying. It is only a hint; see `header_kid`.
        let kid = header_kid(token.expose_value());

        // Try each key in turn. `decode_token` compares the header's
        // `alg` before verifying, so a key of the wrong algorithm costs no
        // signature operation.
        let mut best_error: Option<CatError> = None;
        let mut decoded = None;

        for key in self.candidate_keys(kid.as_deref()) {
            if *budget == 0 {
                tracing::debug!(
                    scope = self.scope.as_deref().unwrap_or("<unscoped>"),
                    "exhausted the signature verification budget for this session"
                );
                return Err(DenyReason::TokenInvalid);
            }
            *budget -= 1;

            match decode_token(token.expose_value(), &key.verifier) {
                Ok(token) => {
                    decoded = Some(token);
                    break;
                }
                Err(err) => best_error = Some(more_informative(best_error, err)),
            }
        }

        let verified = match decoded {
            Some(token) => token,
            None => {
                let err = best_error.unwrap_or(CatError::SignatureVerificationFailed);
                tracing::debug!(
                    scope = self.scope.as_deref().unwrap_or("<unscoped>"),
                    kid = kid.as_deref(),
                    error = %err,
                    "CAT token failed to verify against every configured key"
                );
                return Err(map_cat_error(err));
            }
        };

        // A bearer credential with no expiry is unrevokable: this relay has no
        // revocation list, and rotating a key only affects sessions
        // established afterwards. Requiring `exp` bounds the damage from a
        // leaked token to its lifetime.
        let Some(expires_at) = verified.claims().core.exp else {
            tracing::debug!(
                scope = self.scope.as_deref().unwrap_or("<unscoped>"),
                subject = verified.claims().informational.sub.as_deref(),
                "CAT token has no expiry"
            );
            return Err(DenyReason::TokenInvalid);
        };

        // Keep both timestamps inside the relay's supported lifetime horizon.
        // Bounding `exp` also serves the rule above: an expiry decades away is
        // the unbounded credential this check exists to prevent.
        let now = now_unix();
        let horizon = now.saturating_add(MAX_TOKEN_LIFETIME_SECS);

        if expires_at > horizon {
            tracing::debug!(
                scope = self.scope.as_deref().unwrap_or("<unscoped>"),
                subject = verified.claims().informational.sub.as_deref(),
                "CAT token expiry is implausibly distant"
            );
            return Err(DenyReason::TokenInvalid);
        }

        if let Some(not_before) = verified.claims().core.nbf {
            if !(now.saturating_sub(MAX_TOKEN_LIFETIME_SECS)..=horizon).contains(&not_before) {
                tracing::debug!(
                    scope = self.scope.as_deref().unwrap_or("<unscoped>"),
                    subject = verified.claims().informational.sub.as_deref(),
                    "CAT token not-before is implausibly distant"
                );
                return Err(DenyReason::TokenInvalid);
            }
        }

        // Reject constraints this relay cannot evaluate before consuming the
        // verified token into the validated trust state.
        if let Some(claim) = unenforceable_claim(token.expose_value()) {
            tracing::debug!(
                scope = self.scope.as_deref().unwrap_or("<unscoped>"),
                subject = verified.claims().informational.sub.as_deref(),
                claim,
                "CAT token carries a restriction this relay cannot enforce"
            );
            return Err(DenyReason::PolicyDenied {
                message: format!("unenforceable claim: {claim}"),
            });
        }

        // Standard CWT claims: exp, nbf, iss, aud, plus CAT extensions.
        let validated = verified.validate(&self.token_validator).map_err(|err| {
            tracing::debug!(
                scope = self.scope.as_deref().unwrap_or("<unscoped>"),
                error = %err,
                "CAT token claims rejected"
            );
            map_cat_error(err)
        })?;

        // MOQT claim well-formedness, including the revalidation contract.
        self.moqt_validator
            .validate_moqt_claims(validated.claims())
            .map_err(|err| {
                tracing::debug!(
                    scope = self.scope.as_deref().unwrap_or("<unscoped>"),
                    subject = validated.claims().informational.sub.as_deref(),
                    error = %err,
                    "CAT token MOQT claims rejected"
                );
                map_cat_error(err)
            })?;

        Ok((validated.into_inner(), expires_at))
    }

    /// The configured keys, ordered by how likely each is to verify a token
    /// bearing `kid`.
    ///
    /// `kid` only reorders; it never excludes. The signature is the sole
    /// authority on whether a token is authentic, and the identifier that
    /// selects a key is unauthenticated, so treating a mismatch as grounds for
    /// rejection would let an attacker-controlled label decide the outcome —
    /// and would turn an issuer relabelling its tokens, or a typo in one
    /// configured identifier, into an outage. At most
    /// [`MAX_SCOPE_AUTH_KEYS`] verifications are involved, once per session,
    /// so exhausting the list costs little.
    fn candidate_keys(&self, kid: Option<&str>) -> Vec<&CatKey> {
        match kid {
            Some(kid) => {
                let (named, rest): (Vec<_>, Vec<_>) = self
                    .keys
                    .iter()
                    .partition(|key| key.kid.as_deref() == Some(kid));
                named.into_iter().chain(rest).collect()
            }
            None => self.keys.iter().collect(),
        }
    }

    /// Evaluate the `moqt` claim for one operation.
    ///
    /// `established` is the caller's existing [`Principal`]; on an allow it is
    /// handed back as-is. Rebuilding one here would deep-clone the whole
    /// decoded token on every authorized request, which is both the hot path
    /// and peer-influenced in size, to produce a value the caller discards.
    fn authorize(
        &self,
        established: &Principal,
        principal: &CatPrincipal,
        operation: &AuthzOperation<'_>,
    ) -> AuthDecision {
        // A session outliving its token must stop being authorized, even
        // though no fresh signature check happens here. `exp` is required at
        // setup, so this check always applies.
        let Some(now) = try_now_unix() else {
            tracing::error!(
                scope = self.scope.as_deref().unwrap_or("<unscoped>"),
                "system clock is before the UNIX epoch; cannot evaluate token expiry"
            );
            return AuthDecision::deny(DenyReason::HookFault {
                message: "clock unavailable".to_string(),
            });
        };

        if now > principal.expires_at.saturating_add(principal.clock_skew) {
            tracing::debug!(
                scope = self.scope.as_deref().unwrap_or("<unscoped>"),
                subject = principal.token.informational.sub.as_deref(),
                operation = operation.label(),
                "CAT token expired mid-session"
            );
            return AuthDecision::deny(DenyReason::TokenExpired);
        }

        let (action, namespace, track) = map_operation(operation);
        if scope_authorizes(&principal.token, action, Some(&namespace), track.as_deref()) {
            AuthDecision::allow(established.clone())
        } else {
            tracing::debug!(
                scope = self.scope.as_deref().unwrap_or("<unscoped>"),
                subject = principal.token.informational.sub.as_deref(),
                operation = operation.label(),
                "CAT authorization denied: operation outside token scope"
            );
            AuthDecision::deny(DenyReason::ScopeMismatch)
        }
    }
}

impl std::fmt::Debug for CatAuthHook {
    /// Reports the shape of the configuration without its key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatAuthHook")
            .field("scope", &self.scope)
            .field("keys", &self.keys.len())
            .field(
                "kids",
                &self
                    .keys
                    .iter()
                    .map(|key| key.kid.as_deref().unwrap_or("<none>"))
                    .collect::<Vec<_>>(),
            )
            .field("clock_skew_secs", &self.clock_skew)
            .finish_non_exhaustive()
    }
}

impl CatKey {
    fn build(index: usize, key: &AuthPublicKey) -> Result<Self, AuthError> {
        let verifier = match key.algorithm {
            AuthKeyAlgorithm::Es256 => {
                Es256Algorithm::from_public_key_pem(&key.pem).map_err(|err| {
                    // The PEM itself is never echoed into the error: it is
                    // public material, but putting configuration into messages
                    // that reach logs is a habit worth not forming.
                    AuthError::Configuration(format!(
                        "key {index} ({}) is not a valid ES256 public key: {err}",
                        key.kid.as_deref().unwrap_or("no kid")
                    ))
                })?
            }
        };

        Ok(Self {
            kid: key.kid.clone(),
            verifier,
        })
    }
}

#[async_trait]
impl AuthHook for CatAuthHook {
    async fn on_setup(
        &self,
        session: &SessionContext,
        tokens: &[AuthToken],
    ) -> Result<AuthDecision, AuthError> {
        // §9.3.1.5 lets a peer present more than one token, and §9.2.2.1 lets
        // the parameter repeat. Accept the first that satisfies this scope
        // rather than only the first that is CAT-typed: a client mid-rotation,
        // or one holding tokens for several relays, legitimately sends
        // several, and only some will be usable here.
        let mut candidates = tokens
            .iter()
            .filter(|token| token.token_type == CAT_TOKEN_TYPE)
            .peekable();

        if candidates.peek().is_none() {
            tracing::debug!(
                scope = session.scope().unwrap_or("<unscoped>"),
                presented = tokens.len(),
                "no CAT token in CLIENT_SETUP"
            );
            return Ok(AuthDecision::deny(DenyReason::TokenMissing));
        }

        let mut budget = MAX_SETUP_VERIFICATIONS;
        let mut denial: Option<DenyReason> = None;
        let mut verified = None;
        for token in candidates {
            match self.verify(token, &mut budget) {
                Ok(result) => {
                    verified = Some(result);
                    break;
                }
                // Keep the most specific reason across tokens, for the same
                // reason `more_informative` does across keys: which token the
                // peer happened to list last should not decide what is
                // reported.
                Err(reason) => denial = Some(more_specific_denial(denial, reason)),
            }
        }

        let Some((decoded, expires_at)) = verified else {
            return Ok(AuthDecision::deny(
                denial.unwrap_or(DenyReason::TokenInvalid),
            ));
        };

        // Session establishment is itself an authorized action: a token that
        // grants no CLIENT_SETUP scope must not open a session. CLIENT_SETUP
        // carries no Track Name, so it is evaluated without the track
        // predicate, as the namespace-level actions are.
        if !scope_authorizes(&decoded, MoqtAction::ClientSetup, None, None) {
            tracing::debug!(
                scope = session.scope().unwrap_or("<unscoped>"),
                subject = decoded.informational.sub.as_deref(),
                "CAT token does not authorize CLIENT_SETUP"
            );
            return Ok(AuthDecision::deny(DenyReason::ScopeMismatch));
        }

        tracing::debug!(
            scope = session.scope().unwrap_or("<unscoped>"),
            subject = decoded.informational.sub.as_deref(),
            "CAT token accepted"
        );

        Ok(AuthDecision::allow(principal_from(
            decoded,
            expires_at,
            self.clock_skew,
        )))
    }

    async fn on_request(&self, request: &AuthRequest<'_>) -> Result<AuthDecision, AuthError> {
        let Some(principal) = request.principal.claims::<CatPrincipal>() else {
            // Only reachable if a principal from a different hook were routed
            // here, which the per-scope hook lifetime prevents. Fail closed and
            // surface it as a fault rather than a denial.
            return Err(AuthError::Backend(
                "session principal was not established by the CAT hook".to_string(),
            ));
        };

        Ok(self.authorize(request.principal, principal, &request.operation))
    }
}

/// Build the [`Principal`] carried for the session.
fn principal_from(token: CatToken, expires_at: i64, clock_skew: i64) -> Principal {
    Principal::new(
        token.informational.sub.clone(),
        CatPrincipal {
            token,
            expires_at,
            clock_skew,
        },
    )
}

/// Map a relay operation onto the MOQT action triple the `moqt` claim is
/// evaluated against.
///
/// Total by construction: [`AuthzOperation`] has one variant per live
/// authorization point, so adding a variant is a compile error here rather
/// than a silent denial at runtime.
fn map_operation(operation: &AuthzOperation<'_>) -> (MoqtAction, Vec<Vec<u8>>, Option<Vec<u8>>) {
    match operation {
        AuthzOperation::PublishNamespace { namespace } => (
            MoqtAction::PublishNamespace,
            namespace_tuple(namespace),
            None,
        ),
        AuthzOperation::Publish { namespace, track } => (
            MoqtAction::Publish,
            namespace_tuple(namespace),
            Some(track.as_bytes().to_vec()),
        ),
        AuthzOperation::Subscribe { namespace, track } => (
            MoqtAction::Subscribe,
            namespace_tuple(namespace),
            Some(track.as_bytes().to_vec()),
        ),
        AuthzOperation::SubscribeNamespace { prefix } => {
            (MoqtAction::SubscribeNamespace, prefix_tuple(prefix), None)
        }
        AuthzOperation::TrackStatus { namespace, track } => (
            MoqtAction::TrackStatus,
            namespace_tuple(namespace),
            Some(track.as_bytes().to_vec()),
        ),
    }
}

fn scope_authorizes(
    token: &CatToken,
    action: MoqtAction,
    namespace: Option<&[Vec<u8>]>,
    track: Option<&[u8]>,
) -> bool {
    token.moqt.moqt.as_ref().is_some_and(|scopes| {
        scopes.iter().any(|scope| {
            scope.allows_action(&action)
                && namespace.is_none_or(|namespace| scope.matches_namespace(namespace))
                && track.is_none_or(|track| scope.matches_track(track))
        })
    })
}

fn namespace_tuple(namespace: &TrackNamespace) -> Vec<Vec<u8>> {
    namespace
        .fields
        .iter()
        .map(|field| field.value.clone())
        .collect()
}

fn prefix_tuple(prefix: &TrackNamespacePrefix) -> Vec<Vec<u8>> {
    prefix
        .fields
        .iter()
        .map(|field| field.value.clone())
        .collect()
}

/// Read the `kid` from a token's protected header without verifying it.
///
/// This is a **hint only**. It narrows which keys are worth trying; the
/// signature check that follows is what establishes trust. Nothing here may
/// deny: the header is attacker-controlled at this point, so letting it
/// produce a verdict would hand the peer a say in its own authorization.
///
/// Every unusable shape therefore degrades to `None`, meaning "no hint, try
/// every key", and the token is judged solely on its signature. That includes
/// a `kid` that is not valid UTF-8 — RFC 8152 §3.1 types `kid` as `bstr` and
/// thumbprint-style identifiers are routinely non-textual, while
/// [`AuthPublicKey::kid`] is operator-facing text. Such a token simply has no
/// identifier this relay can match, which is not grounds for rejection.
///
/// Degrading is also cheap: `decode_token` reads `alg` from the same header
/// and fails before performing any signature operation, so a garbage header
/// costs a few CBOR parses rather than a set of ECDSA verifications.
///
/// [`AuthPublicKey::kid`]: crate::AuthPublicKey::kid
fn header_kid(token: &[u8]) -> Option<String> {
    let (header, _) = cose_parts(token)?;
    let value: ciborium::Value = ciborium::de::from_reader(header.as_slice()).ok()?;

    let ciborium::Value::Map(entries) = value else {
        return None;
    };

    entries.into_iter().find_map(|(label, value)| {
        // COSE header label 4 is `kid` (RFC 8152 §3.1).
        let ciborium::Value::Integer(label) = label else {
            return None;
        };
        if i64::try_from(label) != Ok(4) {
            return None;
        }

        match value {
            // cat-token encodes `kid` as a text string.
            ciborium::Value::Text(kid) => Some(kid),
            // COSE defines it as a byte string. Matchable only when it happens
            // to be text, since configured identifiers are text.
            ciborium::Value::Bytes(kid) => String::from_utf8(kid).ok(),
            _ => None,
        }
    })
}

/// Whether the protected COSE header declares any critical extension.
fn has_critical_header(token: &[u8]) -> bool {
    let Some((header, _)) = cose_parts(token) else {
        return false;
    };
    let Ok(ciborium::Value::Map(entries)) = ciborium::de::from_reader(header.as_slice()) else {
        return false;
    };

    entries.into_iter().any(|(label, _)| {
        matches!(label, ciborium::Value::Integer(label) if i64::try_from(label) == Ok(2))
    })
}

/// Pick the more useful of two verification failures.
///
/// A key whose signature check failed says nothing about the token beyond
/// "not this key", whereas an error raised after the signature verified —
/// malformed CBOR, a missing claim — describes the token itself. Reporting the
/// first failure encountered would make the denial reason, and the error code
/// on the wire, depend on how many keys the scope happens to have configured.
fn more_informative(current: Option<CatError>, candidate: CatError) -> CatError {
    fn is_key_mismatch(err: &CatError) -> bool {
        matches!(
            err,
            CatError::SignatureVerificationFailed | CatError::AlgorithmMismatch { .. }
        )
    }

    match current {
        Some(current) if !is_key_mismatch(&current) => current,
        _ => candidate,
    }
}

/// Pick the more useful of two token-level denials.
///
/// The same idea as [`more_informative`], one level up: with several tokens
/// presented, "this one is not for you" says less than "this one is expired",
/// and which the peer listed last should not decide what gets reported.
fn more_specific_denial(current: Option<DenyReason>, candidate: DenyReason) -> DenyReason {
    fn is_generic(reason: &DenyReason) -> bool {
        matches!(reason, DenyReason::TokenInvalid | DenyReason::TokenMissing)
    }

    match current {
        Some(current) if !is_generic(&current) => current,
        _ => candidate,
    }
}

/// CWT and CAT claim keys this relay either enforces or can safely ignore.
///
/// Everything else is refused. An allowlist rather than a denylist because a
/// claim the relay has never heard of is, by construction, one it is not
/// enforcing — and every CAT extension claim so far has been a restriction.
///
/// The permitted set is deliberately small:
///
/// | key | claim | why it is safe |
/// |-----|-------|----------------|
/// | 1 | `iss` | enforced |
/// | 2 | `sub` | identity, not a constraint |
/// | 3 | `aud` | enforced |
/// | 4 | `exp` | enforced, and required |
/// | 5 | `nbf` | enforced |
/// | 6 | `iat` | informational |
/// | 7 | `cti` | token identifier; only a constraint with a replay cache, and `catreplay` — which asks for one — is refused |
/// | 310 | `catv` | describes the token, not its bearer |
/// | 320 | `catifdata` | data for `catif`, which is itself refused |
/// | 323 | `catr` | renewal hint; the token still expires on schedule |
/// | 327 | `moqt` | enforced |
/// | 328 | `moqt-reval` | refused by `validate_moqt_claims` |
const PERMITTED_CLAIM_KEYS: &[i64] = &[1, 2, 3, 4, 5, 6, 7, 310, 320, 323, 327, 328];

/// The first claim in the token's payload that this relay cannot enforce.
///
/// Reads the **raw CWT payload** rather than the decoded [`CatToken`] so a
/// future dependency update cannot quietly widen the set of constraints this
/// relay accepts without corresponding enforcement support.
fn unenforceable_claim(raw_token: &[u8]) -> Option<String> {
    // A payload that cannot be re-read here already verified, so treat the
    // inconsistency as a reason to refuse rather than to trust.
    let Some((_, cbor)) = cose_parts(raw_token) else {
        return Some("unreadable payload".to_string());
    };

    let Ok(ciborium::Value::Map(entries)) = ciborium::de::from_reader(cbor.as_slice()) else {
        return Some("unreadable payload".to_string());
    };

    let mut present = Vec::new();
    for (key, _) in entries {
        let ciborium::Value::Integer(label) = key else {
            // CWT claim keys are integers; a text key is a JWT-ism and not
            // something this relay evaluates.
            return Some("non-integer claim key".to_string());
        };
        let Ok(label) = i64::try_from(label) else {
            return Some("claim out of range".to_string());
        };
        if !PERMITTED_CLAIM_KEYS.contains(&label) {
            return Some(claim_name(label));
        }
        // A claim key appearing twice leaves it to the decoder which copy
        // wins, so a second, differently-encoded copy could decide what the
        // relay enforces. RFC 8949 §5.6 calls duplicate map keys invalid
        // anyway; refusing removes the ambiguity rather than resolving it.
        if present.contains(&label) {
            return Some(format!("duplicate claim: {}", claim_name(label)));
        }
        present.push(label);
    }

    None
}

/// Extract the protected header and payload from a COSE_Sign1 or COSE_Mac0.
fn cose_parts(token: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let value: ciborium::Value = ciborium::de::from_reader(token).ok()?;
    let ciborium::Value::Tag(17 | 18, inner) = value else {
        return None;
    };
    let ciborium::Value::Array(parts) = *inner else {
        return None;
    };
    if parts.len() != 4 {
        return None;
    }
    let ciborium::Value::Bytes(header) = &parts[0] else {
        return None;
    };
    let ciborium::Value::Bytes(payload) = &parts[2] else {
        return None;
    };
    Some((header.clone(), payload.clone()))
}

/// A readable name for a claim key, for logs. Falls back to the number.
fn claim_name(key: i64) -> String {
    let name = match key {
        8 => "cnf",
        282 => "geohash",
        308 => "catreplay",
        309 => "catpor",
        311 => "catnip",
        312 => "catu",
        313 => "catm",
        314 => "catalpn",
        315 => "cath",
        316 => "catgeoiso3166",
        317 => "catgeocoord",
        318 => "catgeoalt",
        319 => "cattpk",
        321 => "catdpop",
        322 => "catif",
        324 => "or",
        325 => "nor",
        326 => "and",
        _ => return format!("claim {key}"),
    };
    name.to_string()
}

/// Translate a `cat-token` error into a relay denial reason.
fn map_cat_error(err: CatError) -> DenyReason {
    match err {
        CatError::TokenExpired | CatError::TokenNotYetValid => DenyReason::TokenExpired,
        CatError::InvalidIssuer => DenyReason::IssuerUnknown,
        CatError::ReplayAttackDetected => DenyReason::TokenReplayed,
        CatError::MoqtActionNotAuthorized(_) => DenyReason::ScopeMismatch,
        CatError::InvalidTokenFormat
        | CatError::InvalidCbor(_)
        | CatError::InvalidBase64(_)
        | CatError::MissingRequiredClaim(_) => DenyReason::TokenMalformed,
        CatError::InvalidAudience
        | CatError::SignatureVerificationFailed
        | CatError::UnsupportedAlgorithm(_)
        | CatError::AlgorithmMismatch { .. } => DenyReason::TokenInvalid,
        other => DenyReason::PolicyDenied {
            message: other.to_string(),
        },
    }
}

/// Seconds since the UNIX epoch, or `None` if the clock is before it.
///
/// A pre-epoch clock means the host has no usable notion of time, which makes
/// every expiry check meaningless. Callers must treat `None` as a denial:
/// substituting a placeholder would silently make every token look unexpired
/// for as long as the clock stayed wrong.
fn try_now_unix() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs() as i64)
}

/// Seconds since the UNIX epoch, saturating at 0 for a pre-epoch clock.
///
/// Only for comparisons where a too-small value is the conservative answer —
/// `try_now_unix` is the right choice anywhere a wrong clock must deny.
fn now_unix() -> i64 {
    try_now_unix().unwrap_or(0)
}

fn duration_to_seconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    // `CryptographicAlgorithm` brings `sign` and `algorithm_id` into scope for
    // the hand-rolled token minting below.
    use cat_token::{
        create_signing_input, encode_token, CatTokenBuilder, CryptographicAlgorithm, Cwt,
        MoqtScopeBuilder,
    };
    use moq_transport::coding::TrackName;
    use p256::pkcs8::EncodePublicKey;

    /// A generated ES256 keypair plus the PEM a scope would be configured with.
    struct TestKey {
        signer: Es256Algorithm,
        pem: String,
    }

    fn generate_key() -> TestKey {
        let signer = Es256Algorithm::new_with_key_pair().expect("keypair");
        let pem = signer
            .verifying_key()
            .to_public_key_pem(Default::default())
            .expect("public key pem");
        TestKey { signer, pem }
    }

    /// Encode a token carrying a `kid` header.
    ///
    /// `cat_token::encode_token` builds its header from a freshly constructed
    /// `Cwt`, whose `kid` is always `None`, so the library cannot emit one.
    /// Third-party C4M issuers can and do, which is exactly the case
    /// [`header_kid`] exists to handle — so the tests mint such tokens through
    /// the same public CWT primitives the library itself uses.
    fn encode_with_kid(token: &CatToken, signer: &Es256Algorithm, kid: &str) -> Vec<u8> {
        encode_with_raw_kid(token, signer, ciborium::Value::Text(kid.to_string()))
    }

    /// As [`encode_with_kid`], but with an arbitrary CBOR value in the `kid`
    /// header, for exercising the encodings COSE permits beyond a text string.
    fn encode_with_raw_kid(
        token: &CatToken,
        signer: &Es256Algorithm,
        kid: ciborium::Value,
    ) -> Vec<u8> {
        encode_with_extra_headers(
            token,
            signer,
            vec![(ciborium::Value::Integer(4.into()), kid)],
        )
    }

    fn encode_with_extra_headers(
        token: &CatToken,
        signer: &Es256Algorithm,
        extra: Vec<(ciborium::Value, ciborium::Value)>,
    ) -> Vec<u8> {
        let cwt = Cwt::new(signer.algorithm_id(), token.clone());

        let mut headers = vec![
            (
                ciborium::Value::Integer(1.into()),
                ciborium::Value::Integer(signer.algorithm_id().into()),
            ),
            (
                ciborium::Value::Integer(16.into()),
                ciborium::Value::Text("CAT".to_string()),
            ),
        ];
        headers.extend(extra);
        let header = ciborium::Value::Map(headers);

        let mut header_cbor = Vec::new();
        ciborium::ser::into_writer(&header, &mut header_cbor).expect("header cbor");
        let payload_cbor = cwt.encode_payload().expect("payload cbor");

        let signing_input =
            create_signing_input(&header_cbor, &payload_cbor, signer.algorithm_id())
                .expect("signing input");
        let signature = signer.sign(&signing_input).expect("sign");

        let cose = ciborium::Value::Tag(
            18,
            Box::new(ciborium::Value::Array(vec![
                ciborium::Value::Bytes(header_cbor),
                ciborium::Value::Map(vec![]),
                ciborium::Value::Bytes(payload_cbor),
                ciborium::Value::Bytes(signature),
            ])),
        );
        let mut encoded = Vec::new();
        ciborium::ser::into_writer(&cose, &mut encoded).expect("COSE_Sign1");
        encoded
    }

    fn config(keys: Vec<AuthPublicKey>) -> ScopeAuthConfig {
        ScopeAuthConfig::new(keys)
            .with_issuers(vec!["test-issuer".to_string()])
            .with_audiences(vec!["test-relay".to_string()])
    }

    fn hook(keys: Vec<AuthPublicKey>) -> CatAuthHook {
        CatAuthHook::new(&config(keys), Some("test-scope")).expect("hook builds")
    }

    /// A token granting CLIENT_SETUP plus publisher rights under a namespace
    /// prefix.
    fn publisher_token(signer: &Es256Algorithm, prefix: &[&[u8]]) -> Bytes {
        mint(signer, prefix, true, None, 3600)
    }

    /// A token granting CLIENT_SETUP plus subscriber rights under a prefix.
    fn subscriber_token(signer: &Es256Algorithm, prefix: &[&[u8]]) -> Bytes {
        mint(signer, prefix, false, None, 3600)
    }

    fn mint(
        signer: &Es256Algorithm,
        prefix: &[&[u8]],
        publisher: bool,
        kid: Option<&str>,
        expires_in: i64,
    ) -> Bytes {
        let mut scope = MoqtScopeBuilder::new();
        scope = if publisher {
            scope.publisher()
        } else {
            scope.subscriber()
        };
        for part in prefix {
            scope = scope.namespace_prefix(part);
        }
        let scope = scope.track_prefix(b"").build();

        let setup_scope = MoqtScopeBuilder::new()
            .action(MoqtAction::ClientSetup)
            .build();

        let token = CatTokenBuilder::new()
            .issuer("test-issuer")
            .single_audience("test-relay")
            .subject("test-subject")
            .expires_in(expires_in)
            .moqt_scope(scope)
            .moqt_scope(setup_scope)
            .build()
            .expect("build token");

        let encoded = match kid {
            Some(kid) => encode_with_kid(&token, signer, kid),
            None => encode_token(&token, signer).expect("encode"),
        };
        Bytes::from(encoded)
    }

    fn auth_token(value: Bytes) -> AuthToken {
        AuthToken::new(CAT_TOKEN_TYPE, value.to_vec())
    }

    fn session() -> SessionContext {
        SessionContext::public(Some("test-scope".to_string()))
    }

    async fn principal_for(hook: &CatAuthHook, value: Bytes) -> Principal {
        hook.on_setup(&session(), &[auth_token(value)])
            .await
            .expect("no fault")
            .into_principal()
            .expect("allowed")
    }

    /// A valid, minimally-scoped token: publisher under `sports`, plus setup.
    fn base_token() -> CatToken {
        CatTokenBuilder::new()
            .issuer("test-issuer")
            .single_audience("test-relay")
            .subject("test-subject")
            .expires_in(3600)
            .moqt_scope(
                MoqtScopeBuilder::new()
                    .publisher()
                    .namespace_prefix(b"sports")
                    .track_prefix(b"")
                    .build(),
            )
            .moqt_scope(
                MoqtScopeBuilder::new()
                    .action(MoqtAction::ClientSetup)
                    .build(),
            )
            .build()
            .expect("build token")
    }

    /// Encode [`base_token`] with one extra claim spliced into the payload.
    ///
    /// Goes through the CBOR directly because the point is to produce shapes
    /// `cat-token`'s own encoder cannot, which is precisely what a third-party
    /// issuer following the RFCs will emit.
    fn mint_with_extra_claim(
        signer: &Es256Algorithm,
        claim_key: i64,
        value: ciborium::Value,
    ) -> Vec<u8> {
        let cwt = Cwt::new(signer.algorithm_id(), base_token());
        let payload_cbor = cwt.encode_payload().expect("payload cbor");

        let ciborium::Value::Map(mut entries) =
            ciborium::de::from_reader::<ciborium::Value, _>(payload_cbor.as_slice())
                .expect("payload is a map")
        else {
            panic!("payload is a map");
        };
        entries.push((ciborium::Value::Integer(claim_key.into()), value));

        let mut payload_cbor = Vec::new();
        ciborium::ser::into_writer(&ciborium::Value::Map(entries), &mut payload_cbor)
            .expect("re-encode");

        let header = ciborium::Value::Map(vec![
            (
                ciborium::Value::Integer(1.into()),
                ciborium::Value::Integer(signer.algorithm_id().into()),
            ),
            (
                ciborium::Value::Integer(16.into()),
                ciborium::Value::Text("CAT".to_string()),
            ),
        ]);
        let mut header_cbor = Vec::new();
        ciborium::ser::into_writer(&header, &mut header_cbor).expect("header cbor");

        let signing_input =
            create_signing_input(&header_cbor, &payload_cbor, signer.algorithm_id())
                .expect("signing input");
        let signature = signer.sign(&signing_input).expect("sign");

        let cose = ciborium::Value::Tag(
            18,
            Box::new(ciborium::Value::Array(vec![
                ciborium::Value::Bytes(header_cbor),
                ciborium::Value::Map(vec![]),
                ciborium::Value::Bytes(payload_cbor),
                ciborium::Value::Bytes(signature),
            ])),
        );
        let mut encoded = Vec::new();
        ciborium::ser::into_writer(&cose, &mut encoded).expect("COSE_Sign1");
        encoded
    }

    /// A token whose scopes carry a non-empty track prefix — the canonical
    /// publisher shape, and the one the library's own `roles::publisher`
    /// helper produces.
    fn track_scoped_token(signer: &Es256Algorithm, track_prefix: &[u8]) -> Bytes {
        let token = CatTokenBuilder::new()
            .issuer("test-issuer")
            .single_audience("test-relay")
            .subject("test-subject")
            .expires_in(3600)
            .moqt_scope(
                MoqtScopeBuilder::new()
                    .publisher()
                    .subscriber()
                    .namespace_prefix(b"sports")
                    .track_prefix(track_prefix)
                    .build(),
            )
            .moqt_scope(
                MoqtScopeBuilder::new()
                    .action(MoqtAction::ClientSetup)
                    .build(),
            )
            .build()
            .expect("build token");

        Bytes::from(encode_token(&token, signer).expect("encode"))
    }

    /// Copy a principal with its recorded expiry moved.
    ///
    /// Expiry is checked against the system clock, which a paused tokio
    /// runtime does not control, so moving the token's expiry is the only way
    /// to exercise the boundary without sleeping through real seconds.
    fn rewind_expiry(claims: &CatPrincipal, expires_at: i64) -> Principal {
        Principal::new(
            claims.token.informational.sub.clone(),
            CatPrincipal {
                token: claims.token.clone(),
                expires_at,
                clock_skew: claims.clock_skew,
            },
        )
    }

    async fn decide(
        hook: &CatAuthHook,
        principal: &Principal,
        operation: AuthzOperation<'_>,
    ) -> AuthDecision {
        let request = AuthRequest {
            session: &session(),
            principal,
            operation,
            request_id: Some(1),
        };
        hook.on_request(&request).await.expect("no fault")
    }

    // ------------------------------------------------------------------
    // Construction / configuration
    // ------------------------------------------------------------------

    #[test]
    fn rejects_an_empty_key_set() {
        let err = CatAuthHook::new(&config(vec![]), None).expect_err("must fail");
        assert!(matches!(err, AuthError::Configuration(_)), "{err:?}");
    }

    #[test]
    fn rejects_more_than_the_maximum_keys() {
        let keys = (0..MAX_SCOPE_AUTH_KEYS + 1)
            .map(|_| AuthPublicKey::es256(generate_key().pem))
            .collect();

        let err = CatAuthHook::new(&config(keys), None).expect_err("must fail");
        assert!(
            err.to_string().contains("at most 5"),
            "{err} should name the bound"
        );
    }

    #[test]
    fn accepts_exactly_the_maximum_keys() {
        let keys = (0..MAX_SCOPE_AUTH_KEYS)
            .map(|_| AuthPublicKey::es256(generate_key().pem))
            .collect();

        assert!(CatAuthHook::new(&config(keys), None).is_ok());
    }

    #[test]
    fn rejects_unparseable_key_material() {
        let good = generate_key();
        let keys = vec![
            AuthPublicKey::es256(good.pem),
            AuthPublicKey::es256("-----BEGIN PUBLIC KEY-----\nnope\n-----END PUBLIC KEY-----"),
        ];

        // One bad key fails the whole scope: a rotating set must not silently
        // degrade into a working single-key configuration.
        let err = CatAuthHook::new(&config(keys), None).expect_err("must fail");
        assert!(matches!(err, AuthError::Configuration(_)), "{err:?}");
    }

    #[test]
    fn rejects_duplicate_key_identifiers() {
        let keys = vec![
            AuthPublicKey::es256(generate_key().pem).with_kid("rotating"),
            AuthPublicKey::es256(generate_key().pem).with_kid("rotating"),
        ];

        let err = CatAuthHook::new(&config(keys), None).expect_err("must fail");
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    // ------------------------------------------------------------------
    // Setup
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn accepts_a_valid_token() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let decision = hook
            .on_setup(
                &session(),
                &[auth_token(publisher_token(&key.signer, &[b"sports"]))],
            )
            .await
            .unwrap();

        assert!(decision.is_allowed());
        assert_eq!(
            decision.principal().unwrap().subject(),
            Some("test-subject")
        );
    }

    #[tokio::test]
    async fn denies_when_no_token_is_presented() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let decision = hook.on_setup(&session(), &[]).await.unwrap();
        assert!(matches!(
            decision.deny_reason(),
            Some(DenyReason::TokenMissing)
        ));
    }

    #[tokio::test]
    async fn ignores_tokens_of_other_types() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let other = AuthToken::new(0, b"out-of-band".to_vec());

        let decision = hook.on_setup(&session(), &[other]).await.unwrap();
        assert!(matches!(
            decision.deny_reason(),
            Some(DenyReason::TokenMissing)
        ));
    }

    #[tokio::test]
    async fn denies_a_token_signed_by_an_unknown_key() {
        let configured = generate_key();
        let attacker = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(configured.pem)]);

        let decision = hook
            .on_setup(
                &session(),
                &[auth_token(publisher_token(&attacker.signer, &[b"sports"]))],
            )
            .await
            .unwrap();

        assert!(matches!(
            decision.deny_reason(),
            Some(DenyReason::TokenInvalid)
        ));
    }

    #[tokio::test]
    async fn denies_an_expired_token() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        // Well past the default 60s skew tolerance.
        let expired = mint(&key.signer, &[b"sports"], true, None, -3600);

        let decision = hook
            .on_setup(&session(), &[auth_token(expired)])
            .await
            .unwrap();
        assert!(matches!(
            decision.deny_reason(),
            Some(DenyReason::TokenExpired)
        ));
    }

    #[tokio::test]
    async fn denies_a_token_from_an_unexpected_issuer() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let scope = MoqtScopeBuilder::new()
            .publisher()
            .namespace_prefix(b"sports")
            .track_prefix(b"")
            .build();
        let token = CatTokenBuilder::new()
            .issuer("some-other-issuer")
            .single_audience("test-relay")
            .subject("test-subject")
            .expires_in(3600)
            .moqt_scope(scope)
            .build()
            .expect("build token");
        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());

        let decision = hook
            .on_setup(&session(), &[auth_token(encoded)])
            .await
            .unwrap();
        assert!(matches!(
            decision.deny_reason(),
            Some(DenyReason::IssuerUnknown)
        ));
    }

    #[tokio::test]
    async fn denies_a_token_for_another_audience() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let scope = MoqtScopeBuilder::new()
            .publisher()
            .namespace_prefix(b"sports")
            .track_prefix(b"")
            .build();
        let token = CatTokenBuilder::new()
            .issuer("test-issuer")
            .single_audience("some-other-relay")
            .subject("test-subject")
            .expires_in(3600)
            .moqt_scope(scope)
            .build()
            .expect("build token");
        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());

        let decision = hook
            .on_setup(&session(), &[auth_token(encoded)])
            .await
            .unwrap();
        assert!(matches!(
            decision.deny_reason(),
            Some(DenyReason::TokenInvalid)
        ));
    }

    #[tokio::test]
    async fn denies_a_token_that_does_not_grant_client_setup() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        // Publisher rights but no CLIENT_SETUP scope.
        let scope = MoqtScopeBuilder::new()
            .publisher()
            .namespace_prefix(b"sports")
            .track_prefix(b"")
            .build();
        let token = CatTokenBuilder::new()
            .issuer("test-issuer")
            .single_audience("test-relay")
            .subject("test-subject")
            .expires_in(3600)
            .moqt_scope(scope)
            .build()
            .expect("build token");
        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());

        let decision = hook
            .on_setup(&session(), &[auth_token(encoded)])
            .await
            .unwrap();
        assert!(matches!(
            decision.deny_reason(),
            Some(DenyReason::ScopeMismatch)
        ));
    }

    #[tokio::test]
    async fn denies_a_token_with_no_moqt_claim() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        // "The default for all actions is 'Blocked'": a token carrying no
        // `moqt` claim authorizes nothing.
        let token = CatTokenBuilder::new()
            .issuer("test-issuer")
            .single_audience("test-relay")
            .subject("test-subject")
            .expires_in(3600)
            .build()
            .expect("build token");
        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());

        let decision = hook
            .on_setup(&session(), &[auth_token(encoded)])
            .await
            .unwrap();
        assert!(matches!(
            decision.deny_reason(),
            Some(DenyReason::ScopeMismatch)
        ));
    }

    #[tokio::test]
    async fn denies_a_token_requiring_revalidation() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        // The relay does not revalidate mid-session, so c4m requires it to
        // reject any token carrying `moqt-reval`.
        let scope = MoqtScopeBuilder::new()
            .publisher()
            .namespace_prefix(b"sports")
            .track_prefix(b"")
            .build();
        let setup = MoqtScopeBuilder::new()
            .action(MoqtAction::ClientSetup)
            .build();
        let token = CatTokenBuilder::new()
            .issuer("test-issuer")
            .single_audience("test-relay")
            .subject("test-subject")
            .expires_in(3600)
            .moqt_scope(scope)
            .moqt_scope(setup)
            .moqt_reval(30.0)
            .build()
            .expect("build token");
        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());

        let decision = hook
            .on_setup(&session(), &[auth_token(encoded)])
            .await
            .unwrap();
        assert!(
            !decision.is_allowed(),
            "revalidation-bound token must be denied"
        );
    }

    #[tokio::test]
    async fn denies_garbage_token_values() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        for value in [
            Bytes::from_static(b""),
            Bytes::from_static(b"not-a-token"),
            Bytes::from_static(b"a.b"),
            Bytes::from_static(b"a.b.c.d"),
            Bytes::from_static(b"!!!.###.$$$"),
            Bytes::from_static(&[0xff, 0xfe, 0xfd]),
        ] {
            let decision = hook
                .on_setup(&session(), &[auth_token(value.clone())])
                .await
                .unwrap();
            assert!(!decision.is_allowed(), "{value:?} must not be allowed");
        }
    }

    // ------------------------------------------------------------------
    // Key selection
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn verifies_against_any_key_in_the_set() {
        // A token signed by the last configured key must still verify, which is
        // what makes a rotation overlap work.
        for signing_index in 0..MAX_SCOPE_AUTH_KEYS {
            let generated: Vec<TestKey> =
                (0..MAX_SCOPE_AUTH_KEYS).map(|_| generate_key()).collect();
            let keys = generated
                .iter()
                .map(|key| AuthPublicKey::es256(key.pem.clone()))
                .collect();
            let hook = hook(keys);

            let token = publisher_token(&generated[signing_index].signer, &[b"sports"]);
            let decision = hook
                .on_setup(&session(), &[auth_token(token)])
                .await
                .unwrap();

            assert!(
                decision.is_allowed(),
                "token signed by key {signing_index} should verify"
            );
        }
    }

    #[tokio::test]
    async fn denies_when_no_key_in_the_set_matches() {
        let generated: Vec<TestKey> = (0..MAX_SCOPE_AUTH_KEYS).map(|_| generate_key()).collect();
        let keys = generated
            .iter()
            .map(|key| AuthPublicKey::es256(key.pem.clone()))
            .collect();
        let hook = hook(keys);

        let attacker = generate_key();
        let token = publisher_token(&attacker.signer, &[b"sports"]);

        let decision = hook
            .on_setup(&session(), &[auth_token(token)])
            .await
            .unwrap();
        assert!(!decision.is_allowed());
    }

    #[tokio::test]
    async fn selects_the_key_named_by_the_token_kid() {
        let old = generate_key();
        let new = generate_key();
        let hook = hook(vec![
            AuthPublicKey::es256(old.pem).with_kid("old"),
            AuthPublicKey::es256(new.pem).with_kid("new"),
        ]);

        // Signed by "new" and labelled as such.
        let token = mint(&new.signer, &[b"sports"], true, Some("new"), 3600);
        assert!(hook
            .on_setup(&session(), &[auth_token(token)])
            .await
            .unwrap()
            .is_allowed());

        // Signed by "old" and labelled as such.
        let token = mint(&old.signer, &[b"sports"], true, Some("old"), 3600);
        assert!(hook
            .on_setup(&session(), &[auth_token(token)])
            .await
            .unwrap()
            .is_allowed());
    }

    #[tokio::test]
    async fn a_kid_less_key_can_verify_a_token_bearing_a_kid() {
        // An operator who configures keys without identifiers should not have
        // clients rejected merely for labelling their tokens.
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let token = mint(&key.signer, &[b"sports"], true, Some("whatever"), 3600);
        assert!(hook
            .on_setup(&session(), &[auth_token(token)])
            .await
            .unwrap()
            .is_allowed());
    }

    /// `kid` reorders the key list so the named key is tried first; it never
    /// removes a key, so a mismatched label cannot deny a valid signature.
    #[test]
    fn a_kid_reorders_but_never_excludes_keys() {
        let hook = hook(vec![
            AuthPublicKey::es256(generate_key().pem),
            AuthPublicKey::es256(generate_key().pem).with_kid("a"),
            AuthPublicKey::es256(generate_key().pem).with_kid("b"),
        ]);

        let kids = |kid: Option<&str>| -> Vec<Option<String>> {
            hook.candidate_keys(kid)
                .into_iter()
                .map(|key| key.kid.clone())
                .collect()
        };

        // The named key first, then the others, with none dropped.
        assert_eq!(
            kids(Some("b")),
            vec![Some("b".to_string()), None, Some("a".to_string())]
        );

        // A name matching nothing still yields every key, in configured order.
        assert_eq!(
            kids(Some("absent")),
            vec![None, Some("a".to_string()), Some("b".to_string())]
        );

        // No name means no reordering.
        assert_eq!(
            kids(None),
            vec![None, Some("a".to_string()), Some("b".to_string())]
        );

        for kid in [Some("a"), Some("b"), Some("absent"), None] {
            assert_eq!(
                kids(kid).len(),
                hook.keys.len(),
                "every key must remain a candidate for kid {kid:?}"
            );
        }
    }

    #[test]
    fn header_kid_reads_the_cose_label() {
        let key = generate_key();

        let labelled = mint(&key.signer, &[b"sports"], true, Some("rotating"), 3600);
        assert_eq!(header_kid(&labelled), Some("rotating".to_string()));

        let unlabelled = publisher_token(&key.signer, &[b"sports"]);
        assert_eq!(header_kid(&unlabelled), None);
    }

    /// An unreadable header yields no hint. It must never produce a verdict:
    /// the header is attacker-controlled, and the signature is what decides.
    #[test]
    fn header_kid_degrades_to_no_hint_on_malformed_input() {
        for value in [
            &b""[..],
            &b"only-one-part"[..],
            &b"two.parts"[..],
            &b"four.parts.are.wrong"[..],
            &b".empty.header"[..],
            &b"!!!not-base64!!!.b.c"[..],
            &[0xff, 0xfe, 0xfd][..],
        ] {
            assert_eq!(
                header_kid(value),
                None,
                "{:?} should yield no hint",
                String::from_utf8_lossy(value)
            );
        }
    }

    #[tokio::test]
    async fn a_token_with_an_unsupported_critical_header_is_refused() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem.clone())]);
        let encoded = encode_with_extra_headers(
            &base_token(),
            &key.signer,
            vec![
                (
                    ciborium::Value::Integer(2.into()),
                    ciborium::Value::Array(vec![ciborium::Value::Integer(42.into())]),
                ),
                (
                    ciborium::Value::Integer(42.into()),
                    ciborium::Value::Bool(true),
                ),
            ],
        );

        let decision = hook
            .on_setup(&session(), &[auth_token(Bytes::from(encoded))])
            .await
            .unwrap();
        assert!(!decision.is_allowed());
    }

    /// RFC 8152 §3.1 types `kid` as `bstr`, so real issuers emit thumbprints
    /// that are not text, and COSE permits other shapes entirely. None of
    /// those may cause a rejection — a token the relay holds the key for must
    /// still be accepted.
    #[tokio::test]
    async fn an_unmatchable_kid_does_not_prevent_verification() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem.clone())]);

        let token = CatTokenBuilder::new()
            .issuer("test-issuer")
            .single_audience("test-relay")
            .subject("test-subject")
            .expires_in(3600)
            .moqt_scope(
                MoqtScopeBuilder::new()
                    .publisher()
                    .namespace_prefix(b"sports")
                    .track_prefix(b"")
                    .build(),
            )
            .moqt_scope(
                MoqtScopeBuilder::new()
                    .action(MoqtAction::ClientSetup)
                    .build(),
            )
            .build()
            .expect("build token");

        // A binary thumbprint, an integer label, and an outright absent header
        // map: each is unusable as a hint, none is grounds for denial.
        for kid in [
            ciborium::Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            ciborium::Value::Integer(7.into()),
            ciborium::Value::Null,
        ] {
            let encoded = encode_with_raw_kid(&token, &key.signer, kid.clone());
            let decision = hook
                .on_setup(&session(), &[auth_token(Bytes::from(encoded))])
                .await
                .unwrap();

            assert!(
                decision.is_allowed(),
                "a token with kid {kid:?} must still verify against the configured key"
            );
        }
    }

    /// A `kid` naming no configured key is a hint that matched nothing, not a
    /// rejection: the signature still decides. Otherwise a relabelled issuer
    /// or a typo in one configured identifier is an outage.
    #[tokio::test]
    async fn a_kid_naming_no_configured_key_still_verifies() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem).with_kid("known")]);

        let token = mint(&key.signer, &[b"sports"], true, Some("unknown"), 3600);

        assert!(hook
            .on_setup(&session(), &[auth_token(token)])
            .await
            .unwrap()
            .is_allowed());
    }

    // ------------------------------------------------------------------
    // Per-request authorization
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn publisher_token_allows_publish_and_denies_subscribe() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);
        let principal = principal_for(&hook, publisher_token(&key.signer, &[b"sports"])).await;

        let namespace = TrackNamespace::from_utf8_path("sports/football");
        let track = TrackName::from("video");

        assert!(decide(
            &hook,
            &principal,
            AuthzOperation::PublishNamespace {
                namespace: &namespace
            }
        )
        .await
        .is_allowed());

        assert!(decide(
            &hook,
            &principal,
            AuthzOperation::Publish {
                namespace: &namespace,
                track: &track
            }
        )
        .await
        .is_allowed());

        assert!(!decide(
            &hook,
            &principal,
            AuthzOperation::Subscribe {
                namespace: &namespace,
                track: &track
            }
        )
        .await
        .is_allowed());
    }

    #[tokio::test]
    async fn subscriber_token_allows_subscribe_and_denies_publish() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);
        let principal = principal_for(&hook, subscriber_token(&key.signer, &[b"sports"])).await;

        let namespace = TrackNamespace::from_utf8_path("sports/football");
        let track = TrackName::from("video");

        assert!(decide(
            &hook,
            &principal,
            AuthzOperation::Subscribe {
                namespace: &namespace,
                track: &track
            }
        )
        .await
        .is_allowed());

        assert!(!decide(
            &hook,
            &principal,
            AuthzOperation::PublishNamespace {
                namespace: &namespace
            }
        )
        .await
        .is_allowed());
    }

    #[tokio::test]
    async fn denies_operations_outside_the_granted_namespace() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);
        let principal = principal_for(&hook, publisher_token(&key.signer, &[b"sports"])).await;

        let other = TrackNamespace::from_utf8_path("news/politics");

        let decision = decide(
            &hook,
            &principal,
            AuthzOperation::PublishNamespace { namespace: &other },
        )
        .await;

        assert!(matches!(
            decision.deny_reason(),
            Some(DenyReason::ScopeMismatch)
        ));
    }

    /// c4m matches the track predicate "against the Track Name", and
    /// PUBLISH_NAMESPACE / SUBSCRIBE_NAMESPACE carry none. Applying it anyway
    /// tests it against an empty name, which fails for any non-empty prefix —
    /// so the canonical publisher scope would be granted PUBLISH while being
    /// denied the PUBLISH_NAMESPACE that has to precede it.
    #[tokio::test]
    async fn namespace_level_actions_ignore_the_track_predicate() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let principal = principal_for(&hook, track_scoped_token(&key.signer, b"live/")).await;

        let namespace = TrackNamespace::from_utf8_path("sports/football");
        let prefix = TrackNamespacePrefix::from_utf8_path("sports");

        assert!(
            decide(
                &hook,
                &principal,
                AuthzOperation::PublishNamespace {
                    namespace: &namespace
                }
            )
            .await
            .is_allowed(),
            "a track-scoped token must still be able to announce its namespace"
        );

        assert!(
            decide(
                &hook,
                &principal,
                AuthzOperation::SubscribeNamespace { prefix: &prefix }
            )
            .await
            .is_allowed(),
            "a track-scoped token must still be able to subscribe to the prefix"
        );

        // The predicate still applies where a Track Name exists.
        assert!(
            decide(
                &hook,
                &principal,
                AuthzOperation::Publish {
                    namespace: &namespace,
                    track: &TrackName::from("live/video"),
                }
            )
            .await
            .is_allowed(),
            "a matching track name is permitted"
        );

        assert!(
            !decide(
                &hook,
                &principal,
                AuthzOperation::Publish {
                    namespace: &namespace,
                    track: &TrackName::from("premium/4k"),
                }
            )
            .await
            .is_allowed(),
            "a track name outside the prefix must be denied"
        );

        assert!(
            !decide(
                &hook,
                &principal,
                AuthzOperation::Subscribe {
                    namespace: &namespace,
                    track: &TrackName::from("premium/4k"),
                }
            )
            .await
            .is_allowed(),
            "the track predicate must still gate SUBSCRIBE"
        );
    }

    /// A CLIENT_SETUP scope sharing a token with a track predicate must not be
    /// blocked by it: CLIENT_SETUP carries no Track Name either.
    #[tokio::test]
    async fn client_setup_ignores_the_track_predicate() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let token = CatTokenBuilder::new()
            .issuer("test-issuer")
            .single_audience("test-relay")
            .subject("test-subject")
            .expires_in(3600)
            // One scope, granting setup and publish, narrowed by track prefix.
            .moqt_scope(
                MoqtScopeBuilder::new()
                    .action(MoqtAction::ClientSetup)
                    .action(MoqtAction::PublishNamespace)
                    .action(MoqtAction::Publish)
                    .namespace_prefix(b"sports")
                    .track_prefix(b"live/")
                    .build(),
            )
            .build()
            .expect("build token");
        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());

        assert!(hook
            .on_setup(&session(), &[auth_token(encoded)])
            .await
            .unwrap()
            .is_allowed());
    }

    /// A token carrying a restriction the relay does not evaluate must be
    /// refused, not honoured with the restriction quietly dropped.
    #[tokio::test]
    async fn tokens_carrying_unenforceable_restrictions_are_refused() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let base = || {
            CatTokenBuilder::new()
                .issuer("test-issuer")
                .single_audience("test-relay")
                .subject("test-subject")
                .expires_in(3600)
                .moqt_scope(
                    MoqtScopeBuilder::new()
                        .publisher()
                        .namespace_prefix(b"sports")
                        .track_prefix(b"")
                        .build(),
                )
                .moqt_scope(
                    MoqtScopeBuilder::new()
                        .action(MoqtAction::ClientSetup)
                        .build(),
                )
        };

        // Single-use, and geographically pinned: both are constraints this
        // relay never evaluates, so both must deny.
        for token in [
            base()
                .replay_protection(cat_token::ReplayProtection::Prohibited)
                .build()
                .expect("build token"),
            base().geohash("9q8yy").build().expect("build token"),
        ] {
            let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());
            let decision = hook
                .on_setup(&session(), &[auth_token(encoded)])
                .await
                .unwrap();

            assert!(
                !decision.is_allowed(),
                "a token with an unenforceable restriction must be refused"
            );
        }

        // The same token without those claims is fine, so the refusal is
        // attributable to the claim and not to the fixture.
        let token = base().build().expect("build token");
        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());
        assert!(hook
            .on_setup(&session(), &[auth_token(encoded)])
            .await
            .unwrap()
            .is_allowed());
    }

    /// SUBSCRIBE_NAMESPACE grants discovery of a prefix; it does not imply the
    /// right to receive the tracks under it.
    ///
    /// The relay's SUBSCRIBE_NAMESPACE handler can push PUBLISH and stream
    /// objects for matching tracks, so it authorizes each one as a SUBSCRIBE
    /// before doing so (`Producer::publish_track_for_namespace`). This pins
    /// the policy that fix depends on: the two actions are distinct, and a
    /// track predicate still applies to the per-track decision.
    #[tokio::test]
    async fn subscribe_namespace_does_not_imply_subscribe() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        // Discovery of the prefix only: no Subscribe action at all.
        let token = CatTokenBuilder::new()
            .issuer("test-issuer")
            .single_audience("test-relay")
            .subject("test-subject")
            .expires_in(3600)
            .moqt_scope(
                MoqtScopeBuilder::new()
                    .action(MoqtAction::ClientSetup)
                    .action(MoqtAction::SubscribeNamespace)
                    .namespace_prefix(b"sports")
                    .build(),
            )
            .build()
            .expect("build token");
        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());
        let principal = principal_for(&hook, encoded).await;

        let prefix = TrackNamespacePrefix::from_utf8_path("sports");
        let namespace = TrackNamespace::from_utf8_path("sports/football");

        assert!(
            decide(
                &hook,
                &principal,
                AuthzOperation::SubscribeNamespace { prefix: &prefix }
            )
            .await
            .is_allowed(),
            "the prefix subscription itself is granted"
        );

        assert!(
            !decide(
                &hook,
                &principal,
                AuthzOperation::Subscribe {
                    namespace: &namespace,
                    track: &TrackName::from("video"),
                }
            )
            .await
            .is_allowed(),
            "delivery of a track under that prefix is not"
        );

        // The narrower, likelier shape: discovery of the whole prefix, but
        // delivery only of tracks whose name matches.
        let restricted = principal_for(&hook, track_scoped_token(&key.signer, b"preview-")).await;

        assert!(decide(
            &hook,
            &restricted,
            AuthzOperation::SubscribeNamespace { prefix: &prefix }
        )
        .await
        .is_allowed());
        assert!(decide(
            &hook,
            &restricted,
            AuthzOperation::Subscribe {
                namespace: &namespace,
                track: &TrackName::from("preview-clip"),
            }
        )
        .await
        .is_allowed());
        assert!(
            !decide(
                &hook,
                &restricted,
                AuthzOperation::Subscribe {
                    namespace: &namespace,
                    track: &TrackName::from("premium-4k"),
                }
            )
            .await
            .is_allowed(),
            "a track outside the grant must not be deliverable via the prefix"
        );
    }

    /// c4m §3.1.1 requires a relay to reject a request whose DPoP proof or key
    /// binding fails. This relay performs no DPoP validation, so a
    /// sender-constrained token must be refused rather than silently accepted
    /// with bearer semantics.
    #[tokio::test]
    async fn a_sender_constrained_token_is_refused() {
        use cat_token::ConfirmationClaim;

        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let mut token = base_token();
        token.dpop.cnf = Some(ConfirmationClaim {
            jkt: vec![0xab; 32],
            ckt: None,
        });

        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());
        let decision = hook
            .on_setup(&session(), &[auth_token(encoded)])
            .await
            .unwrap();

        assert!(
            !decision.is_allowed(),
            "a DPoP-bound token must not be honoured as a bearer token"
        );
    }

    /// The refusal must not depend on the CBOR shape a claim happens to use.
    ///
    /// `cat-token`'s decoder recognizes only the shapes it emits and silently
    /// drops the rest, so checking the decoded token would miss, for instance,
    /// a `cnf` in RFC 8747's COSE_Key form — turning a proof-of-possession
    /// credential into a bearer credential.
    #[tokio::test]
    async fn restrictive_claims_are_refused_whatever_shape_they_take() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        // (claim key, a value `cat-token`'s decoder does *not* recognize)
        let smuggled = [
            // cnf as an RFC 8747 COSE_Key rather than {3: bstr}.
            (
                8,
                ciborium::Value::Map(vec![(
                    ciborium::Value::Integer(1.into()),
                    ciborium::Value::Integer(2.into()),
                )]),
            ),
            // cnf carrying jkt as text rather than bytes.
            (
                8,
                ciborium::Value::Map(vec![(
                    ciborium::Value::Integer(3.into()),
                    ciborium::Value::Text("thumbprint".to_string()),
                )]),
            ),
            (8, ciborium::Value::Bytes(vec![0x01, 0x02])),
            // catreplay as an integer rather than text.
            (308, ciborium::Value::Integer(1.into())),
            // catu as text rather than an integer.
            (312, ciborium::Value::Text("3".to_string())),
            (319, ciborium::Value::Bytes(vec![0xaa])),
            (321, ciborium::Value::Bool(true)),
            // A composite claim, which the decoder drops entirely.
            (326, ciborium::Value::Array(vec![])),
            // A claim from no registry at all.
            (9999, ciborium::Value::Integer(1.into())),
        ];

        for (claim_key, value) in smuggled {
            let encoded = mint_with_extra_claim(&key.signer, claim_key, value.clone());

            // The signature and the standard claims are all valid; only the
            // smuggled claim should stand in the way.
            let decision = hook
                .on_setup(&session(), &[auth_token(Bytes::from(encoded))])
                .await
                .unwrap();

            assert!(
                !decision.is_allowed(),
                "claim {claim_key} encoded as {value:?} must be refused"
            );
        }
    }

    /// Permitting a claim because it is enforced is only sound if the decoder
    /// understood it.
    ///
    /// RFC 8392 types `nbf` as "integer or floating-point number" and
    /// `cat-token` accepts only the integer form, so a conformant issuer's
    /// float `nbf` would otherwise be dropped and the not-before silently
    /// ignored — a post-dated token usable before its window opens.
    #[tokio::test]
    async fn a_load_bearing_claim_in_an_unsupported_encoding_is_refused() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let future = (now_unix() + 3600) as f64;
        // Claims the base token does not already carry, so these are genuinely
        // "present on the wire, dropped by the decoder" rather than duplicates.
        let unsupported = [
            // nbf: RFC 8392 permits a float NumericDate.
            (5, ciborium::Value::Float(future)),
            (5, ciborium::Value::Text(future.to_string())),
            // moqt-reval: c4m requires refusing a token that demands
            // revalidation, which cannot happen if the claim is dropped.
            (328, ciborium::Value::Text("300".to_string())),
            (328, ciborium::Value::Bool(true)),
            (328, ciborium::Value::Integer(300.into())),
        ];

        for (claim_key, value) in unsupported {
            let encoded = mint_with_extra_claim(&key.signer, claim_key, value.clone());
            let decision = hook
                .on_setup(&session(), &[auth_token(Bytes::from(encoded))])
                .await
                .unwrap();

            assert!(
                !decision.is_allowed(),
                "claim {claim_key} as {value:?} was dropped by the decoder and \
                 must not be treated as absent"
            );
        }
    }

    /// A repeated claim key leaves the decoder to pick a winner, so a second
    /// copy in a shape it prefers could decide what the relay enforces.
    #[tokio::test]
    async fn a_duplicated_claim_is_refused() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        // Each of these already appears in the base token, so splicing another
        // in produces a duplicate.
        for (claim_key, value) in [
            (1, ciborium::Value::Text("attacker".to_string())),
            (3, ciborium::Value::Integer(1.into())),
            (4, ciborium::Value::Integer((now_unix() + 60).into())),
            (327, ciborium::Value::Array(vec![])),
        ] {
            let encoded = mint_with_extra_claim(&key.signer, claim_key, value.clone());
            let decision = hook
                .on_setup(&session(), &[auth_token(Bytes::from(encoded))])
                .await
                .unwrap();

            assert!(
                !decision.is_allowed(),
                "a duplicated claim {claim_key} must be refused"
            );
        }
    }

    /// A post-dated token must not be usable before its window opens,
    /// whichever encoding its `nbf` uses.
    #[tokio::test]
    async fn a_not_yet_valid_token_is_refused() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        // The integer form the library does understand.
        let mut token = base_token();
        token.core.nbf = Some(now_unix() + 3600);
        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());
        assert!(!hook
            .on_setup(&session(), &[auth_token(encoded)])
            .await
            .unwrap()
            .is_allowed());

        // And the float form it does not.
        let encoded = mint_with_extra_claim(
            &key.signer,
            5,
            ciborium::Value::Float((now_unix() + 3600) as f64),
        );
        assert!(!hook
            .on_setup(&session(), &[auth_token(Bytes::from(encoded))])
            .await
            .unwrap()
            .is_allowed());
    }

    /// The allowlist must not refuse the claims a normal token carries.
    #[tokio::test]
    async fn permitted_claims_are_accepted() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let mut token = base_token();
        token.core.cti = Some(b"token-id".to_vec());
        token.informational.iat = Some(now_unix());
        token.cat.catv = Some(1);
        token.request.catr = Some(
            cat_token::CatRenewal::automatic()
                .with_expadd(600.0)
                .expect("valid renewal"),
        );
        token.informational.catifdata = Some(vec!["data".to_string()]);

        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());
        assert!(hook
            .on_setup(&session(), &[auth_token(encoded)])
            .await
            .unwrap()
            .is_allowed());
    }

    /// An extreme not-before is outside the relay's supported token horizon.
    #[tokio::test]
    async fn an_implausible_not_before_is_refused() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        for nbf in [
            i64::MIN,
            i64::MIN / 2,
            now_unix() - 10 * MAX_TOKEN_LIFETIME_SECS,
        ] {
            let mut token = CatTokenBuilder::new()
                .issuer("test-issuer")
                .single_audience("test-relay")
                .subject("test-subject")
                .expires_in(3600)
                .moqt_scope(
                    MoqtScopeBuilder::new()
                        .publisher()
                        .namespace_prefix(b"sports")
                        .track_prefix(b"")
                        .build(),
                )
                .moqt_scope(
                    MoqtScopeBuilder::new()
                        .action(MoqtAction::ClientSetup)
                        .build(),
                )
                .build()
                .expect("build token");
            token.core.nbf = Some(nbf);

            let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());
            // Must deny, and in particular must not panic on the way.
            let decision = hook
                .on_setup(&session(), &[auth_token(encoded)])
                .await
                .unwrap();
            assert!(!decision.is_allowed(), "nbf {nbf} must be refused");
        }
    }

    /// Admission cost must not scale with what the peer chooses to send.
    /// Without a budget, `MAX_SETUP_TOKENS` tokens against a full key set would
    /// buy 40 signature verifications per connection attempt.
    #[tokio::test]
    async fn signature_verifications_are_budgeted_per_session() {
        let generated: Vec<TestKey> = (0..MAX_SCOPE_AUTH_KEYS).map(|_| generate_key()).collect();
        let keys = generated
            .iter()
            .map(|key| AuthPublicKey::es256(key.pem.clone()))
            .collect();
        let hook = hook(keys);

        // Every token is signed by an unconfigured key, so each exhausts the
        // whole candidate list before failing.
        let attacker = generate_key();
        let mut tokens: Vec<AuthToken> = (0..crate::auth::MAX_SETUP_TOKENS)
            .map(|_| auth_token(publisher_token(&attacker.signer, &[b"sports"])))
            .collect();
        tokens.push(auth_token(publisher_token(
            &generated[0].signer,
            &[b"sports"],
        )));

        let decision = hook.on_setup(&session(), &tokens).await.unwrap();
        assert!(
            !decision.is_allowed(),
            "a valid token after budget exhaustion must be unreachable"
        );
    }

    /// The budget must not reject honest clients. Two cases have to fit: one
    /// token against a full key set mid-rotation, and a client presenting a
    /// token for another relay before the one that works here.
    #[tokio::test]
    async fn the_budget_admits_the_honest_worst_case() {
        let generated: Vec<TestKey> = (0..MAX_SCOPE_AUTH_KEYS).map(|_| generate_key()).collect();
        let keys: Vec<AuthPublicKey> = generated
            .iter()
            .map(|key| AuthPublicKey::es256(key.pem.clone()))
            .collect();
        let hook = hook(keys);

        // Signed by the last key, so every earlier one is tried and fails.
        let token = publisher_token(&generated[MAX_SCOPE_AUTH_KEYS - 1].signer, &[b"sports"]);
        assert!(
            hook.on_setup(&session(), &[auth_token(token)])
                .await
                .unwrap()
                .is_allowed(),
            "one token against a full key set must fit the budget"
        );

        // A token this relay cannot verify, followed by one it can: the first
        // exhausts all five keys before the second is reached.
        let elsewhere = generate_key();
        let tokens = vec![
            auth_token(publisher_token(&elsewhere.signer, &[b"sports"])),
            auth_token(publisher_token(
                &generated[MAX_SCOPE_AUTH_KEYS - 1].signer,
                &[b"sports"],
            )),
        ];
        assert!(
            hook.on_setup(&session(), &tokens)
                .await
                .unwrap()
                .is_allowed(),
            "two tokens against a full key set must fit the budget"
        );
    }

    /// §9.3.1.5 permits more than one token. A token that does not satisfy
    /// this scope must not mask a later one that does.
    #[tokio::test]
    async fn a_later_token_is_tried_when_an_earlier_one_fails() {
        let configured = generate_key();
        let other = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(configured.pem)]);

        let unusable = auth_token(publisher_token(&other.signer, &[b"sports"]));
        let usable = auth_token(publisher_token(&configured.signer, &[b"sports"]));

        let decision = hook
            .on_setup(&session(), &[unusable, usable])
            .await
            .unwrap();

        assert!(
            decision.is_allowed(),
            "the second token should have been tried"
        );
    }

    #[tokio::test]
    async fn subscribe_namespace_is_authorized_against_the_prefix() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);
        let principal = principal_for(&hook, subscriber_token(&key.signer, &[b"sports"])).await;

        let granted = TrackNamespacePrefix::from_utf8_path("sports");
        assert!(decide(
            &hook,
            &principal,
            AuthzOperation::SubscribeNamespace { prefix: &granted }
        )
        .await
        .is_allowed());

        let other = TrackNamespacePrefix::from_utf8_path("news");
        assert!(!decide(
            &hook,
            &principal,
            AuthzOperation::SubscribeNamespace { prefix: &other }
        )
        .await
        .is_allowed());
    }

    /// Namespace announcements authorize each concrete namespace as the prefix
    /// naming exactly it, so a namespace *below* the granted prefix must still
    /// be announceable. Getting this wrong would silently break discovery for
    /// every ordinary prefix subscription.
    #[tokio::test]
    async fn a_namespace_below_the_granted_prefix_is_announceable() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);
        let principal = principal_for(&hook, subscriber_token(&key.signer, &[b"sports"])).await;

        for path in ["sports", "sports/football", "sports/football/match-42"] {
            let namespace = TrackNamespacePrefix::from_utf8_path(path);
            assert!(
                decide(
                    &hook,
                    &principal,
                    AuthzOperation::SubscribeNamespace { prefix: &namespace }
                )
                .await
                .is_allowed(),
                "{path} is under the granted prefix and must be announceable"
            );
        }

        let outside = TrackNamespacePrefix::from_utf8_path("news/politics");
        assert!(
            !decide(
                &hook,
                &principal,
                AuthzOperation::SubscribeNamespace { prefix: &outside }
            )
            .await
            .is_allowed(),
            "a namespace outside the grant must not be announced"
        );
    }

    /// The case the announcement filter exists for: a `nil` terminator limits
    /// the grant to an exact namespace depth, so deeper namespaces must not be
    /// disclosed even though the prefix subscription itself is allowed.
    #[tokio::test]
    async fn a_nil_terminated_scope_hides_deeper_namespaces() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let token = CatTokenBuilder::new()
            .issuer("test-issuer")
            .single_audience("test-relay")
            .subject("test-subject")
            .expires_in(3600)
            .moqt_scope(
                MoqtScopeBuilder::new()
                    .subscriber()
                    .action(MoqtAction::ClientSetup)
                    .namespace_exact(b"sports")
                    .namespace_nil()
                    .build(),
            )
            .build()
            .expect("build token");
        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());
        let principal = principal_for(&hook, encoded).await;

        let granted = TrackNamespacePrefix::from_utf8_path("sports");
        assert!(
            decide(
                &hook,
                &principal,
                AuthzOperation::SubscribeNamespace { prefix: &granted }
            )
            .await
            .is_allowed(),
            "the exact namespace is granted"
        );

        let deeper = TrackNamespacePrefix::from_utf8_path("sports/football");
        assert!(
            !decide(
                &hook,
                &principal,
                AuthzOperation::SubscribeNamespace { prefix: &deeper }
            )
            .await
            .is_allowed(),
            "a nil terminator must hide deeper namespaces from announcement"
        );
    }

    /// A session must stop being authorized once its token expires, even
    /// though no fresh signature check happens per request.
    ///
    /// Driven by rewinding the principal's recorded expiry rather than by
    /// sleeping: `now_unix` reads the system clock, which a paused tokio
    /// runtime does not control, so a sleep-based version would have to
    /// outwait real seconds and would still sit on a truncation boundary.
    #[tokio::test]
    async fn a_token_expiring_mid_session_stops_authorizing_requests() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let live = principal_for(&hook, publisher_token(&key.signer, &[b"sports"])).await;
        let namespace = TrackNamespace::from_utf8_path("sports/football");

        // While the token is live the operation is permitted.
        assert!(decide(
            &hook,
            &live,
            AuthzOperation::PublishNamespace {
                namespace: &namespace
            }
        )
        .await
        .is_allowed());

        // Same token and claims, but expiry now comfortably in the past.
        let claims = live.claims::<CatPrincipal>().expect("cat principal");
        let expired = rewind_expiry(claims, now_unix() - claims.clock_skew - 60);

        let decision = decide(
            &hook,
            &expired,
            AuthzOperation::PublishNamespace {
                namespace: &namespace,
            },
        )
        .await;
        assert!(
            matches!(decision.deny_reason(), Some(DenyReason::TokenExpired)),
            "expected expiry, got {:?}",
            decision.deny_reason()
        );
    }

    /// Expiry is evaluated against the skew tolerance, not the raw timestamp.
    #[tokio::test]
    async fn expiry_respects_the_clock_skew_tolerance() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let live = principal_for(&hook, publisher_token(&key.signer, &[b"sports"])).await;
        let claims = live.claims::<CatPrincipal>().expect("cat principal");
        let namespace = TrackNamespace::from_utf8_path("sports/football");

        // Expired a moment ago, but still inside the tolerance window.
        let within_skew = rewind_expiry(claims, now_unix() - claims.clock_skew / 2);

        assert!(
            decide(
                &hook,
                &within_skew,
                AuthzOperation::PublishNamespace {
                    namespace: &namespace
                }
            )
            .await
            .is_allowed(),
            "a token inside the skew window must still authorize"
        );
    }

    /// A token with no `exp` is unrevokable, so it is refused outright rather
    /// than becoming a permanent credential.
    #[tokio::test]
    async fn a_token_without_an_expiry_is_refused() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let token = CatTokenBuilder::new()
            .issuer("test-issuer")
            .single_audience("test-relay")
            .subject("test-subject")
            .moqt_scope(
                MoqtScopeBuilder::new()
                    .publisher()
                    .namespace_prefix(b"sports")
                    .track_prefix(b"")
                    .build(),
            )
            .moqt_scope(
                MoqtScopeBuilder::new()
                    .action(MoqtAction::ClientSetup)
                    .build(),
            )
            .build()
            .expect("build token");
        assert!(token.core.exp.is_none(), "fixture must omit exp");

        let encoded = Bytes::from(encode_token(&token, &key.signer).unwrap());
        let decision = hook
            .on_setup(&session(), &[auth_token(encoded)])
            .await
            .unwrap();

        assert!(matches!(
            decision.deny_reason(),
            Some(DenyReason::TokenInvalid)
        ));
    }

    /// An expiry beyond the relay's maximum lifetime is refused: it is the
    /// unbounded credential the `exp` requirement exists to prevent, and it is
    /// not a bounded bearer credential.
    #[tokio::test]
    async fn an_implausibly_distant_expiry_is_refused() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        // Bounded above by what chrono can represent, since the fixture builds
        // the expiry as an offset from now.
        for expires_in in [MAX_TOKEN_LIFETIME_SECS + 3600, 100 * 365 * 24 * 60 * 60] {
            let token = mint(&key.signer, &[b"sports"], true, None, expires_in);
            let decision = hook
                .on_setup(&session(), &[auth_token(token)])
                .await
                .unwrap();

            assert!(
                !decision.is_allowed(),
                "expiry {expires_in}s away must be refused"
            );
        }

        // Just inside the limit is still accepted.
        let token = mint(
            &key.signer,
            &[b"sports"],
            true,
            None,
            MAX_TOKEN_LIFETIME_SECS - 60,
        );
        assert!(hook
            .on_setup(&session(), &[auth_token(token)])
            .await
            .unwrap()
            .is_allowed());
    }

    /// A scope may not ask for a skew so large that expiry stops meaning
    /// anything, nor one that overflows the library's `exp + tolerance`.
    #[test]
    fn clock_skew_is_clamped() {
        let key = generate_key();
        let config = ScopeAuthConfig::new(vec![AuthPublicKey::es256(key.pem)])
            .with_clock_skew(Duration::from_secs(u64::MAX));

        let hook = CatAuthHook::new(&config, None).expect("hook builds");
        assert_eq!(hook.clock_skew, MAX_CLOCK_SKEW_SECS);
    }

    #[tokio::test]
    async fn a_token_within_its_lifetime_keeps_authorizing() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let live = principal_for(&hook, publisher_token(&key.signer, &[b"sports"])).await;
        let namespace = TrackNamespace::from_utf8_path("sports/football");

        assert!(decide(
            &hook,
            &live,
            AuthzOperation::PublishNamespace {
                namespace: &namespace
            }
        )
        .await
        .is_allowed());
    }

    #[tokio::test]
    async fn a_principal_from_another_hook_is_a_fault_not_an_allow() {
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);

        let foreign = Principal::anonymous();
        let namespace = TrackNamespace::from_utf8_path("sports/football");
        let request = AuthRequest {
            session: &session(),
            principal: &foreign,
            operation: AuthzOperation::PublishNamespace {
                namespace: &namespace,
            },
            request_id: None,
        };

        let err = hook
            .on_request(&request)
            .await
            .expect_err("a foreign principal is a fault");
        assert!(matches!(err, AuthError::Backend(_)), "{err:?}");
    }

    #[tokio::test]
    async fn authorization_does_not_depend_on_the_relay_scope_string() {
        // The scope label is for logging; authorization comes from the claims.
        let key = generate_key();
        let hook = hook(vec![AuthPublicKey::es256(key.pem)]);
        let principal = principal_for(&hook, publisher_token(&key.signer, &[b"sports"])).await;

        let namespace = TrackNamespace::from_utf8_path("sports/football");
        let elsewhere = SessionContext::public(Some("a-completely-different-scope".to_string()));
        let request = AuthRequest {
            session: &elsewhere,
            principal: &principal,
            operation: AuthzOperation::PublishNamespace {
                namespace: &namespace,
            },
            request_id: None,
        };

        assert!(hook.on_request(&request).await.unwrap().is_allowed());
    }

    #[test]
    fn cat_errors_map_onto_deny_reasons() {
        assert!(matches!(
            map_cat_error(CatError::TokenExpired),
            DenyReason::TokenExpired
        ));
        assert!(matches!(
            map_cat_error(CatError::TokenNotYetValid),
            DenyReason::TokenExpired
        ));
        assert!(matches!(
            map_cat_error(CatError::InvalidIssuer),
            DenyReason::IssuerUnknown
        ));
        assert!(matches!(
            map_cat_error(CatError::SignatureVerificationFailed),
            DenyReason::TokenInvalid
        ));
        assert!(matches!(
            map_cat_error(CatError::InvalidTokenFormat),
            DenyReason::TokenMalformed
        ));
        assert!(matches!(
            map_cat_error(CatError::ReplayAttackDetected),
            DenyReason::TokenReplayed
        ));
        assert!(matches!(
            map_cat_error(CatError::MoqtActionNotAuthorized("x".into())),
            DenyReason::ScopeMismatch
        ));
    }
}
