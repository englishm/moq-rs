// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::coding::{Decode, DecodeError, Encode, EncodeError, TrackName, TrackNamespacePrefix};

/// A track matching SUBSCRIBE_TRACKS that cannot be sent as PUBLISH.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishBlocked {
    pub track_namespace_suffix: TrackNamespacePrefix,
    pub track_name: TrackName,
}

impl Decode for PublishBlocked {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        Ok(Self {
            track_namespace_suffix: TrackNamespacePrefix::decode(r)?,
            track_name: TrackName::decode(r)?,
        })
    }
}

impl Encode for PublishBlocked {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.track_namespace_suffix.encode(w)?;
        self.track_name.encode(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn encode_decode() {
        let message = PublishBlocked {
            track_namespace_suffix: TrackNamespacePrefix::from_utf8_path("participant/100"),
            track_name: "video".into(),
        };
        let mut buf = BytesMut::new();

        message.encode(&mut buf).unwrap();

        assert_eq!(PublishBlocked::decode(&mut buf).unwrap(), message);
    }
}
