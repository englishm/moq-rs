// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AUTHORIZATION TOKEN parameter decoding (draft-ietf-moq-transport-16
//! §9.2.2.1, §9.3.1.5).
//!
//! The parameter value is a single Token structure:
//!
//! ```text
//! Token {
//!   Alias Type (i),
//!   [Token Alias (i),]
//!   [Token Type (i),]
//!   [Token Value (..)]
//! }
//! ```
//!
//! Two properties of this format are easy to get wrong and are both load
//! bearing here:
//!
//! * **Token Value is not length-prefixed.** `(..)` runs to the end of the
//!   parameter value, so its length comes from the enclosing key-value pair.
//! * **One Token per parameter.** §9.2.2.1 permits the parameter to be
//!   repeated, so multiple tokens arrive as repeated key-value pairs rather
//!   than as a sequence packed into one value.

use moq_transport::coding::{Decode, KeyValuePairs, Value, VarInt};
use moq_transport::setup::ParameterType;

use super::AuthToken;

/// Alias Type code points (draft-16 §13.1, Table 7).
mod alias_type {
    /// Retire an alias. Carries an alias but no type or value.
    pub const DELETE: u64 = 0x0;
    /// Bind an alias to a type and value for the rest of the session.
    pub const REGISTER: u64 = 0x1;
    /// Reference a previously registered alias. No type or value.
    pub const USE_ALIAS: u64 = 0x2;
    /// Inline type and value, no alias.
    pub const USE_VALUE: u64 = 0x3;
}

/// Token type for a Common Access Token (draft-ietf-moq-c4m-01 §7.1).
pub const CAT_TOKEN_TYPE: u64 = 0x01;

/// Upper bound on tokens accepted from a single CLIENT_SETUP.
///
/// §9.2.2.1 lets the parameter repeat without stating a limit, but each token
/// costs signature verifications at setup, so a receiver has to bound it. Four
/// is well above any honest case — a client holding credentials for several
/// relays presents one or two — while the real protection against a peer
/// inflating admission cost is the verification budget, since a single token
/// can already be tried against every configured key.
pub const MAX_SETUP_TOKENS: usize = 4;

/// A failure to decode an AUTHORIZATION TOKEN parameter.
///
/// Each variant maps to the session termination code draft-16 mandates for
/// that condition; see [`session_error_code`](TokenError::session_error_code).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// The Token structure could not be decoded.
    #[error("malformed token structure: {0}")]
    Malformed(&'static str),

    /// DELETE or USE_ALIAS appeared in CLIENT_SETUP. §9.2.2.1 requires the
    /// server to close the session with PROTOCOL_VIOLATION, since no alias can
    /// have been registered before the first message of the session.
    #[error("alias type {0:#x} is not permitted in CLIENT_SETUP")]
    AliasInSetup(u64),

    /// The parameter was present but not bytes-valued.
    #[error("AUTHORIZATION TOKEN parameter must be bytes-encoded")]
    NotBytes,

    /// More tokens than [`MAX_SETUP_TOKENS`] were presented.
    #[error("too many authorization tokens: {0}")]
    TooMany(usize),
}

impl TokenError {
    /// The session termination code for this failure (draft-16 §13.4.1).
    pub fn session_error_code(&self) -> u32 {
        match self {
            // §9.2.2.1: "If a server receives Alias Type DELETE (0x0) or
            // USE_ALIAS (0x2) in a CLIENT_SETUP message, it MUST close the
            // session with a PROTOCOL_VIOLATION."
            Self::AliasInSetup(_) => 0x3,
            // §9.2.2.1: "If the Token structure cannot be decoded, the receiver
            // MUST close the Session with KEY_VALUE_FORMATTING_ERROR."
            Self::Malformed(_) | Self::NotBytes => 0x6,
            // Not a decoding failure; a resource bound. UNAUTHORIZED.
            Self::TooMany(_) => 0x2,
        }
    }

    /// The reason phrase sent to the peer. Fixed text, never validation detail.
    pub fn reason_phrase(&self) -> &'static str {
        match self {
            Self::AliasInSetup(_) => "token alias not permitted in setup",
            Self::Malformed(_) | Self::NotBytes => "malformed authorization token",
            Self::TooMany(_) => "too many authorization tokens",
        }
    }

    /// A stable, low-cardinality label for metrics.
    pub fn metric_label(&self) -> &'static str {
        match self {
            Self::AliasInSetup(_) => "alias_in_setup",
            Self::Malformed(_) | Self::NotBytes => "token_malformed",
            Self::TooMany(_) => "too_many_tokens",
        }
    }
}

/// Decode every AUTHORIZATION TOKEN parameter from a CLIENT_SETUP parameter set.
///
/// Iterates the parameters directly rather than using [`KeyValuePairs::get`],
/// which returns only the first match and would silently drop the repetitions
/// §9.2.2.1 permits.
///
/// Returns tokens in the order they appeared. An absent parameter yields an
/// empty vector; whether that is acceptable is the caller's policy decision.
pub fn decode_setup_tokens(params: &KeyValuePairs) -> Result<Vec<AuthToken>, TokenError> {
    let key = u64::from(ParameterType::AuthorizationToken);
    let mut tokens = Vec::new();

    let present = params.0.iter().filter(|kvp| kvp.key == key).count();

    for kvp in params.0.iter().filter(|kvp| kvp.key == key) {
        let Value::BytesValue(bytes) = &kvp.value else {
            return Err(TokenError::NotBytes);
        };

        // Decode every parameter even once the bound is exceeded, so that a
        // structural violation is still reported as one. §9.2.2.1's "MUST
        // close with PROTOCOL_VIOLATION" for an alias directive in
        // CLIENT_SETUP does not stop applying because the peer also sent too
        // many tokens.
        let token = decode_token(bytes)?;
        if tokens.len() < MAX_SETUP_TOKENS {
            tokens.push(token);
        }
    }

    if present > MAX_SETUP_TOKENS {
        return Err(TokenError::TooMany(present));
    }

    Ok(tokens)
}

/// Decode one Token structure from a parameter value.
///
/// Borrows the caller's bytes rather than copying them: the value is a bearer
/// credential, and an intermediate `Bytes` would be a second copy that nothing
/// zeroizes. `&[u8]` implements `Buf`, so decoding advances the borrow instead,
/// leaving [`AuthToken`]'s zeroizing buffer as the only copy this module makes.
fn decode_token(value: &[u8]) -> Result<AuthToken, TokenError> {
    let mut buf = value;

    let alias_type = VarInt::decode(&mut buf)
        .map_err(|_| TokenError::Malformed("missing alias type"))?
        .into_inner();

    match alias_type {
        // Both reference an alias that cannot exist yet: CLIENT_SETUP is the
        // first message of the session, so nothing has been registered.
        alias_type::DELETE | alias_type::USE_ALIAS => Err(TokenError::AliasInSetup(alias_type)),

        // §9.3.1.5: a server that advertises no MAX_AUTH_TOKEN_CACHE_SIZE
        // "MUST NOT fail the session with AUTH_TOKEN_CACHE_OVERFLOW. Instead,
        // it MUST treat the parameter as Alias Type USE_VALUE." We never
        // advertise the parameter, so its default of 0 applies and every
        // registration is handled as an inline value. The alias is consumed
        // and discarded.
        alias_type::REGISTER => {
            VarInt::decode(&mut buf)
                .map_err(|_| TokenError::Malformed("REGISTER missing token alias"))?;
            decode_type_and_value(buf)
        }

        alias_type::USE_VALUE => decode_type_and_value(buf),

        // An unrecognized alias type means the rest of the structure cannot be
        // interpreted, which §9.2.2.1 treats as a decode failure.
        _ => Err(TokenError::Malformed("unknown alias type")),
    }
}

/// Decode the trailing `Token Type (i)` and `Token Value (..)`.
///
/// The value is whatever remains: it carries no length prefix of its own.
fn decode_type_and_value(mut buf: &[u8]) -> Result<AuthToken, TokenError> {
    let token_type = VarInt::decode(&mut buf)
        .map_err(|_| TokenError::Malformed("missing token type"))?
        .into_inner();

    Ok(AuthToken::new(token_type, buf.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use moq_transport::coding::{Encode, KeyValuePair};

    /// Build a parameter set from raw AUTHORIZATION TOKEN values, preserving
    /// repeats. `KeyValuePairs::set` deduplicates by key, so the repeated-key
    /// case has to be constructed directly.
    fn params(values: Vec<Vec<u8>>) -> KeyValuePairs {
        let key = u64::from(ParameterType::AuthorizationToken);
        KeyValuePairs(
            values
                .into_iter()
                .map(|value| KeyValuePair::new_bytes(key, value))
                .collect(),
        )
    }

    fn varint(value: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        VarInt::try_from(value).unwrap().encode(&mut buf).unwrap();
        buf
    }

    /// A USE_VALUE token, encoded the way a conformant peer would.
    fn use_value(token_type: u64, value: &[u8]) -> Vec<u8> {
        let mut buf = varint(alias_type::USE_VALUE);
        buf.extend_from_slice(&varint(token_type));
        buf.extend_from_slice(value);
        buf
    }

    #[test]
    fn decodes_a_use_value_token() {
        let decoded = decode_setup_tokens(&params(vec![use_value(CAT_TOKEN_TYPE, b"payload")]))
            .expect("well-formed");

        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].token_type, CAT_TOKEN_TYPE);
        assert_eq!(decoded[0].expose_value(), b"payload");
    }

    /// The PoC used 0x2 for USE_VALUE. Per §13.1 Table 7 that is USE_ALIAS,
    /// which §9.2.2.1 forbids in CLIENT_SETUP.
    #[test]
    fn use_value_is_0x3_and_use_alias_is_rejected() {
        assert_eq!(alias_type::USE_VALUE, 0x3);
        assert_eq!(alias_type::USE_ALIAS, 0x2);

        let mut mislabelled = varint(0x2);
        mislabelled.extend_from_slice(&varint(CAT_TOKEN_TYPE));
        mislabelled.extend_from_slice(b"payload");

        assert_eq!(
            decode_setup_tokens(&params(vec![mislabelled])).unwrap_err(),
            TokenError::AliasInSetup(0x2)
        );
    }

    #[test]
    fn delete_in_setup_is_a_protocol_violation() {
        let mut delete = varint(alias_type::DELETE);
        delete.extend_from_slice(&varint(7));

        let err = decode_setup_tokens(&params(vec![delete])).unwrap_err();
        assert_eq!(err, TokenError::AliasInSetup(0x0));
        // §9.2.2.1 mandates PROTOCOL_VIOLATION (0x3) specifically.
        assert_eq!(err.session_error_code(), 0x3);
    }

    #[test]
    fn use_alias_in_setup_is_a_protocol_violation() {
        let mut use_alias = varint(alias_type::USE_ALIAS);
        use_alias.extend_from_slice(&varint(3));

        let err = decode_setup_tokens(&params(vec![use_alias])).unwrap_err();
        assert_eq!(err, TokenError::AliasInSetup(0x2));
        assert_eq!(err.session_error_code(), 0x3);
    }

    /// §9.3.1.5: with no MAX_AUTH_TOKEN_CACHE_SIZE advertised, REGISTER is
    /// handled as USE_VALUE rather than failing the session.
    #[test]
    fn register_is_treated_as_use_value() {
        let mut register = varint(alias_type::REGISTER);
        register.extend_from_slice(&varint(42)); // Token Alias
        register.extend_from_slice(&varint(CAT_TOKEN_TYPE));
        register.extend_from_slice(b"payload");

        let decoded = decode_setup_tokens(&params(vec![register])).expect("treated as USE_VALUE");

        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].token_type, CAT_TOKEN_TYPE);
        assert_eq!(decoded[0].expose_value(), b"payload");
    }

    /// The value runs to the end of the parameter. A payload that begins with
    /// bytes resembling a varint length must survive intact — this is exactly
    /// what a length-prefixed parser corrupts.
    #[test]
    fn token_value_is_not_length_prefixed() {
        // 0x07 would be read as "length 7" by a length-prefixing parser,
        // truncating a 12-byte payload.
        let payload = b"\x07\x41\x42\x43\x44\x45\x46\x47\x48\x49\x4a\x4b";
        let decoded =
            decode_setup_tokens(&params(vec![use_value(CAT_TOKEN_TYPE, payload)])).unwrap();

        assert_eq!(decoded[0].expose_value(), payload);
        assert_eq!(decoded[0].expose_value().len(), 12);
    }

    #[test]
    fn empty_token_value_is_accepted_by_the_decoder() {
        // Structurally valid: Token Value is optional in the grammar. Whether
        // an empty CAT payload is acceptable is the hook's decision, not the
        // decoder's.
        let decoded = decode_setup_tokens(&params(vec![use_value(CAT_TOKEN_TYPE, b"")])).unwrap();

        assert_eq!(decoded.len(), 1);
        assert!(decoded[0].expose_value().is_empty());
    }

    /// §9.2.2.1: "The AUTHORIZATION TOKEN parameter MAY be repeated within a
    /// message." Each repeat is its own key-value pair.
    #[test]
    fn repeated_parameters_all_decode() {
        let decoded = decode_setup_tokens(&params(vec![
            use_value(CAT_TOKEN_TYPE, b"first"),
            use_value(0, b"second"),
            use_value(CAT_TOKEN_TYPE, b"third"),
        ]))
        .unwrap();

        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0].expose_value(), b"first");
        assert_eq!(decoded[1].token_type, 0);
        assert_eq!(decoded[2].expose_value(), b"third");
    }

    #[test]
    fn absent_parameter_yields_no_tokens() {
        assert!(decode_setup_tokens(&KeyValuePairs::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn other_parameters_are_ignored() {
        let mut p = KeyValuePairs::default();
        p.set_intvalue(u64::from(ParameterType::MaxRequestId), 100);
        p.set_bytesvalue(u64::from(ParameterType::Path), b"/tenant".to_vec());

        assert!(decode_setup_tokens(&p).unwrap().is_empty());
    }

    #[test]
    fn empty_parameter_value_is_malformed() {
        let err = decode_setup_tokens(&params(vec![vec![]])).unwrap_err();
        assert_eq!(err, TokenError::Malformed("missing alias type"));
        assert_eq!(err.session_error_code(), 0x6);
    }

    #[test]
    fn use_value_without_token_type_is_malformed() {
        let err = decode_setup_tokens(&params(vec![varint(alias_type::USE_VALUE)])).unwrap_err();
        assert_eq!(err, TokenError::Malformed("missing token type"));
        assert_eq!(err.session_error_code(), 0x6);
    }

    #[test]
    fn register_without_alias_is_malformed() {
        let err = decode_setup_tokens(&params(vec![varint(alias_type::REGISTER)])).unwrap_err();
        assert_eq!(err, TokenError::Malformed("REGISTER missing token alias"));
    }

    #[test]
    fn unknown_alias_type_is_malformed() {
        let err = decode_setup_tokens(&params(vec![varint(0x9)])).unwrap_err();
        assert_eq!(err, TokenError::Malformed("unknown alias type"));
        assert_eq!(err.session_error_code(), 0x6);
    }

    #[test]
    fn truncated_varint_is_malformed() {
        // 0x40 opens a two-byte varint whose second byte never arrives.
        let err = decode_setup_tokens(&params(vec![vec![0x40]])).unwrap_err();
        assert_eq!(err, TokenError::Malformed("missing alias type"));
    }

    #[test]
    fn int_valued_parameter_is_rejected() {
        let key = u64::from(ParameterType::AuthorizationToken);
        let p = KeyValuePairs(vec![KeyValuePair::new_int(key, 5)]);

        let err = decode_setup_tokens(&p).unwrap_err();
        assert_eq!(err, TokenError::NotBytes);
        assert_eq!(err.session_error_code(), 0x6);
    }

    #[test]
    fn token_count_is_bounded() {
        let values = (0..MAX_SETUP_TOKENS + 1)
            .map(|_| use_value(CAT_TOKEN_TYPE, b"payload"))
            .collect();

        let err = decode_setup_tokens(&params(values)).unwrap_err();
        assert!(matches!(err, TokenError::TooMany(_)));

        // Exactly at the bound is still accepted.
        let at_limit = (0..MAX_SETUP_TOKENS)
            .map(|_| use_value(CAT_TOKEN_TYPE, b"payload"))
            .collect();
        assert_eq!(
            decode_setup_tokens(&params(at_limit)).unwrap().len(),
            MAX_SETUP_TOKENS
        );
    }

    #[test]
    fn multi_byte_varint_token_type_round_trips() {
        // 0x63346d is the value cat-token uses for its own "c4m" type; it needs
        // a four-byte varint, exercising the non-trivial encoding path.
        let decoded = decode_setup_tokens(&params(vec![use_value(0x63346d, b"payload")])).unwrap();

        assert_eq!(decoded[0].token_type, 0x63346d);
        assert_eq!(decoded[0].expose_value(), b"payload");
    }
}
