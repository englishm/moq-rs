// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::coding::{
    Decode, DecodeError, Encode, EncodeError, KeyValuePairs, TrackNamespacePrefix,
};

/// Solicit existing and future PUBLISH requests under a namespace prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscribeTracks {
    pub id: u64,
    pub track_namespace_prefix: TrackNamespacePrefix,
    pub params: KeyValuePairs,
}

impl Decode for SubscribeTracks {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        Ok(Self {
            id: u64::decode(r)?,
            track_namespace_prefix: TrackNamespacePrefix::decode(r)?,
            params: KeyValuePairs::decode_message_params(r)?,
        })
    }
}

impl Encode for SubscribeTracks {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.id.encode(w)?;
        self.track_namespace_prefix.encode(w)?;
        self.params.encode_message_params(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn encode_decode() {
        let mut params = KeyValuePairs::default();
        params.set_forward(false);
        let message = SubscribeTracks {
            id: 42,
            track_namespace_prefix: TrackNamespacePrefix::from_utf8_path("room/123"),
            params,
        };
        let mut buf = BytesMut::new();

        message.encode(&mut buf).unwrap();

        assert_eq!(SubscribeTracks::decode(&mut buf).unwrap(), message);
    }
}
