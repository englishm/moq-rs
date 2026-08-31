// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// TODO: Unimplemented control message events (not yet needed for basic relay interop testing):
// - PublishNamespaceDone (parsed/created)
// - PublishNamespaceCancel (parsed/created)
// - TrackStatus (parsed/created)
// - RequestUpdate (parsed/created)
// - Fetch, FetchOk, FetchCancel (parsed/created)
// - Publish, PublishOk, PublishDone (parsed/created)
// Note: MaxRequestId/RequestsBlocked removed in draft-18 (#1471)
// - Namespace and NamespaceDone are implemented for SUBSCRIBE_NAMESPACE response streams.
//
// TODO: Unimplemented data plane events (from draft-pardue-moq-qlog-moq-events):
// - stream_type_set (when stream type becomes known)
// - object_datagram_status_created/parsed
// - fetch_header_created/parsed
// - fetch_object_created/parsed
//
// TODO: stream_id field currently uses placeholder value (0)
// - Need to plumb actual QUIC stream IDs through web_transport abstractions
// - This would enable correlation between QUIC qlog and MoQ mlog events

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};

use crate::{coding, data, message, setup};

/// MoQ Transport event following qlog patterns
#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Time in milliseconds since connection start
    pub time: f64,

    /// Event name in format "moqt:event_name"
    pub name: String,

    /// Event-specific data
    pub data: EventData,
}

/// Union of all MoQ Transport event types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type")]
pub enum EventData {
    #[serde(rename = "control_message_parsed")]
    ControlMessageParsed(ControlMessageParsed),

    #[serde(rename = "control_message_created")]
    ControlMessageCreated(ControlMessageCreated),

    #[serde(rename = "subgroup_header_parsed")]
    SubgroupHeaderParsed(SubgroupHeaderParsed),

    #[serde(rename = "subgroup_header_created")]
    SubgroupHeaderCreated(SubgroupHeaderCreated),

    #[serde(rename = "subgroup_object_parsed")]
    SubgroupObjectParsed(SubgroupObjectParsed),

    #[serde(rename = "subgroup_object_created")]
    SubgroupObjectCreated(SubgroupObjectCreated),

    #[serde(rename = "object_datagram_parsed")]
    ObjectDatagramParsed(ObjectDatagramParsed),

    #[serde(rename = "object_datagram_created")]
    ObjectDatagramCreated(ObjectDatagramCreated),

    #[serde(rename = "loglevel")]
    LogLevel(LogLevelEvent),
}

/// Control message parsed event (Section 4.2 of draft-pardue-moq-qlog-moq-events)
#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlMessageParsed {
    pub stream_id: u64,
    pub message_type: String,

    /// Message-specific fields
    #[serde(flatten)]
    pub message: JsonValue,
}

/// Control message created event (Section 4.1 of draft-pardue-moq-qlog-moq-events)
#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlMessageCreated {
    pub stream_id: u64,
    pub message_type: String,

    /// Message-specific fields
    #[serde(flatten)]
    pub message: JsonValue,
}

/// Subgroup header parsed event (data plane)
#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubgroupHeaderParsed {
    pub stream_id: u64,

    /// Header-specific fields
    #[serde(flatten)]
    pub header: JsonValue,
}

/// Subgroup header created event (data plane)
#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubgroupHeaderCreated {
    pub stream_id: u64,

    /// Header-specific fields
    #[serde(flatten)]
    pub header: JsonValue,
}

/// Subgroup object parsed event (data plane)
#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubgroupObjectParsed {
    pub stream_id: u64,

    /// Object-specific fields
    #[serde(flatten)]
    pub object: JsonValue,
}

/// Subgroup object created event (data plane)
#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubgroupObjectCreated {
    pub stream_id: u64,

    /// Object-specific fields
    #[serde(flatten)]
    pub object: JsonValue,
}

/// Object Datagram parsed event (data plane)
#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectDatagramParsed {
    pub stream_id: u64,

    /// Object-specific fields
    #[serde(flatten)]
    pub object: JsonValue,
}

/// Object Datagram created event (data plane)
#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectDatagramCreated {
    pub stream_id: u64,

    /// Object-specific fields
    #[serde(flatten)]
    pub object: JsonValue,
}

/// LogLevel event for flexible logging (qlog loglevel schema)
/// See: https://www.ietf.org/archive/id/draft-ietf-quic-qlog-main-schema-12.html#name-loglevel-events
#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLevelEvent {
    pub message: String,
}

// Helper functions to create vector of string pairs from KVPs
fn key_value_pairs_to_vec(kvps: &[coding::KeyValuePair]) -> Vec<(String, String)> {
    kvps.iter()
        .map(|kvp| (kvp.key.to_string(), format!("{:?}", kvp.value)))
        .collect()
}

fn create_control_message_event(
    time: f64,
    stream_id: u64,
    is_parsed: bool,
    msg_type: &str,
    message: JsonValue,
) -> Event {
    if is_parsed {
        Event {
            time,
            name: "moqt:control_message_parsed".to_string(),
            data: EventData::ControlMessageParsed(ControlMessageParsed {
                stream_id,
                message_type: msg_type.to_string(),
                message,
            }),
        }
    } else {
        Event {
            time,
            name: "moqt:control_message_created".to_string(),
            data: EventData::ControlMessageCreated(ControlMessageCreated {
                stream_id,
                message_type: msg_type.to_string(),
                message,
            }),
        }
    }
}

/// Create a control_message_parsed event for CLIENT_SETUP.
/// From draft-16 the setup payload carries only parameters; version is agreed via ALPN.
pub fn client_setup_parsed(time: f64, stream_id: u64, msg: &setup::Setup) -> Event {
    create_control_message_event(
        time,
        stream_id,
        true,
        "client_setup",
        json!({
            "parameters": key_value_pairs_to_vec(&msg.params.0),
        }),
    )
}

/// Create a control_message_created event for SERVER_SETUP.
/// From draft-16 the setup payload carries only parameters; version is agreed via ALPN.
pub fn server_setup_created(time: f64, stream_id: u64, msg: &setup::Setup) -> Event {
    create_control_message_event(
        time,
        stream_id,
        false,
        "server_setup",
        json!({
            "parameters": key_value_pairs_to_vec(&msg.params.0),
        }),
    )
}

/// Helper to convert SUBSCRIBE message to JSON
fn subscribe_to_json(msg: &message::Subscribe) -> JsonValue {
    json!({
        "subscribe_id": msg.id,
        "track_namespace": msg.track_namespace.to_string(),
        "track_name": msg.track_name.to_string(),
        "parameters": key_value_pairs_to_vec(&msg.params.0),
    })
}

/// Create a control_message_parsed event for SUBSCRIBE
pub fn subscribe_parsed(time: f64, stream_id: u64, msg: &message::Subscribe) -> Event {
    create_control_message_event(time, stream_id, true, "subscribe", subscribe_to_json(msg))
}

/// Create a control_message_created event for SUBSCRIBE
pub fn subscribe_created(time: f64, stream_id: u64, msg: &message::Subscribe) -> Event {
    create_control_message_event(time, stream_id, false, "subscribe", subscribe_to_json(msg))
}

/// Helper to convert SUBSCRIBE_OK message to JSON
fn subscribe_ok_to_json(msg: &message::SubscribeOk) -> JsonValue {
    json!({
        "subscribe_id": msg.id,
        "track_alias": msg.track_alias,
        "parameters": key_value_pairs_to_vec(&msg.params.0),
        "track_extensions": key_value_pairs_to_vec(&msg.track_extensions.0),
    })
}

/// Create a control_message_parsed event for SUBSCRIBE_OK
pub fn subscribe_ok_parsed(time: f64, stream_id: u64, msg: &message::SubscribeOk) -> Event {
    create_control_message_event(
        time,
        stream_id,
        true,
        "subscribe_ok",
        subscribe_ok_to_json(msg),
    )
}

/// Create a control_message_created event for SUBSCRIBE_OK
pub fn subscribe_ok_created(time: f64, stream_id: u64, msg: &message::SubscribeOk) -> Event {
    create_control_message_event(
        time,
        stream_id,
        false,
        "subscribe_ok",
        subscribe_ok_to_json(msg),
    )
}

/// Helper to convert PUBLISH_NAMESPACE message to JSON
fn publish_namespace_to_json(msg: &message::PublishNamespace) -> JsonValue {
    json!({
        "request_id": msg.id,
        "track_namespace": msg.track_namespace.to_string(),
        "parameters": key_value_pairs_to_vec(&msg.params.0),
    })
}

/// Create a control_message_parsed event for PUBLISH_NAMESPACE (was ANNOUNCE in earlier drafts)
pub fn publish_namespace_parsed(
    time: f64,
    stream_id: u64,
    msg: &message::PublishNamespace,
) -> Event {
    create_control_message_event(
        time,
        stream_id,
        true,
        "publish_namespace",
        publish_namespace_to_json(msg),
    )
}

/// Create a control_message_created event for PUBLISH_NAMESPACE
pub fn publish_namespace_created(
    time: f64,
    stream_id: u64,
    msg: &message::PublishNamespace,
) -> Event {
    create_control_message_event(
        time,
        stream_id,
        false,
        "publish_namespace",
        publish_namespace_to_json(msg),
    )
}

fn subscribe_namespace_to_json(msg: &message::SubscribeNamespace) -> JsonValue {
    json!({
        "request_id": msg.id,
        "track_namespace_prefix": msg.track_namespace_prefix.to_string(),
        "subscribe_options": format!("{:?}", msg.subscribe_options),
        "parameters": key_value_pairs_to_vec(&msg.params.0),
    })
}

/// Create a control_message_parsed event for SUBSCRIBE_NAMESPACE.
pub fn subscribe_namespace_parsed(
    time: f64,
    stream_id: u64,
    msg: &message::SubscribeNamespace,
) -> Event {
    create_control_message_event(
        time,
        stream_id,
        true,
        "subscribe_namespace",
        subscribe_namespace_to_json(msg),
    )
}

/// Create a control_message_created event for SUBSCRIBE_NAMESPACE.
pub fn subscribe_namespace_created(
    time: f64,
    stream_id: u64,
    msg: &message::SubscribeNamespace,
) -> Event {
    create_control_message_event(
        time,
        stream_id,
        false,
        "subscribe_namespace",
        subscribe_namespace_to_json(msg),
    )
}

fn namespace_to_json(msg: &message::Namespace) -> JsonValue {
    json!({
        "track_namespace_suffix": msg.track_namespace_suffix.to_string(),
    })
}

/// Create a control_message_parsed event for NAMESPACE.
pub fn namespace_parsed(time: f64, stream_id: u64, msg: &message::Namespace) -> Event {
    create_control_message_event(time, stream_id, true, "namespace", namespace_to_json(msg))
}

/// Create a control_message_created event for NAMESPACE.
pub fn namespace_created(time: f64, stream_id: u64, msg: &message::Namespace) -> Event {
    create_control_message_event(time, stream_id, false, "namespace", namespace_to_json(msg))
}

fn namespace_done_to_json(msg: &message::NamespaceDone) -> JsonValue {
    json!({
        "track_namespace_suffix": msg.track_namespace_suffix.to_string(),
    })
}

/// Create a control_message_parsed event for NAMESPACE_DONE.
pub fn namespace_done_parsed(time: f64, stream_id: u64, msg: &message::NamespaceDone) -> Event {
    create_control_message_event(
        time,
        stream_id,
        true,
        "namespace_done",
        namespace_done_to_json(msg),
    )
}

/// Create a control_message_created event for NAMESPACE_DONE.
pub fn namespace_done_created(time: f64, stream_id: u64, msg: &message::NamespaceDone) -> Event {
    create_control_message_event(
        time,
        stream_id,
        false,
        "namespace_done",
        namespace_done_to_json(msg),
    )
}

fn request_ok_to_json(request_kind: &str, msg: &message::RequestOk) -> JsonValue {
    json!({
        "request_id": msg.id,
        "request_kind": request_kind,
        "parameters": key_value_pairs_to_vec(&msg.params.0),
    })
}

/// Create a control_message_parsed event for REQUEST_OK.
pub fn request_ok_parsed(
    time: f64,
    stream_id: u64,
    request_kind: &str,
    msg: &message::RequestOk,
) -> Event {
    create_control_message_event(
        time,
        stream_id,
        true,
        "request_ok",
        request_ok_to_json(request_kind, msg),
    )
}

/// Create a control_message_created event for REQUEST_OK.
pub fn request_ok_created(
    time: f64,
    stream_id: u64,
    request_kind: &str,
    msg: &message::RequestOk,
) -> Event {
    create_control_message_event(
        time,
        stream_id,
        false,
        "request_ok",
        request_ok_to_json(request_kind, msg),
    )
}

fn request_error_to_json(request_kind: &str, msg: &message::RequestError) -> JsonValue {
    json!({
        "request_id": msg.id,
        "request_kind": request_kind,
        "error_code": msg.error_code,
        "retry_interval": msg.retry_interval,
        "reason_phrase": &msg.reason.0,
    })
}

/// Create a control_message_parsed event for REQUEST_ERROR.
pub fn request_error_parsed(
    time: f64,
    stream_id: u64,
    request_kind: &str,
    msg: &message::RequestError,
) -> Event {
    create_control_message_event(
        time,
        stream_id,
        true,
        "request_error",
        request_error_to_json(request_kind, msg),
    )
}

/// Create a control_message_created event for REQUEST_ERROR.
pub fn request_error_created(
    time: f64,
    stream_id: u64,
    request_kind: &str,
    msg: &message::RequestError,
) -> Event {
    create_control_message_event(
        time,
        stream_id,
        false,
        "request_error",
        request_error_to_json(request_kind, msg),
    )
}

/// Create a control_message_parsed event for UNSUBSCRIBE
pub fn unsubscribe_parsed(time: f64, stream_id: u64, msg: &message::Unsubscribe) -> Event {
    create_control_message_event(
        time,
        stream_id,
        true,
        "unsubscribe",
        json!({
            "subscribe_id": msg.id,
        }),
    )
}

/// Create a control_message_created event for UNSUBSCRIBE
pub fn unsubscribe_created(time: f64, stream_id: u64, msg: &message::Unsubscribe) -> Event {
    create_control_message_event(
        time,
        stream_id,
        false,
        "unsubscribe",
        json!({
            "subscribe_id": msg.id,
        }),
    )
}

/// Create a control_message_parsed event for GOAWAY
pub fn go_away_parsed(time: f64, stream_id: u64, msg: &message::GoAway) -> Event {
    create_control_message_event(
        time,
        stream_id,
        true,
        "goaway",
        json!({
                    "new_session_uri": &msg.uri.0,
        }),
    )
}

/// Create a control_message_created event for GOAWAY
pub fn go_away_created(time: f64, stream_id: u64, msg: &message::GoAway) -> Event {
    create_control_message_event(
        time,
        stream_id,
        false,
        "goaway",
        json!({
                    "new_session_uri": &msg.uri.0,
        }),
    )
}

// Data plane events

/// Helper to convert SubgroupHeader to JSON
fn subgroup_header_to_json(header: &data::SubgroupHeader) -> JsonValue {
    let mut json = json!({
        "header_type": format!("{:?}", header.header_type),
        "header_type_value": header.header_type.value(),
        "track_alias": header.track_alias,
        "group_id": header.group_id,
        "publisher_priority": header.publisher_priority,
        "has_properties": header.header_type.has_properties(),
        "end_of_group": header.header_type.end_of_group(),
        "default_priority": header.header_type.uses_default_priority(),
        "first_object": header.header_type.begins_with_first_object(),
    });

    if let Some(subgroup_id) = header.subgroup_id {
        json["subgroup_id"] = json!(subgroup_id);
    }

    json
}

/// Create a subgroup_header_parsed event
pub fn subgroup_header_parsed(time: f64, stream_id: u64, header: &data::SubgroupHeader) -> Event {
    Event {
        time,
        name: "moqt:subgroup_header_parsed".to_string(),
        data: EventData::SubgroupHeaderParsed(SubgroupHeaderParsed {
            stream_id,
            header: subgroup_header_to_json(header),
        }),
    }
}

/// Create a subgroup_header_created event
pub fn subgroup_header_created(time: f64, stream_id: u64, header: &data::SubgroupHeader) -> Event {
    Event {
        time,
        name: "moqt:subgroup_header_created".to_string(),
        data: EventData::SubgroupHeaderCreated(SubgroupHeaderCreated {
            stream_id,
            header: subgroup_header_to_json(header),
        }),
    }
}

/// Helper to convert SubgroupObject to JSON
fn subgroup_object_to_json(
    group_id: u64,
    subgroup_id: u64,
    object_id: u64,
    object: &data::SubgroupObject,
) -> JsonValue {
    let mut object_data = json!({
        "group_id": group_id,
        "subgroup_id": subgroup_id,
        "object_id": object_id,
        // TODO send object_playload itself
        "object_payload_length": object.payload_length,
    });

    if let Some(status) = object.status {
        object_data["object_status"] = json!(format!("{:?}", status));
    }

    object_data
}

/// Create a subgroup_object_parsed event
pub fn subgroup_object_parsed(
    time: f64,
    stream_id: u64,
    group_id: u64,
    subgroup_id: u64,
    object_id: u64,
    object: &data::SubgroupObject,
) -> Event {
    Event {
        time,
        name: "moqt:subgroup_object_parsed".to_string(),
        data: EventData::SubgroupObjectParsed(SubgroupObjectParsed {
            stream_id,
            object: subgroup_object_to_json(group_id, subgroup_id, object_id, object),
        }),
    }
}

/// Create a subgroup_object_created event
pub fn subgroup_object_created(
    time: f64,
    stream_id: u64,
    group_id: u64,
    subgroup_id: u64,
    object_id: u64,
    object: &data::SubgroupObject,
) -> Event {
    Event {
        time,
        name: "moqt:subgroup_object_created".to_string(),
        data: EventData::SubgroupObjectCreated(SubgroupObjectCreated {
            stream_id,
            object: subgroup_object_to_json(group_id, subgroup_id, object_id, object),
        }),
    }
}

/// Helper to convert SubgroupObject to JSON
fn subgroup_object_ext_to_json(
    group_id: u64,
    subgroup_id: u64,
    object_id: u64,
    object: &data::SubgroupObjectExt,
) -> JsonValue {
    let mut object_data = json!({
        "group_id": group_id,
        "subgroup_id": subgroup_id,
        "object_id": object_id,
        "extension_headers": key_value_pairs_to_vec(&object.extension_headers.0),
        // TODO send object_playload itself
        "object_payload_length": object.payload_length,
    });

    if let Some(status) = object.status {
        object_data["object_status"] = json!(format!("{:?}", status));
    }

    object_data
}

/// Create a subgroup_object_parsed event (with extensions)
pub fn subgroup_object_ext_parsed(
    time: f64,
    stream_id: u64,
    group_id: u64,
    subgroup_id: u64,
    object_id: u64,
    object: &data::SubgroupObjectExt,
) -> Event {
    Event {
        time,
        name: "moqt:subgroup_object_parsed".to_string(),
        data: EventData::SubgroupObjectParsed(SubgroupObjectParsed {
            stream_id,
            object: subgroup_object_ext_to_json(group_id, subgroup_id, object_id, object),
        }),
    }
}

/// Create a subgroup_object_created event (with extensions)
pub fn subgroup_object_ext_created(
    time: f64,
    stream_id: u64,
    group_id: u64,
    subgroup_id: u64,
    object_id: u64,
    object: &data::SubgroupObjectExt,
) -> Event {
    Event {
        time,
        name: "moqt:subgroup_object_created".to_string(),
        data: EventData::SubgroupObjectCreated(SubgroupObjectCreated {
            stream_id,
            object: subgroup_object_ext_to_json(group_id, subgroup_id, object_id, object),
        }),
    }
}

/// Helper to convert Datagram to JSON
fn object_datagram_to_json(datagram: &data::Datagram) -> JsonValue {
    let mut json = json!({
        "datagram_type": format!("{:?}", datagram.datagram_type),
        "track_alias": datagram.track_alias,
        "group_id": datagram.group_id,
        "object_id": datagram.object_id.unwrap_or(0),
        "publisher_priority": datagram.publisher_priority,
        // TODO send object_playload
        "payload_length": datagram.payload.as_ref().map_or(0, |p| p.len()),
    });

    if let Some(extension_headers) = &datagram.extension_headers {
        json["extension_headers"] = json!(key_value_pairs_to_vec(&extension_headers.0));
    }

    if let Some(status) = datagram.status {
        json["object_status"] = json!(format!("{:?}", status));
    }

    json
}

/// Create a object_datagram_parsed event
pub fn object_datagram_parsed(time: f64, stream_id: u64, datagram: &data::Datagram) -> Event {
    Event {
        time,
        name: "moqt:object_datagram_parsed".to_string(),
        data: EventData::ObjectDatagramParsed(ObjectDatagramParsed {
            stream_id,
            object: object_datagram_to_json(datagram),
        }),
    }
}

/// Create a object_datagram_created event
pub fn object_datagram_created(time: f64, stream_id: u64, datagram: &data::Datagram) -> Event {
    Event {
        time,
        name: "moqt:object_datagram_created".to_string(),
        data: EventData::ObjectDatagramCreated(ObjectDatagramCreated {
            stream_id,
            object: object_datagram_to_json(datagram),
        }),
    }
}

// LogLevel events (generic logging)

/// Log levels for qlog loglevel events
#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Fatal,
    Error,
    Warn,
    Info,
    Debug,
    Verbose,
}

impl LogLevel {
    fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Fatal => "fatal",
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Verbose => "verbose",
        }
    }
}

/// Create a loglevel event for flexible logging
///
/// # Arguments
/// * `time` - Timestamp in milliseconds since connection start
/// * `level` - Log level (debug, info, warn, error, fatal, verbose)
/// * `message` - Freeform message text with structured information
///
/// # Example
/// ```ignore
/// loglevel_event(
///     12.345,
///     LogLevel::Debug,
///     "object_queued: track_alias=1 group=5 subgroup=2 object=10 payload_len=1024"
/// )
/// ```
pub fn loglevel_event(time: f64, level: LogLevel, message: String) -> Event {
    Event {
        time,
        name: format!("loglevel:{}", level.as_str()),
        data: EventData::LogLevel(LogLevelEvent { message }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding::{KeyValuePairs, TrackNamespacePrefix};
    use crate::{
        data::{StreamHeaderType, SubgroupHeader, SubgroupIdMode},
        message::{SubscribeNamespace, SubscribeOptions},
    };

    #[test]
    fn subscribe_namespace_event_includes_prefix_and_options() {
        let msg = SubscribeNamespace {
            id: 10,
            track_namespace_prefix: TrackNamespacePrefix::from_utf8_path("example.com/meeting"),
            subscribe_options: SubscribeOptions::Both,
            params: KeyValuePairs::default(),
        };

        let event = subscribe_namespace_created(1.0, 7, &msg);

        let EventData::ControlMessageCreated(data) = event.data else {
            panic!("expected control_message_created event");
        };
        assert_eq!(data.stream_id, 7);
        assert_eq!(data.message_type, "subscribe_namespace");
        assert_eq!(data.message["request_id"], 10);
        assert_eq!(
            data.message["track_namespace_prefix"],
            "/example.com/meeting"
        );
        assert_eq!(data.message["subscribe_options"], "Both");
    }

    #[test]
    fn subgroup_header_event_preserves_legacy_name_and_exposes_modifiers() {
        let legacy = subgroup_header_to_json(&SubgroupHeader {
            header_type: StreamHeaderType::SubgroupIdExt,
            track_alias: 1,
            group_id: 2,
            subgroup_id: Some(3),
            publisher_priority: 4,
        });
        assert_eq!(legacy["header_type"], "SubgroupIdExt");
        assert_eq!(legacy["header_type_value"], 0x15);

        let modified = subgroup_header_to_json(&SubgroupHeader {
            header_type: StreamHeaderType::subgroup(SubgroupIdMode::Zero, false, true, true, true),
            track_alias: 1,
            group_id: 2,
            subgroup_id: None,
            publisher_priority: 200,
        });
        assert_eq!(
            modified["header_type"],
            "SubgroupZeroIdEndOfGroupDefaultPriorityFirstObject"
        );
        assert_eq!(modified["header_type_value"], 0x78);
        assert_eq!(modified["end_of_group"], true);
        assert_eq!(modified["default_priority"], true);
        assert_eq!(modified["first_object"], true);
    }
}
