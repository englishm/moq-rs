// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::coding::{
    validate_full_track_name, Decode, DecodeError, Encode, EncodeError, KeyValuePairs, TrackName,
    TrackNamespace,
};

/// Sent by the subscriber to request all future objects for the given track.
///
/// Objects will use the provided ID instead of the full track name, to save bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subscribe {
    /// The subscription request ID
    pub id: u64,

    /// Track properties
    pub track_namespace: TrackNamespace,
    pub track_name: TrackName, // TODO SLG - consider making a FullTrackName base struct (total size limit of 4096)

    /// Optional parameters
    pub params: KeyValuePairs,
}

impl Decode for Subscribe {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let id = u64::decode(r)?;

        let track_namespace = TrackNamespace::decode(r)?;
        let track_name = TrackName::decode(r)?;
        validate_full_track_name(&track_namespace, track_name.as_bytes())?;

        let params = KeyValuePairs::decode_message_params(r)?;

        Ok(Self {
            id,
            track_namespace,
            track_name,
            params,
        })
    }
}

impl Encode for Subscribe {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.id.encode(w)?;

        self.track_namespace.encode(w)?;
        self.track_name.encode(w)?;

        self.params.encode_message_params(w)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{GroupOrder, U8_VALUE_PARAMETER_TYPES};
    use bytes::BytesMut;

    #[test]
    fn encode_decode() {
        let mut buf = BytesMut::new();

        let mut kvps = KeyValuePairs::new();
        kvps.set_bytesvalue(123, vec![0x00, 0x01, 0x02, 0x03]);

        let msg = Subscribe {
            id: 12345,
            track_namespace: TrackNamespace::from_utf8_path("test/path/to/resource"),
            track_name: "audiotrack".into(),
            params: kvps.clone(),
        };
        msg.encode(&mut buf).unwrap();
        let decoded = Subscribe::decode(&mut buf).unwrap();
        assert_eq!(decoded, msg);
    }

    /// The interop guarantee: RENDEZVOUS_TIMEOUT survives the wire unchanged.
    /// 0x04 is even, so the KVP codec carries it as a varint with no length prefix.
    #[test]
    fn rendezvous_timeout_survives_the_wire() {
        let mut buf = BytesMut::new();

        let mut params = KeyValuePairs::default();
        params.set_rendezvous_timeout(30_000);

        let msg = Subscribe {
            id: 7,
            track_namespace: TrackNamespace::from_utf8_path("meeting/room"),
            track_name: "camera".into(),
            params,
        };
        msg.encode(&mut buf).unwrap();

        let decoded = Subscribe::decode(&mut buf).unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(decoded.params.rendezvous_timeout().unwrap(), Some(30_000));
    }

    #[test]
    fn default_params_roundtrip() {
        // Verify a minimal SUBSCRIBE with no params still round-trips cleanly.
        let mut buf = BytesMut::new();
        let msg = Subscribe {
            id: 0,
            track_namespace: TrackNamespace::from_utf8_path("a/b"),
            track_name: "t".into(),
            params: KeyValuePairs::default(),
        };
        msg.encode(&mut buf).unwrap();
        let decoded = Subscribe::decode(&mut buf).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn decode_rejects_full_track_name_over_limit() {
        let mut buf = BytesMut::new();
        let msg = Subscribe {
            id: 0,
            track_namespace: TrackNamespace {
                fields: vec![crate::coding::TupleField {
                    value: vec![b'a'; crate::coding::MAX_FULL_TRACK_NAME_LEN],
                }],
            },
            track_name: "x".into(),
            params: KeyValuePairs::default(),
        };

        msg.encode(&mut buf).unwrap();
        let err = Subscribe::decode(&mut buf).unwrap_err();
        assert!(matches!(err, DecodeError::TrackNameTooLong));
    }

    #[test]
    fn minimal_wire_format_has_no_fixed_subscription_fields() {
        let mut buf = BytesMut::new();
        let msg = Subscribe {
            id: 2,
            track_namespace: TrackNamespace::from_utf8_path("ns/v"),
            track_name: "track".into(),
            params: KeyValuePairs::default(),
        };
        msg.encode(&mut buf).unwrap();
        let decoded = Subscribe::decode(&mut buf).unwrap();
        assert_eq!(decoded, msg);
    }

    // ── draft-18 typed-parameter wire format ──────────────────────────────────
    // Draft-17 §10.2 changed SUBSCRIBER_PRIORITY (0x20), GROUP_ORDER (0x22) and
    // FORWARD (0x10) from a varint value to a single raw uint8. The default
    // SUBSCRIBER_PRIORITY of 128 is the value that breaks the older varint form:
    // as a draft-17 leading-1-bits varint it is two bytes (0x80 0x80),
    // desynchronising the whole parameter block for imquic / moq-go peers that
    // read exactly one byte. These vectors pin the byte layout against those
    // reference implementations.

    #[test]
    fn subscribe_params_encode_typed_values_as_single_bytes() {
        let mut params = KeyValuePairs::new();
        params.set_forward(true); // 0x10 → 0x01
        params.set_subscriber_priority(128); // 0x20 → 0x80 (single byte)
        params.set_group_order(GroupOrder::Descending); // 0x22 → 0x02

        let mut buf = BytesMut::new();
        params
            .encode_with_u8_types(&mut buf, U8_VALUE_PARAMETER_TYPES)
            .unwrap();

        // count=3; then delta-encoded (delta, value) pairs sorted by type:
        //   FORWARD             delta 0x10, value 0x01
        //   SUBSCRIBER_PRIORITY delta 0x10, value 0x80  ← single byte, not 0x80 0x80
        //   GROUP_ORDER         delta 0x02, value 0x02
        assert_eq!(buf.to_vec(), vec![0x03, 0x10, 0x01, 0x10, 0x80, 0x02, 0x02]);

        let decoded =
            KeyValuePairs::decode_with_u8_types(&mut buf, U8_VALUE_PARAMETER_TYPES).unwrap();
        assert_eq!(decoded.forward().unwrap(), Some(true));
        assert_eq!(decoded.subscriber_priority().unwrap(), Some(128));
        assert_eq!(decoded.group_order().unwrap(), Some(GroupOrder::Descending));
    }

    #[test]
    fn typed_encoding_avoids_two_byte_varint_priority() {
        // Guard the fix boundary: the *generic* KVP encoding (even=varint) emits
        // the desyncing two-byte 0x80 0x80 for priority 128, whereas the u8-typed
        // encoding used by SUBSCRIBE must emit a single 0x80.
        let mut params = KeyValuePairs::new();
        params.set_subscriber_priority(128); // 0x20 = 128

        let mut generic = BytesMut::new();
        params.encode(&mut generic).unwrap();
        assert_eq!(generic.to_vec(), vec![0x01, 0x20, 0x80, 0x80]);

        let mut typed = BytesMut::new();
        params
            .encode_with_u8_types(&mut typed, U8_VALUE_PARAMETER_TYPES)
            .unwrap();
        assert_eq!(typed.to_vec(), vec![0x01, 0x20, 0x80]);
    }

    #[test]
    fn subscribe_round_trips_default_priority_128() {
        // Insert in ascending key order (FORWARD 0x10, SUBSCRIBER_PRIORITY 0x20,
        // GROUP_ORDER 0x22) so this Vec matches the sorted order that decode
        // reconstructs — KeyValuePairs equality is order-sensitive.
        let mut params = KeyValuePairs::new();
        params.set_forward(true);
        params.set_subscriber_priority(128);
        params.set_group_order(GroupOrder::Ascending);

        let msg = Subscribe {
            id: 7,
            track_namespace: TrackNamespace::from_utf8_path("ns/v"),
            track_name: "track".into(),
            params,
        };

        let mut buf = BytesMut::new();
        msg.encode(&mut buf).unwrap();
        let decoded = Subscribe::decode(&mut buf).unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(decoded.params.subscriber_priority().unwrap(), Some(128));
    }
}
