// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Control messages sent over the bidirectional control stream.
//!
//! Wire format per draft-ietf-moq-transport-16 §9:
//!
//! ```text
//! MOQT Control Message {
//!   Message Type (i),
//!   Message Length (16),   ← 16-bit unsigned big-endian
//!   Message Payload (..),
//! }
//! ```
//!
//! The receiver MUST close the session with PROTOCOL_VIOLATION if the
//! payload length does not match Message Length.  Unknown message types
//! MUST also close the session.

mod fetch;
mod fetch_cancel;
mod fetch_ok;
mod fetch_type;
mod filter_type;
mod go_away;
mod group_order;

mod namespace;
mod params;
mod pubilsh_namespace_done;
mod publish;
mod publish_done;
mod publish_namespace;
mod publish_namespace_cancel;
mod publish_ok;
mod publisher;
mod request_error;
mod request_ok;
mod request_update;

mod subscribe;
mod subscribe_namespace;
mod subscribe_ok;
mod subscriber;
mod track_status;
mod unsubscribe;

pub use fetch::*;
pub use fetch_cancel::*;
pub use fetch_ok::*;
pub use fetch_type::*;
pub use filter_type::*;
pub use go_away::*;
pub use group_order::*;

pub use namespace::*;
pub use params::*;
pub use pubilsh_namespace_done::*;
pub use publish::*;
pub use publish_done::*;
pub use publish_namespace::*;
pub use publish_namespace_cancel::*;
pub use publish_ok::*;
pub use publisher::*;
pub use request_error::*;
pub use request_ok::*;
pub use request_update::*;

pub use subscribe::*;
pub use subscribe_namespace::*;
pub use subscribe_ok::*;
pub use subscriber::*;
pub use track_status::*;
pub use unsubscribe::*;

use crate::coding::{Decode, DecodeError, Encode, EncodeError};
use bytes::Buf as _;
use std::fmt;

// Use a macro to generate the Message enum and its encode/decode impls.
macro_rules! message_types {
    {$($name:ident = $val:expr,)*} => {
        /// Wire type IDs for control messages (draft-18 Table 5).
        ///
        /// These are the `u64` values used in the `Message Type` field on
        /// the wire. Use these constants instead of hardcoded hex literals
        /// when matching or constructing message frames.
        #[allow(non_upper_case_globals)]
        pub mod wire_id {
            $(pub const $name: u64 = $val;)*
        }

        /// All supported control message types.
        #[derive(Clone)]
        pub enum Message {
            $($name($name)),*
        }

        impl Decode for Message {
            fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
                let t = u64::decode(r)?;
                let len = u16::decode(r)? as usize;

                // Enforce the length field: read exactly `len` bytes as the
                // payload and decode from that slice, so a truncated or
                // overlong payload is detected immediately.
                <u64 as Decode>::decode_remaining(r, len)?;
                let mut payload = r.copy_to_bytes(len);

                let msg = match t {
                    $($val => {
                        let msg = $name::decode(&mut payload)?;
                        Ok(Self::$name(msg))
                    })*
                    _ => Err(DecodeError::InvalidMessage(t)),
                }?;

                // Any bytes left in the payload slice mean the message was
                // shorter than declared — that is a PROTOCOL_VIOLATION.
                if payload.has_remaining() {
                    return Err(DecodeError::InvalidMessage(t));
                }

                if !msg.parameter_scopes_valid() {
                    return Err(DecodeError::InvalidParameter);
                }

                Ok(msg)
            }
        }

        impl Encode for Message {
            fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
                if !self.parameter_scopes_valid() {
                    return Err(EncodeError::InvalidValue);
                }

                match self {
                    $(Self::$name(ref m) => {
                        self.id().encode(w)?;

                        let mut buf = Vec::new();
                        m.encode(&mut buf)?;
                        if buf.len() > u16::MAX as usize {
                            return Err(EncodeError::MsgBoundsExceeded);
                        }
                        (buf.len() as u16).encode(w)?;

                        Self::encode_remaining(w, buf.len())?;
                        w.put_slice(&buf);
                        Ok(())
                    },)*
                }
            }
        }

        impl Message {
            pub fn id(&self) -> u64 {
                match self {
                    $(Self::$name(_) => $val,)*
                }
            }

            pub fn name(&self) -> &'static str {
                match self {
                    $(Self::$name(_) => stringify!($name),)*
                }
            }

            pub(crate) fn parameter_scopes_valid(&self) -> bool {
                let params = match self {
                    Self::RequestUpdate(m) => &m.params,
                    Self::RequestOk(m) => &m.params,
                    Self::Subscribe(m) => &m.params,
                    Self::SubscribeOk(m) => &m.params,
                    Self::PublishNamespace(m) => &m.params,
                    Self::TrackStatus(m) => &m.params,
                    Self::Publish(m) => &m.params,
                    Self::PublishOk(m) => &m.params,
                    Self::Fetch(m) => &m.params,
                    Self::FetchOk(m) => &m.params,
                    Self::SubscribeNamespace(m) => &m.params,
                    _ => return true,
                };

                params.parameters_allowed_on(self.id())
            }

            /// Return the request ID if this message participates in request ID sequencing.
            ///
            /// Responses and cancellation messages reference existing request IDs
            /// and therefore return `None`. This is used only for request ID
            /// sequencing validation on receive.
            pub fn sequenced_request_id(&self) -> Option<u64> {
                match self {
                    Self::Subscribe(m) => Some(m.id),
                    Self::RequestUpdate(m) => Some(m.id),
                    Self::Fetch(m) => Some(m.id),
                    Self::TrackStatus(m) => Some(m.id),
                    Self::SubscribeNamespace(m) => Some(m.id),
                    Self::Publish(m) => Some(m.id),
                    Self::PublishNamespace(m) => Some(m.id),
                    _ => None,
                }
            }

            /// Return the target request ID for response/follow-up messages sent
            /// back on a bidi stream (draft-18). Returns `None` for request-initiating
            /// messages and session-level messages.
            pub fn response_target_id(&self) -> Option<u64> {
                match self {
                    Self::RequestOk(m) => Some(m.id),
                    Self::RequestError(m) => Some(m.id),
                    Self::SubscribeOk(m) => Some(m.id),
                    Self::PublishDone(m) => Some(m.id),
                    Self::PublishNamespaceDone(m) => Some(m.id),
                    Self::Unsubscribe(m) => Some(m.id),
                    Self::PublishOk(m) => Some(m.id),
                    Self::FetchOk(m) => Some(m.id),
                    _ => None,
                }
            }
        }

        $(impl From<$name> for Message {
            fn from(m: $name) -> Self {
                Message::$name(m)
            }
        })*

        impl fmt::Debug for Message {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $(Self::$name(ref m) => m.fmt(f),)*
                }
            }
        }
    }
}

// Wire IDs per draft-ietf-moq-transport-16 Table 1.
message_types! {
    // NOTE: Setup messages live in a separate module (setup::Client/Server).

    // ── Shared request responses (new in draft-16) ───────────────────────────
    RequestUpdate   = 0x2,
    RequestError    = 0x5,   // draft-16: REQUEST_ERROR
    RequestOk       = 0x7,   // draft-16: REQUEST_OK

    // ── SUBSCRIBE family ─────────────────────────────────────────────────────
    Subscribe       = 0x3,
    SubscribeOk     = 0x4,
    Unsubscribe     = 0xa,

    // ── PUBLISH_NAMESPACE family ──────────────────────────────────────────────
    PublishNamespace        = 0x6,
    Namespace               = 0x8,
    PublishNamespaceDone    = 0x9,
    NamespaceDone           = 0xe,
    PublishNamespaceCancel  = 0xc,

    // ── TRACK_STATUS ──────────────────────────────────────────────────────────
    TrackStatus     = 0xd,

    // ── PUBLISH family ────────────────────────────────────────────────────────
    Publish         = 0x1d,
    PublishDone     = 0xb,
    PublishOk       = 0x1e,

    // ── FETCH family ─────────────────────────────────────────────────────────
    Fetch           = 0x16,
    FetchCancel     = 0x17,
    FetchOk         = 0x18,

    // ── SUBSCRIBE_NAMESPACE (bidi stream; §10.18) ────────────────────────────
    SubscribeNamespace = 0x50,

    // ── Session management ────────────────────────────────────────────────────
    GoAway          = 0x10,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding::{
        KeyValuePairs, Location, ReasonPhrase, TrackNamespace, TrackNamespacePrefix,
    };

    fn namespace() -> TrackNamespace {
        TrackNamespace::from_utf8_path("test/ns")
    }

    fn assert_sequenced(msg: Message, id: u64) {
        assert_eq!(msg.sequenced_request_id(), Some(id));
    }

    fn assert_not_sequenced(msg: Message) {
        assert_eq!(msg.sequenced_request_id(), None);
    }

    #[test]
    fn sequenced_request_id_covers_all_request_start_messages() {
        assert_sequenced(
            Message::Subscribe(Subscribe {
                id: 0,
                track_namespace: namespace(),
                track_name: "track".into(),
                params: KeyValuePairs::default(),
            }),
            0,
        );

        assert_sequenced(
            Message::RequestUpdate(RequestUpdate {
                id: 2,
                existing_request_id: 0,
                params: KeyValuePairs::default(),
            }),
            2,
        );

        assert_sequenced(
            Message::Fetch(Fetch {
                id: 4,
                fetch_type: FetchType::Standalone,
                standalone_fetch: Some(StandaloneFetch {
                    track_namespace: namespace(),
                    track_name: "track".into(),
                    start_location: Location::new(0, 0),
                    end_location: Location::new(0, 1),
                }),
                joining_fetch: None,
                params: KeyValuePairs::default(),
            }),
            4,
        );

        assert_sequenced(
            Message::TrackStatus(TrackStatus {
                id: 6,
                track_namespace: namespace(),
                track_name: "track".into(),
                params: KeyValuePairs::default(),
            }),
            6,
        );

        assert_sequenced(
            Message::SubscribeNamespace(SubscribeNamespace {
                id: 8,
                track_namespace_prefix: TrackNamespacePrefix::from_utf8_path("test/ns"),
                subscribe_options: SubscribeOptions::Both,
                params: KeyValuePairs::default(),
            }),
            8,
        );

        assert_sequenced(
            Message::Publish(Publish {
                id: 10,
                track_namespace: namespace(),
                track_name: "track".into(),
                track_alias: 1,
                params: KeyValuePairs::default(),
                track_extensions: TrackExtensions::default(),
            }),
            10,
        );

        assert_sequenced(
            Message::PublishNamespace(PublishNamespace {
                id: 12,
                track_namespace: namespace(),
                params: KeyValuePairs::default(),
            }),
            12,
        );
    }

    #[test]
    fn sequenced_request_id_ignores_messages_that_reference_existing_requests() {
        assert_not_sequenced(Message::RequestOk(RequestOk {
            id: 0,
            params: KeyValuePairs::default(),
        }));

        assert_not_sequenced(Message::RequestError(RequestError {
            id: 0,
            error_code: 0,
            retry_interval: 0,
            reason: ReasonPhrase(String::new()),
        }));

        assert_not_sequenced(Message::SubscribeOk(SubscribeOk {
            id: 0,
            track_alias: 1,
            params: KeyValuePairs::default(),
            track_extensions: TrackExtensions::default(),
        }));

        assert_not_sequenced(Message::Unsubscribe(Unsubscribe { id: 0 }));

        assert_not_sequenced(Message::FetchCancel(FetchCancel { id: 0 }));

        assert_not_sequenced(Message::FetchOk(FetchOk {
            id: 0,
            end_of_track: false,
            end_location: Location::new(0, 0),
            params: KeyValuePairs::default(),
            track_extensions: TrackExtensions::default(),
        }));

        assert_not_sequenced(Message::PublishOk(PublishOk {
            id: 0,
            params: KeyValuePairs::default(),
        }));

        assert_not_sequenced(Message::PublishDone(PublishDone {
            id: 0,
            status_code: 0,
            stream_count: 0,
            reason: ReasonPhrase(String::new()),
        }));
    }

    #[test]
    fn decode_rejects_legacy_stub_message_type() {
        let mut buf = bytes::BytesMut::new();
        0x100u64.encode(&mut buf).unwrap();
        0u16.encode(&mut buf).unwrap();

        let err = Message::decode(&mut buf).unwrap_err();
        assert!(matches!(err, DecodeError::InvalidMessage(0x100)));
    }

    #[test]
    fn message_boundary_enforces_parameter_scope() {
        let mut params = KeyValuePairs::default();
        params.set_rendezvous_timeout(1_000);
        let request_ok = RequestOk { id: 1, params };

        let mut encoded = bytes::BytesMut::new();
        let err = Message::RequestOk(request_ok.clone())
            .encode(&mut encoded)
            .unwrap_err();
        assert!(matches!(err, EncodeError::InvalidValue));

        encoded.clear();
        let mut payload = bytes::BytesMut::new();
        request_ok.encode(&mut payload).unwrap();
        wire_id::RequestOk.encode(&mut encoded).unwrap();
        (payload.len() as u16).encode(&mut encoded).unwrap();
        encoded.extend_from_slice(&payload);

        let err = Message::decode(&mut encoded).unwrap_err();
        assert!(matches!(err, DecodeError::InvalidParameter));

        let mut params = KeyValuePairs::default();
        params.set_rendezvous_timeout(1_000);
        params.set_bytesvalue(123, vec![1, 2, 3]);
        let subscribe = Message::Subscribe(Subscribe {
            id: 2,
            track_namespace: namespace(),
            track_name: "track".into(),
            params,
        });

        encoded.clear();
        subscribe.encode(&mut encoded).unwrap();
        let Message::Subscribe(decoded) = Message::decode(&mut encoded).unwrap() else {
            panic!("expected SUBSCRIBE");
        };
        assert_eq!(decoded.params.rendezvous_timeout().unwrap(), Some(1_000));
        assert!(decoded.params.get(123).is_some());
    }

    #[test]
    fn draft18_wire_layouts_for_control_messages() {
        fn encoded(msg: Message) -> Vec<u8> {
            let mut buf = bytes::BytesMut::new();
            msg.encode(&mut buf).unwrap();
            buf.to_vec()
        }

        let ns = TrackNamespace::from_utf8_path("ns");
        let prefix = TrackNamespacePrefix::new();

        assert_eq!(
            encoded(Message::Subscribe(Subscribe {
                id: 0,
                track_namespace: ns.clone(),
                track_name: "t".into(),
                params: KeyValuePairs::default(),
            })),
            vec![0x03, 0x00, 0x08, 0x00, 0x01, 0x02, b'n', b's', 0x01, b't', 0x00]
        );

        assert_eq!(
            encoded(Message::SubscribeOk(SubscribeOk {
                id: 0,
                track_alias: 1,
                params: KeyValuePairs::default(),
                track_extensions: TrackExtensions::default(),
            })),
            vec![0x04, 0x00, 0x03, 0x00, 0x01, 0x00]
        );

        assert_eq!(
            encoded(Message::TrackStatus(TrackStatus {
                id: 0,
                track_namespace: ns.clone(),
                track_name: "t".into(),
                params: KeyValuePairs::default(),
            })),
            vec![0x0d, 0x00, 0x08, 0x00, 0x01, 0x02, b'n', b's', 0x01, b't', 0x00]
        );

        assert_eq!(
            encoded(Message::Publish(Publish {
                id: 0,
                track_namespace: ns.clone(),
                track_name: "t".into(),
                track_alias: 5,
                params: KeyValuePairs::default(),
                track_extensions: TrackExtensions::default(),
            })),
            vec![0x1d, 0x00, 0x09, 0x00, 0x01, 0x02, b'n', b's', 0x01, b't', 0x05, 0x00]
        );

        assert_eq!(
            encoded(Message::PublishOk(PublishOk {
                id: 0,
                params: KeyValuePairs::default(),
            })),
            vec![0x1e, 0x00, 0x02, 0x00, 0x00]
        );

        assert_eq!(
            encoded(Message::Fetch(Fetch {
                id: 0,
                fetch_type: FetchType::Standalone,
                standalone_fetch: Some(StandaloneFetch {
                    track_namespace: ns,
                    track_name: "t".into(),
                    start_location: Location::new(0, 0),
                    end_location: Location::new(0, 1),
                }),
                joining_fetch: None,
                params: KeyValuePairs::default(),
            })),
            vec![
                0x16, 0x00, 0x0d, 0x00, 0x01, 0x01, 0x02, b'n', b's', 0x01, b't', 0x00, 0x00, 0x00,
                0x01, 0x00
            ]
        );

        assert_eq!(
            encoded(Message::FetchOk(FetchOk {
                id: 0,
                end_of_track: false,
                end_location: Location::new(0, 1),
                params: KeyValuePairs::default(),
                track_extensions: TrackExtensions::default(),
            })),
            vec![0x18, 0x00, 0x05, 0x00, 0x00, 0x00, 0x01, 0x00]
        );

        assert_eq!(
            encoded(Message::SubscribeNamespace(SubscribeNamespace {
                id: 0,
                track_namespace_prefix: prefix,
                subscribe_options: SubscribeOptions::Both,
                params: KeyValuePairs::default(),
            })),
            vec![0x50, 0x00, 0x04, 0x00, 0x00, 0x02, 0x00]
        );
    }
}
