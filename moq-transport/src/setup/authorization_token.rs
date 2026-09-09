// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;

use crate::coding::{BoundsExceeded, Encode, EncodeError, VarInt};

/// An opaque authorization token sent inline during session setup.
///
/// The token type identifies how the receiver interprets `value`; this type
/// deliberately has no knowledge of CAT or any other token format.
pub struct AuthorizationToken {
    token_type: VarInt,
    value: Vec<u8>,
}

impl AuthorizationToken {
    /// Create a token with a MoQT token type and opaque value.
    pub fn new(token_type: u64, value: Vec<u8>) -> Result<Self, BoundsExceeded> {
        Ok(Self {
            token_type: token_type.try_into()?,
            value,
        })
    }

    pub(crate) fn encode_value(&self) -> Result<Vec<u8>, EncodeError> {
        let mut encoded = Vec::new();
        VarInt::from(3u8).encode(&mut encoded)?; // USE_VALUE
        self.token_type.encode(&mut encoded)?;
        encoded.extend_from_slice(&self.value);
        Ok(encoded)
    }
}

impl fmt::Debug for AuthorizationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizationToken")
            .field("token_type", &self.token_type)
            .field(
                "value",
                &format_args!("<redacted, {} bytes>", self.value.len()),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_an_inline_token_without_a_value_length() {
        let token = AuthorizationToken::new(1, vec![0xaa, 0xbb]).unwrap();

        assert_eq!(token.encode_value().unwrap(), [0x03, 0x01, 0xaa, 0xbb]);
    }

    #[test]
    fn debug_redacts_the_token_value() {
        let token = AuthorizationToken::new(1, b"secret".to_vec()).unwrap();
        let debug = format!("{token:?}");

        assert!(debug.contains("<redacted, 6 bytes>"));
        assert!(!debug.contains("secret"));
    }
}
