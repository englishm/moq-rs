// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use moq_transport::coding::{TrackName, TrackNamespace, TrackNamespacePrefix};
use moq_transport::message::RequestErrorCode;
use secrecy::{ExposeSecret, SecretSlice};

use crate::SessionContext;

/// A resolved AUTHORIZATION TOKEN (draft-16 §9.2.2.1).
///
/// Alias bookkeeping is resolved before a hook sees this, so implementations
/// always get a Token Type and Token Value pair, never a raw alias directive.
///
/// The value is a bearer credential: anyone holding the bytes can act as the
/// peer until the token expires. It is therefore held in a [`SecretSlice`],
/// which zeroizes on drop and redacts under `Debug`, and reading it requires
/// an explicit [`expose_secret`](ExposeSecret::expose_secret) — so a copy is
/// something a caller has to ask for rather than something that happens by
/// accident.
///
/// # What this does and does not protect
///
/// It bounds the lifetime of this one copy, and this is the only copy the
/// relay's own decoding creates — [`decode_setup_tokens`] borrows the
/// parameter rather than duplicating it. It does **not** make the process
/// secret-free. Others exist and none are zeroized:
///
/// * the QUIC receive path, and the transport's `Reader` buffer, which
///   retains the raw CLIENT_SETUP for the life of the session;
/// * [`Session::setup_params`], likewise for the life of the session;
/// * with the `auth-cat` feature, the allocations `cat-token` makes per
///   verification attempt while base64-decoding the token and re-encoding the
///   signing input — together these reconstruct the token exactly, and there
///   are up to `MAX_SETUP_VERIFICATIONS` attempts.
///
/// So this narrows one window rather than closing the file. Its real value is
/// the type: reading a bearer credential requires saying
/// [`expose_secret`](ExposeSecret::expose_secret), and `Debug` cannot print
/// one by accident.
///
/// Deliberately not [`Clone`] — `secrecy` would permit it, but duplicating a
/// credential should be a visible act, and no caller needs it.
///
/// [`decode_setup_tokens`]: super::decode_setup_tokens
/// [`Session::setup_params`]: moq_transport::session::Session::setup_params
#[derive(Debug)]
pub struct AuthToken {
    /// Token type from the IANA "MOQT Auth Token Type" registry. Type 0 is
    /// reserved for types negotiated out of band.
    pub token_type: u64,

    /// The token payload. Its serialization is defined by `token_type`.
    ///
    /// Read with `token.value.expose_secret()`, or [`expose_value`].
    ///
    /// [`expose_value`]: Self::expose_value
    pub value: SecretSlice<u8>,
}

impl AuthToken {
    /// Wrap a decoded token value.
    ///
    /// Takes an owned `Vec` so the caller hands over its copy rather than
    /// leaving one behind for the allocator to recycle unzeroized. That holds
    /// when the vector's capacity equals its length, as it does for the
    /// `to_vec` in [`decode_setup_tokens`]: conversion to a boxed slice then
    /// reuses the allocation instead of reallocating and freeing the original.
    ///
    /// [`decode_setup_tokens`]: super::decode_setup_tokens
    pub fn new(token_type: u64, value: Vec<u8>) -> Self {
        Self {
            token_type,
            value: SecretSlice::from(value),
        }
    }

    /// The token payload.
    ///
    /// Named for what it does: every read of a bearer credential should be
    /// visible at the call site.
    pub fn expose_value(&self) -> &[u8] {
        self.value.expose_secret()
    }
}

/// An authenticated identity, established once at session setup.
///
/// Carries hook-private validated state — for the CAT hook, the decoded token —
/// so that per-request authorization is a pure claims check with no
/// cryptography. Cheap to clone: the claims are behind an [`Arc`].
#[derive(Clone)]
pub struct Principal {
    /// Token subject (`sub`), for logging and metrics only. Never an
    /// authorization input: authorization decisions come from the claims.
    subject: Option<String>,

    /// Validated state owned by the hook that produced this principal.
    claims: Arc<dyn Any + Send + Sync>,
}

impl Principal {
    /// Create a principal carrying hook-private validated state.
    pub fn new<T: Any + Send + Sync>(subject: Option<String>, claims: T) -> Self {
        Self {
            subject,
            claims: Arc::new(claims),
        }
    }

    /// Create a principal with no claims, for hooks that carry no per-session
    /// state.
    pub fn anonymous() -> Self {
        Self::new(None, ())
    }

    /// The token subject, for logging and metrics.
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    /// Recover the claims this principal was created with.
    ///
    /// Returns `None` if the principal came from a different hook. A hook that
    /// only ever sees principals it created should treat `None` as an internal
    /// error and deny.
    pub fn claims<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.claims.downcast_ref::<T>()
    }
}

impl fmt::Debug for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Principal")
            .field("subject", &self.subject)
            .finish_non_exhaustive()
    }
}

/// The operation being authorized.
///
/// Each variant corresponds to a live authorization point in the relay. FETCH
/// is absent because the transport answers it with NOT_SUPPORTED, so no
/// authorization decision is ever reached for it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuthzOperation<'a> {
    /// Inbound PUBLISH_NAMESPACE (draft-16 §9.20).
    PublishNamespace { namespace: &'a TrackNamespace },

    /// Inbound PUBLISH for a single track (§9.13).
    Publish {
        namespace: &'a TrackNamespace,
        track: &'a TrackName,
    },

    /// Inbound SUBSCRIBE (§9.9).
    Subscribe {
        namespace: &'a TrackNamespace,
        track: &'a TrackName,
    },

    /// Inbound SUBSCRIBE_NAMESPACE (§9.25).
    SubscribeNamespace { prefix: &'a TrackNamespacePrefix },

    /// Inbound TRACK_STATUS (§9.19).
    TrackStatus {
        namespace: &'a TrackNamespace,
        track: &'a TrackName,
    },
}

impl AuthzOperation<'_> {
    /// A stable label for logs and metrics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::PublishNamespace { .. } => "publish_namespace",
            Self::Publish { .. } => "publish",
            Self::Subscribe { .. } => "subscribe",
            Self::SubscribeNamespace { .. } => "subscribe_namespace",
            Self::TrackStatus { .. } => "track_status",
        }
    }
}

/// A single authorization request.
pub struct AuthRequest<'a> {
    /// The session the request arrived on.
    pub session: &'a SessionContext,

    /// The identity established at setup.
    pub principal: &'a Principal,

    /// What the peer is asking to do.
    pub operation: AuthzOperation<'a>,

    /// Request ID of the message being authorized, when known.
    pub request_id: Option<u64>,
}

/// A hook's verdict.
#[derive(Debug, Clone)]
pub struct AuthDecision {
    pub verdict: Verdict,
}

impl AuthDecision {
    /// Allow the operation, establishing the given identity.
    pub fn allow(principal: Principal) -> Self {
        Self {
            verdict: Verdict::Allow(principal),
        }
    }

    /// Deny the operation.
    pub fn deny(reason: DenyReason) -> Self {
        Self {
            verdict: Verdict::Deny(reason),
        }
    }

    /// Whether the operation was allowed.
    pub fn is_allowed(&self) -> bool {
        matches!(self.verdict, Verdict::Allow(_))
    }

    /// The established identity, if allowed.
    pub fn principal(&self) -> Option<&Principal> {
        match &self.verdict {
            Verdict::Allow(principal) => Some(principal),
            Verdict::Deny(_) => None,
        }
    }

    /// The denial reason, if denied.
    pub fn deny_reason(&self) -> Option<&DenyReason> {
        match &self.verdict {
            Verdict::Allow(_) => None,
            Verdict::Deny(reason) => Some(reason),
        }
    }

    /// Consume the decision, yielding the identity if allowed.
    pub fn into_principal(self) -> Result<Principal, DenyReason> {
        match self.verdict {
            Verdict::Allow(principal) => Ok(principal),
            Verdict::Deny(reason) => Err(reason),
        }
    }
}

/// Allow, carrying the established identity, or deny with a reason.
#[derive(Debug, Clone)]
pub enum Verdict {
    Allow(Principal),
    Deny(DenyReason),
}

/// Why an operation was denied.
///
/// Abstract on purpose: the relay maps these onto wire codes, and the mapping
/// differs between session termination (§13.4.1) and per-request rejection
/// (§13.4.2).
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum DenyReason {
    /// No AUTHORIZATION TOKEN of a usable type was presented.
    #[error("token missing")]
    TokenMissing,

    /// The token was structurally valid but did not verify, or carried
    /// unacceptable standard claims such as `iss` or `aud`.
    #[error("token invalid")]
    TokenInvalid,

    /// The token is outside its `exp` / `nbf` validity window.
    #[error("token expired")]
    TokenExpired,

    /// The token was replayed.
    #[error("token replayed")]
    TokenReplayed,

    /// The token could not be decoded.
    #[error("token malformed")]
    TokenMalformed,

    /// The token is valid but its claims do not cover this operation.
    #[error("operation outside token scope")]
    ScopeMismatch,

    /// The token was issued by an issuer this scope does not accept.
    #[error("issuer unknown")]
    IssuerUnknown,

    /// Denied for a hook-specific reason that is a property of the token or
    /// the scope's configuration, and so holds for as long as the session
    /// does — an unenforceable claim, an unusable key set.
    ///
    /// Callers may cache this the same way they cache the named variants.
    #[error("{message}")]
    PolicyDenied { message: String },

    /// The hook could not reach a verdict: an unreachable backend, an
    /// unusable clock.
    ///
    /// Denies like any other reason, but says nothing about the request, so
    /// it must be retried rather than remembered — and it indicates a relay
    /// or infrastructure problem rather than a misbehaving peer.
    #[error("{message}")]
    HookFault { message: String },
}

impl DenyReason {
    /// Whether this is a decision about the peer's authority, rather than a
    /// failure to reach one.
    ///
    /// A policy denial is a property of the token or the scope configuration
    /// and holds for as long as the session does, so a caller may cache it.
    /// [`HookFault`](Self::HookFault) is the only variant that may not be
    /// cached: it describes the relay's own state, which can recover between
    /// one request and the next.
    pub fn is_policy_denial(&self) -> bool {
        !matches!(self, Self::HookFault { .. })
    }

    /// A stable label for metrics. Deliberately low-cardinality, and never
    /// derived from peer-controlled text.
    pub fn label(&self) -> &'static str {
        match self {
            Self::TokenMissing => "token_missing",
            Self::TokenInvalid => "token_invalid",
            Self::TokenExpired => "token_expired",
            Self::TokenReplayed => "token_replayed",
            Self::TokenMalformed => "token_malformed",
            Self::ScopeMismatch => "scope_mismatch",
            Self::IssuerUnknown => "issuer_unknown",
            Self::PolicyDenied { .. } => "policy_denied",
            Self::HookFault { .. } => "hook_fault",
        }
    }

    /// The REQUEST_ERROR code for rejecting a single request (§13.4.2).
    ///
    /// Malformed and expired tokens have dedicated codes; everything else is
    /// UNAUTHORIZED. The reason phrase sent alongside is fixed text, so no
    /// detail about why validation failed reaches the peer.
    pub fn request_error_code(&self) -> u64 {
        match self {
            Self::TokenMalformed => RequestErrorCode::MalformedAuthToken as u64,
            Self::TokenExpired => RequestErrorCode::ExpiredAuthToken as u64,
            _ => RequestErrorCode::Unauthorized as u64,
        }
    }
}

/// A failure to reach a verdict.
///
/// Distinct from [`DenyReason`]: a deny is an authorization outcome, whereas
/// this means the hook could not evaluate the request at all. Both fail closed,
/// but only this one indicates a relay or configuration fault worth alerting
/// on, so the two are counted separately.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    /// The scope's authorization configuration is unusable — no keys, too many
    /// keys, unparseable key material, or a scheme this binary was not built
    /// with. Every session in the scope is refused until it is corrected.
    #[error("scope authorization is misconfigured: {0}")]
    Configuration(String),

    /// A dependency needed to reach a verdict failed, such as the coordinator
    /// being unreachable.
    #[error("authorization backend failure: {0}")]
    Backend(String),

    /// The AUTHORIZATION TOKEN parameter could not be decoded. The peer
    /// violated the wire format, so the session is terminated rather than the
    /// individual request rejected.
    #[error("malformed authorization token parameter: {0}")]
    Malformed(#[from] super::TokenError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_token_debug_does_not_leak_the_value() {
        let token = AuthToken::new(1, b"super-secret-bearer-token".to_vec());

        let rendered = format!("{token:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("REDACTED"), "{rendered}");
        // The type is still identifiable for diagnostics.
        assert!(rendered.contains("token_type: 1"), "{rendered}");
    }

    /// The value is reachable only through an explicit exposure, so a copy is
    /// something a caller asks for rather than something that happens by
    /// accident.
    #[test]
    fn auth_token_value_requires_explicit_exposure() {
        let token = AuthToken::new(1, b"payload".to_vec());
        assert_eq!(token.expose_value(), b"payload");
        assert_eq!(token.value.expose_secret(), b"payload");
    }

    #[test]
    fn principal_debug_does_not_leak_claims() {
        let principal = Principal::new(Some("alice".to_string()), "sensitive-claims");
        let rendered = format!("{principal:?}");
        assert!(rendered.contains("alice"), "{rendered}");
        assert!(!rendered.contains("sensitive-claims"), "{rendered}");
    }

    #[test]
    fn principal_round_trips_typed_claims() {
        #[derive(Debug, PartialEq)]
        struct Claims(u32);

        let principal = Principal::new(Some("bob".to_string()), Claims(7));
        assert_eq!(principal.subject(), Some("bob"));
        assert_eq!(principal.claims::<Claims>(), Some(&Claims(7)));
        // A hook asking for a type it did not store gets None, not a panic.
        assert_eq!(principal.claims::<String>(), None);
    }

    #[test]
    fn decision_accessors_agree() {
        let allow = AuthDecision::allow(Principal::anonymous());
        assert!(allow.is_allowed());
        assert!(allow.principal().is_some());
        assert!(allow.deny_reason().is_none());

        let deny = AuthDecision::deny(DenyReason::TokenMissing);
        assert!(!deny.is_allowed());
        assert!(deny.principal().is_none());
        assert!(deny.deny_reason().is_some());
        assert!(deny.into_principal().is_err());
    }

    /// Only a hook fault may be retried; everything else is a stable property
    /// of the token or the scope and may be cached for the session.
    #[test]
    fn only_hook_faults_are_retryable() {
        for stable in [
            DenyReason::TokenMissing,
            DenyReason::TokenInvalid,
            DenyReason::TokenExpired,
            DenyReason::TokenReplayed,
            DenyReason::TokenMalformed,
            DenyReason::ScopeMismatch,
            DenyReason::IssuerUnknown,
            DenyReason::PolicyDenied {
                message: "unenforceable claim: cnf".to_string(),
            },
        ] {
            assert!(
                stable.is_policy_denial(),
                "{stable} should be cacheable as a policy decision"
            );
        }

        assert!(!DenyReason::HookFault {
            message: "authorization unavailable".to_string(),
        }
        .is_policy_denial());
    }

    #[test]
    fn deny_reasons_map_to_draft16_request_error_codes() {
        // §13.4.2: MALFORMED_AUTH_TOKEN 0x4, EXPIRED_AUTH_TOKEN 0x5,
        // UNAUTHORIZED 0x1.
        assert_eq!(DenyReason::TokenMalformed.request_error_code(), 0x4);
        assert_eq!(DenyReason::TokenExpired.request_error_code(), 0x5);
        assert_eq!(DenyReason::TokenMissing.request_error_code(), 0x1);
        assert_eq!(DenyReason::TokenInvalid.request_error_code(), 0x1);
        assert_eq!(DenyReason::ScopeMismatch.request_error_code(), 0x1);
        assert_eq!(DenyReason::IssuerUnknown.request_error_code(), 0x1);
        assert_eq!(DenyReason::TokenReplayed.request_error_code(), 0x1);
    }
}
