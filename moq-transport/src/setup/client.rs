// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! CLIENT_SETUP message (draft-ietf-moq-transport-16 §9.3).
//!
//! From draft-16, version negotiation is performed via ALPN only.  The
//! CLIENT_SETUP and SERVER_SETUP payloads carry setup parameters only;
//! they no longer contain a version list.

use crate::coding::{Decode, DecodeError, Encode, EncodeError, KeyValuePairs};
use bytes::Buf as _;
use std::fmt;

/// Sent by the client to set up the session.
///
/// Message Type = 0x20 (unchanged from draft-11+).
/// The payload contains only setup parameters; version is agreed via ALPN.
pub struct Client {
    /// Setup parameters (PATH, AUTHORITY, MAX_REQUEST_ID,
    /// MAX_AUTH_TOKEN_CACHE_SIZE, AUTHORIZATION_TOKEN, MOQT_IMPLEMENTATION, …).
    pub params: KeyValuePairs,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Params<'a>(&'a KeyValuePairs);

        impl fmt::Debug for Params<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let mut list = f.debug_list();
                let Params(params) = self;
                for param in &params.0 {
                    if param.key == u64::from(super::ParameterType::AuthorizationToken) {
                        list.entry(&format_args!("{{{}: <redacted>}}", param.key));
                    } else {
                        list.entry(param);
                    }
                }
                list.finish()
            }
        }

        f.debug_struct("Client")
            .field("params", &Params(&self.params))
            .finish()
    }
}

impl Decode for Client {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let typ = u64::decode(r)?;
        if typ != 0x20 {
            return Err(DecodeError::InvalidMessage(typ));
        }

        let len = u16::decode(r)? as usize;
        <u64 as Decode>::decode_remaining(r, len)?;
        let mut payload = r.copy_to_bytes(len);

        let params = KeyValuePairs::decode(&mut payload)?;
        if payload.has_remaining() {
            return Err(DecodeError::InvalidMessage(typ));
        }

        Ok(Self { params })
    }
}

impl Encode for Client {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        (0x20_u64).encode(w)?;

        let mut buf = Vec::new();
        self.params.encode(&mut buf)?;

        if buf.len() > u16::MAX as usize {
            return Err(EncodeError::MsgBoundsExceeded);
        }
        (buf.len() as u16).encode(w)?;
        Self::encode_remaining(w, buf.len())?;
        w.put_slice(&buf);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        coding::KeyValuePair,
        setup::{AuthorizationToken, ParameterType, Version},
    };
    use bytes::BytesMut;

    #[test]
    fn encode_decode_params_only() {
        let mut buf = BytesMut::new();

        let mut params = KeyValuePairs::default();
        // PATH is odd key (0x01) → bytes value; delta from 0 = 1
        params.set_bytesvalue(ParameterType::Path.into(), b"testpath".to_vec());
        // MAX_REQUEST_ID is even key (0x02) → int value; delta from 1 = 1
        params.set_intvalue(ParameterType::MaxRequestId.into(), 100);

        let client = Client { params };
        client.encode(&mut buf).unwrap();

        // Wire layout:
        //   0x20          type
        //   len (2B)      16-bit length of payload
        //   payload:
        //     0x02        count = 2 params (varint)
        //     0x01        delta=1 → abs_type=1 (PATH, odd→bytes)
        //     0x08        length=8
        //     "testpath"
        //     0x01        delta=1 → abs_type=2 (MAX_REQUEST_ID, even→int)
        //     0x40 0x64   value=100 (2-byte varint, 100 ≥ 64)
        let bytes = buf.to_vec();
        assert_eq!(bytes[0], 0x20); // type
        let payload_len = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
        assert_eq!(bytes.len(), 3 + payload_len);

        let decoded = Client::decode(&mut buf).unwrap();
        assert_eq!(decoded.params, client.params);
    }

    #[test]
    fn repeated_authorization_tokens_round_trip_and_are_redacted() {
        let mut params = KeyValuePairs::default();
        params.set_bytesvalue(ParameterType::Path.into(), b"clock".to_vec());
        params.set_intvalue(ParameterType::MaxRequestId.into(), 100);
        for token in [
            AuthorizationToken::new(1, b"first-secret".to_vec()).unwrap(),
            AuthorizationToken::new(0x63346d, b"second-secret".to_vec()).unwrap(),
        ] {
            params.0.push(KeyValuePair::new_bytes(
                ParameterType::AuthorizationToken.into(),
                token.encode_value().unwrap(),
            ));
        }
        let client = Client { params };
        let debug = format!("{client:?}");
        assert!(!debug.contains("first-secret"));
        assert!(!debug.contains("second-secret"));
        assert_eq!(debug.matches("<redacted>").count(), 2);

        let mut buf = BytesMut::new();
        client.encode(&mut buf).unwrap();
        let decoded = Client::decode(&mut buf).unwrap();

        assert_eq!(decoded.params, client.params);
        assert_eq!(
            decoded
                .params
                .0
                .iter()
                .filter(|param| param.key == u64::from(ParameterType::AuthorizationToken))
                .count(),
            2
        );
    }

    #[test]
    fn encode_decode_no_params() {
        let mut buf = BytesMut::new();
        let client = Client {
            params: KeyValuePairs::default(),
        };
        client.encode(&mut buf).unwrap();

        // Wire: 0x20, 0x00 0x01 (length=1), 0x00 (count=0)
        assert_eq!(buf[0], 0x20);
        let payload_len = u16::from_be_bytes([buf[1], buf[2]]) as usize;
        assert_eq!(payload_len, 1); // just the count varint (0x00)

        let decoded = Client::decode(&mut buf).unwrap();
        assert!(decoded.params.0.is_empty());
    }

    #[test]
    fn decode_rejects_overlong_payload() {
        let mut buf = BytesMut::new();
        let client = Client {
            params: KeyValuePairs::default(),
        };
        client.encode(&mut buf).unwrap();
        buf[2] += 1;
        buf.extend_from_slice(&[0x00]);

        assert!(matches!(
            Client::decode(&mut buf).unwrap_err(),
            DecodeError::InvalidMessage(0x20)
        ));
    }

    /// Confirm DRAFT_16 version constant is defined and has the right value.
    #[test]
    fn draft_16_version_constant() {
        assert_eq!(Version::DRAFT_16.0, 0xff000010);
    }
}
