// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

mod error;
mod pending_requests;
mod publish_namespace;
mod publish_received;
mod published;
mod published_namespace;
mod publisher;
mod reader;
mod request_id;
mod session_id;
mod subscribe;
mod subscribe_namespace;
mod subscribed;
mod subscribed_namespace;
mod subscriber;
mod track_status_requested;
mod writer;

pub use error::*;
pub(crate) use pending_requests::{PendingRequest, PendingRequests, PendingResponse};
pub use publish_namespace::*;
pub use publish_received::PublishReceived;
pub(crate) use publish_received::PublishReceivedRecv;
pub(crate) use published::{split_published_state, PublishedRecv};
pub use published::{Published, PublishedInfo};
pub use published_namespace::*;
pub use publisher::*;
pub use request_id::{RequestId, RequestIdAllocation};
pub use session_id::SessionId;
pub use subscribe::*;
pub use subscribe_namespace::*;
pub use subscribed::*;
pub use subscribed_namespace::*;
pub use subscriber::*;
pub use track_status_requested::*;

use reader::*;
use writer::*;

use futures::{stream::FuturesUnordered, StreamExt};
use request_id::max_request_id_from_params;
use std::sync::{Arc, Mutex};

use crate::coding::{KeyValuePairs, Value};
use crate::message::Message;
use crate::mlog;
use crate::watch::Queue;
use crate::{message, setup};
use std::path::PathBuf;

fn add_mlog_event<F>(mlog: &Option<Arc<Mutex<mlog::MlogWriter>>>, make_event: F)
where
    F: FnOnce(f64) -> mlog::Event,
{
    if let Some(mlog) = mlog {
        if let Ok(mut mlog) = mlog.lock() {
            let event = make_event(mlog.elapsed_ms());
            let _ = mlog.add_event(event);
        }
    }
}

/// `fmt::Display` adapter that sanitizes peer-supplied strings for logging.
///
/// Replaces every Unicode control character (Rust's `char::is_control()`, which
/// covers U+0000–U+001F, U+007F, and the C1 range U+0080–U+009F) and the Unicode
/// line/paragraph separators U+2028/U+2029 with `?` to prevent log-injection
/// attacks (CWE-117). Other codepoints are passed through unchanged. Pair with
/// [`HexDisplay`] to retain the exact raw bytes.
struct SanitizedDisplay<'a>(&'a str);

impl std::fmt::Display for SanitizedDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for c in self.0.chars() {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                write!(f, "?")?;
            } else {
                write!(f, "{c}")?;
            }
        }
        Ok(())
    }
}

/// `fmt::Display` adapter that encodes a peer-supplied string as lowercase hex.
///
/// Retains every raw byte of the original value so analysts can reconstruct
/// exactly what the peer sent, even if [`SanitizedDisplay`] replaced some
/// characters with `?`.
struct HexDisplay<'a>(&'a str);

impl std::fmt::Display for HexDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0.bytes() {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// The transport protocol negotiated for this MoQT connection.
///
/// MoQT can run over either WebTransport (HTTP/3 + QUIC) or raw QUIC.
/// The transport type affects protocol behavior — for example, the PATH
/// parameter is only sent in CLIENT_SETUP for raw QUIC connections,
/// since WebTransport carries the path in the HTTP/3 CONNECT URL.
///
/// This enum is intentionally extensible for future transport options
/// (e.g., QMUX, WebSocket fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// WebTransport over HTTP/3 (RFC 9220).
    /// ALPN: "h3". Path carried in HTTP/3 CONNECT :path pseudo-header.
    WebTransport,
    /// Raw QUIC with MoQT framing directly on QUIC streams.
    /// ALPN: "moqt-16". Path carried in CLIENT_SETUP PATH parameter.
    RawQuic,
}

const DEFAULT_MAX_REQUEST_ID: u64 = 100;

/// Maximum number of concurrently accepted inbound SUBSCRIBE_NAMESPACE request
/// streams. Backpressures `accept_bi()` so a peer cannot exhaust memory by opening
/// unbounded bidirectional streams (draft-16 §3.3/§9.25).
const MAX_CONCURRENT_SUBSCRIBE_NAMESPACE_STREAMS: usize = 256;

/// Maximum time to wait for the SUBSCRIBE_NAMESPACE header on a freshly accepted
/// bidirectional stream before treating the peer as misbehaving. Prevents idle or
/// malicious streams from occupying an accept slot indefinitely.
const SUBSCRIBE_NAMESPACE_HEADER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Session-level protocol limits advertised during setup.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct SessionConfig {
    /// Maximum request ID plus one that we advertise to the peer.
    pub max_request_id: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_request_id: DEFAULT_MAX_REQUEST_ID,
        }
    }
}

/// Session object for managing all communications in a single QUIC connection.
#[must_use = "run() must be called"]
pub struct Session {
    webtransport: web_transport::Session,

    /// Control Stream Reader and Writer (QUIC bi-directional stream)
    sender: Writer, // Control Stream Sender
    recver: Reader, // Control Stream Receiver

    publisher: Option<Publisher>, // Contains Publisher side logic, uses outgoing message queue to send control messages
    subscriber: Option<Subscriber>, // Contains Subscriber side logic, uses outgoing message queue to send control messages

    /// Queue used by Publisher and Subscriber for sending Control Messages
    outgoing: Queue<Message>,

    /// Queue used by Subscriber to request opening SUBSCRIBE_NAMESPACE bidi streams.
    subscribe_namespace_open: Queue<OpenSubscribeNamespace>,

    /// Session-level request ID manager.
    /// Publisher and Subscriber share one outbound request ID sequence.
    request_id: RequestId,

    /// Outbound requests that are waiting for a terminal response.
    pending_requests: PendingRequests,

    /// Optional mlog writer for MoQ Transport events
    /// Wrapped in Arc<Mutex<>> to share across send/recv tasks when enabled
    mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,

    /// The transport protocol negotiated for this connection.
    transport: Transport,

    /// The connection path, derived from the WebTransport URL path or CLIENT_SETUP PATH parameter.
    /// For incoming connections: extracted during accept() from the WebTransport CONNECT URL
    /// (takes precedence) or the CLIENT_SETUP PATH parameter (key 0x1).
    /// For outgoing connections: auto-extracted from the session URL in connect().
    connection_path: Option<String>,

    /// Correlation id for this session, tagged onto every session-scoped log record.
    /// Normally the QUIC connection ID hex, which the peer also observes and which names this
    /// connection's qlog and mlog files.
    session_id: SessionId,
}

impl Session {
    const MAX_CONNECTION_PATH_LEN: usize = 1024;

    fn log_peer_max_request_id(session_id: &SessionId, peer_max: u64) {
        if peer_max == 0 {
            tracing::warn!(
                target: "moq_transport::control",
                session_id = %session_id,
                "peer MAX_REQUEST_ID is 0; outbound requests are disabled until MAX_REQUEST_ID increases"
            );
        }
    }

    /// Normalize and validate a connection path.
    ///
    /// Returns `Ok(None)` for empty or root-only paths. Returns `Err` for
    /// paths that are too long, don't start with `/`, contain empty,
    /// dot, or percent-encoded segments, or are otherwise malformed.
    ///
    /// Percent-encoded characters are rejected rather than decoded because
    /// scope identity must be unambiguous: `/foo%2Fbar` and `/foo/bar`
    /// must not silently map to different scopes, and `%2E%2E` must not
    /// bypass the dot-segment check.
    ///
    /// This is used internally by `accept()` and `connect()`, but is also
    /// available for callers that need to validate paths from other sources
    /// (e.g., announce URLs used for forward connections).
    pub fn normalize_connection_path(raw: &str) -> Result<Option<String>, SessionError> {
        if raw.is_empty() || raw == "/" {
            return Ok(None);
        }

        if raw.len() > Self::MAX_CONNECTION_PATH_LEN {
            return Err(SessionError::InvalidPath("path too long".to_string()));
        }

        if !raw.starts_with('/') {
            return Err(SessionError::InvalidPath(
                "path must start with '/'".to_string(),
            ));
        }

        let trimmed = raw.trim_end_matches('/');
        if trimmed.is_empty() {
            return Ok(None);
        }

        let mut segments = trimmed.split('/');
        let _ = segments.next();
        for segment in segments {
            if segment.is_empty() {
                return Err(SessionError::InvalidPath(
                    "path contains empty segment".to_string(),
                ));
            }
            if segment.contains('%') {
                return Err(SessionError::InvalidPath(
                    "path must not contain percent-encoded characters".to_string(),
                ));
            }
            if segment == "." || segment == ".." {
                return Err(SessionError::InvalidPath(
                    "path contains invalid segment".to_string(),
                ));
            }
        }

        Ok(Some(trimmed.to_string()))
    }

    fn decode_client_setup_path(params: &KeyValuePairs) -> Result<Option<String>, SessionError> {
        let Some(kvp) = params.get(setup::ParameterType::Path.into()) else {
            return Ok(None);
        };

        let bytes = match &kvp.value {
            Value::BytesValue(bytes) => bytes,
            _ => {
                return Err(SessionError::InvalidPath(
                    "PATH parameter must be bytes-encoded".to_string(),
                ))
            }
        };

        if bytes.len() > Self::MAX_CONNECTION_PATH_LEN {
            return Err(SessionError::InvalidPath("path too long".to_string()));
        }

        let path = std::str::from_utf8(bytes)
            .map_err(|_| SessionError::InvalidPath("path must be UTF-8".to_string()))?;

        Self::normalize_connection_path(path)
    }

    /// Returns the negotiated transport protocol for this connection.
    pub fn transport(&self) -> Transport {
        self.transport
    }

    /// Returns the connection path, if one was present on the incoming connection.
    ///
    /// For server-side sessions (created via `accept()`), this is derived from:
    /// 1. The WebTransport CONNECT URL path (takes precedence), or
    /// 2. The CLIENT_SETUP PATH parameter (key 0x1), used for raw QUIC connections.
    ///
    /// Returns `None` if no path was present or if the path was just "/".
    pub fn connection_path(&self) -> Option<&str> {
        self.connection_path.as_deref()
    }

    /// This session's correlation id, tagged onto its log records.
    ///
    /// Normally the QUIC connection ID hex, so it also names this connection's qlog and mlog
    /// files and matches what the peer observes.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Log a control message with structured fields for observability.
    /// Uses target "moq_transport::control" so it can be filtered independently.
    fn log_control_message(session_id: &SessionId, msg: &Message, direction: &str) {
        match msg {
            Message::Subscribe(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "SUBSCRIBE",
                    subscribe_id = m.id,
                    namespace = %m.track_namespace,
                    track_name = %m.track_name,
                    "MoQT control message"
                );
            }
            Message::SubscribeOk(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "SUBSCRIBE_OK",
                    subscribe_id = m.id,
                    track_alias = m.track_alias,
                    "MoQT control message"
                );
            }
            Message::Unsubscribe(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "UNSUBSCRIBE",
                    subscribe_id = m.id,
                    "MoQT control message"
                );
            }
            Message::PublishNamespace(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "PUBLISH_NAMESPACE",
                    request_id = m.id,
                    namespace = %m.track_namespace,
                    "MoQT control message"
                );
            }
            Message::PublishNamespaceDone(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "PUBLISH_NAMESPACE_DONE",
                    request_id = m.id,
                    "MoQT control message"
                );
            }
            Message::Namespace(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "NAMESPACE",
                    namespace_suffix = %m.track_namespace_suffix,
                    "MoQT control message"
                );
            }
            Message::NamespaceDone(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "NAMESPACE_DONE",
                    namespace_suffix = %m.track_namespace_suffix,
                    "MoQT control message"
                );
            }
            Message::PublishNamespaceCancel(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "PUBLISH_NAMESPACE_CANCEL",
                    request_id = m.id,
                    error_code = m.error_code,
                    reason = ?m.reason_phrase.0,
                    "MoQT control message"
                );
            }
            Message::TrackStatus(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "TRACK_STATUS",
                    request_id = m.id,
                    namespace = %m.track_namespace,
                    track_name = %m.track_name,
                    "MoQT control message"
                );
            }
            Message::SubscribeNamespace(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "SUBSCRIBE_NAMESPACE",
                    request_id = m.id,
                    namespace_prefix = %m.track_namespace_prefix,
                    "MoQT control message"
                );
            }
            Message::Fetch(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "FETCH",
                    request_id = m.id,
                    fetch_type = ?m.fetch_type,
                    "MoQT control message"
                );
            }
            Message::FetchOk(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "FETCH_OK",
                    request_id = m.id,
                    end_of_track = m.end_of_track,
                    "MoQT control message"
                );
            }
            Message::FetchCancel(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "FETCH_CANCEL",
                    request_id = m.id,
                    "MoQT control message"
                );
            }
            Message::Publish(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "PUBLISH",
                    request_id = m.id,
                    namespace = %m.track_namespace,
                    track_name = %m.track_name,
                    track_alias = m.track_alias,
                    "MoQT control message"
                );
            }
            Message::PublishOk(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "PUBLISH_OK",
                    request_id = m.id,
                    "MoQT control message"
                );
            }
            Message::PublishDone(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "PUBLISH_DONE",
                    request_id = m.id,
                    status_code = m.status_code,
                    stream_count = m.stream_count,
                    // The reason is peer-supplied (up to 1024 bytes, arbitrary UTF-8).
                    // Log a sanitized form (control chars → '?') and the raw hex so
                    // analysts can reconstruct the exact bytes without log-injection risk.
                    reason_lossy = ?SanitizedDisplay(&m.reason.0).to_string(),
                    reason_hex = %HexDisplay(&m.reason.0),
                    "MoQT control message"
                );
            }
            Message::GoAway(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "GOAWAY",
                    uri = %m.uri.0,
                    "MoQT control message"
                );
            }
            Message::MaxRequestId(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "MAX_REQUEST_ID",
                    request_id = m.request_id,
                    "MoQT control message"
                );
            }
            Message::RequestsBlocked(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "REQUESTS_BLOCKED",
                    max_request_id = m.max_request_id,
                    "MoQT control message"
                );
            }
            Message::RequestOk(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "REQUEST_OK",
                    request_id = m.id,
                    "MoQT control message"
                );
            }
            Message::RequestError(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "REQUEST_ERROR",
                    request_id = m.id,
                    error_code = m.error_code,
                    retry_interval = m.retry_interval,
                    // The reason is peer-supplied (up to 1024 bytes, arbitrary UTF-8).
                    // Log a sanitized form (control chars → '?') and the raw hex so
                    // analysts can reconstruct the exact bytes without log-injection risk.
                    reason_lossy = ?SanitizedDisplay(&m.reason.0).to_string(),
                    reason_hex = %HexDisplay(&m.reason.0),
                    "MoQT control message"
                );
            }
            Message::RequestUpdate(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    direction,
                    msg_type = "REQUEST_UPDATE",
                    request_id = m.id,
                    existing_request_id = m.existing_request_id,
                    "MoQT control message"
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        webtransport: web_transport::Session,
        session_id: SessionId,
        sender: Writer,
        recver: Reader,
        mlog: Option<mlog::MlogWriter>,
        transport: Transport,
        connection_path: Option<String>,
        request_id: RequestId,
    ) -> (Self, Option<Publisher>, Option<Subscriber>) {
        let outgoing = Queue::default().split();
        let pending_requests = PendingRequests::default();
        let subscribe_namespace_open = Queue::default().split();

        // Wrap mlog in Arc<Mutex<>> for sharing across tasks
        let mlog_shared = mlog.map(|m| Arc::new(Mutex::new(m)));

        let publisher = Some(Publisher::new(
            outgoing.0.clone(),
            webtransport.clone(),
            mlog_shared.clone(),
            request_id.clone(),
            pending_requests.clone(),
            session_id.clone(),
        ));
        let subscriber = Some(Subscriber::new(
            outgoing.0,
            subscribe_namespace_open.0,
            mlog_shared.clone(),
            request_id.clone(),
            pending_requests.clone(),
            session_id.clone(),
        ));

        let session = Self {
            webtransport,
            sender,
            recver,
            publisher: publisher.clone(),
            subscriber: subscriber.clone(),
            outgoing: outgoing.1,
            subscribe_namespace_open: subscribe_namespace_open.1,
            request_id,
            pending_requests,
            mlog: mlog_shared,
            transport,
            connection_path,
            session_id,
        };

        (session, publisher, subscriber)
    }

    /// Create an outbound/client QUIC connection.
    ///
    /// Opens the bidirectional control stream, sends CLIENT_SETUP with
    /// parameters only (version is agreed via ALPN), and waits for SERVER_SETUP.
    ///
    /// For native `moqt://` connections the PATH and AUTHORITY parameters are
    /// sent automatically.  For WebTransport the path is carried in the HTTP/3
    /// CONNECT URL so PATH is not sent.
    ///
    /// Generates a local [`SessionId`] fallback. Use [`Self::connect_with_session_id`]
    /// when a peer-observed QUIC connection ID is available.
    pub async fn connect(
        session: web_transport::Session,
        mlog_path: Option<PathBuf>,
        transport: Transport,
    ) -> Result<(Session, Publisher, Subscriber), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `connect` accept it and remove `connect_with_session_id`.
        Self::connect_with_session_id(session, SessionId::generate(), mlog_path, transport).await
    }

    /// Create an outbound/client QUIC connection with an explicit correlation ID.
    ///
    /// `session_id` should normally be the peer-observed QUIC connection ID.
    pub async fn connect_with_session_id(
        session: web_transport::Session,
        session_id: SessionId,
        mlog_path: Option<PathBuf>,
        transport: Transport,
    ) -> Result<(Session, Publisher, Subscriber), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `connect_with_config` accept it and remove `connect_with_config_and_session_id`.
        Self::connect_with_config_and_session_id(
            session,
            session_id,
            mlog_path,
            transport,
            SessionConfig::default(),
        )
        .await
    }

    /// Create an outbound/client QUIC connection with explicit session configuration.
    ///
    /// Generates a local [`SessionId`] fallback. Use
    /// [`Self::connect_with_config_and_session_id`] when a peer-observed QUIC connection ID is
    /// available.
    pub async fn connect_with_config(
        session: web_transport::Session,
        mlog_path: Option<PathBuf>,
        transport: Transport,
        config: SessionConfig,
    ) -> Result<(Session, Publisher, Subscriber), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `connect_with_config` accept it and remove `connect_with_config_and_session_id`.
        Self::connect_with_config_and_session_id(
            session,
            SessionId::generate(),
            mlog_path,
            transport,
            config,
        )
        .await
    }

    /// Create an outbound/client QUIC connection with explicit configuration and correlation ID.
    ///
    /// `session_id` should normally be the peer-observed QUIC connection ID.
    pub async fn connect_with_config_and_session_id(
        session: web_transport::Session,
        session_id: SessionId,
        mlog_path: Option<PathBuf>,
        transport: Transport,
        config: SessionConfig,
    ) -> Result<(Session, Publisher, Subscriber), SessionError> {
        let url = session.url().clone();
        let url_path = url.path();
        let path = Self::normalize_connection_path(url_path)?;

        let mlog = mlog_path.and_then(|p| {
            mlog::MlogWriter::new(p)
                .map_err(
                    |e| tracing::warn!(session_id = %session_id, "Failed to create mlog: {}", e),
                )
                .ok()
        });

        let control = session.open_bi().await?;
        let mut sender = Writer::new(session_id.clone(), control.0);
        let mut recver = Reader::new(session_id.clone(), control.1);

        let mut params = KeyValuePairs::default();

        if transport == Transport::RawQuic {
            // Draft-16 §9.3.1.1: send AUTHORITY for native QUIC.
            if let Some(host) = url.host_str() {
                let authority = if let Some(port) = url.port() {
                    format!("{}:{}", host, port)
                } else {
                    host.to_string()
                };
                params.set_bytesvalue(
                    setup::ParameterType::Authority.into(),
                    authority.into_bytes(),
                );
            }

            // Draft-16 §9.3.1.2: send PATH (path + optional query) for native QUIC.
            let path_and_query = match url.query() {
                Some(q) => format!("{}?{}", url_path, q),
                None => url_path.to_string(),
            };
            if !path_and_query.is_empty() && path_and_query != "/" {
                params.set_bytesvalue(
                    setup::ParameterType::Path.into(),
                    path_and_query.into_bytes(),
                );
            }
        }

        // The MAX_REQUEST_ID we advertise to the server.
        let our_max_request_id = config.max_request_id;
        params.set_intvalue(
            setup::ParameterType::MaxRequestId.into(),
            our_max_request_id,
        );

        let client = setup::Client { params };

        tracing::debug!(
            target: "moq_transport::control",
            session_id = %session_id,
            direction = "sent",
            msg_type = "CLIENT_SETUP",
            ?transport,
            path = path.as_deref(),
            "MoQT control message"
        );
        sender.encode(&client).await?;

        let server: setup::Server = recver.decode().await?;
        tracing::debug!(
            target: "moq_transport::control",
            session_id = %session_id,
            direction = "recv",
            msg_type = "SERVER_SETUP",
            "MoQT control message"
        );

        let peer_max = max_request_id_from_params(&server.params);
        Self::log_peer_max_request_id(&session_id, peer_max);
        // Client sends even IDs (0); peer server sends odd IDs (1).
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `RequestId::new` accept it and remove `RequestId::new_with_session_id`.
        let request_id =
            RequestId::new_with_session_id(session_id.clone(), 0, peer_max, our_max_request_id, 1);
        let session = Session::new(
            session, session_id, sender, recver, mlog, transport, path, request_id,
        );
        let publisher = session.1.ok_or(SessionError::Internal)?;
        let subscriber = session.2.ok_or(SessionError::Internal)?;
        Ok((session.0, publisher, subscriber))
    }

    /// Accept an inbound server connection.
    ///
    /// Waits for the bidirectional control stream, decodes CLIENT_SETUP,
    /// sends SERVER_SETUP with parameters only.  Version is already agreed
    /// via ALPN before this is called.
    ///
    /// Generates a local [`SessionId`] fallback. Use [`Self::accept_with_session_id`]
    /// when a peer-observed QUIC connection ID is available.
    pub async fn accept(
        session: web_transport::Session,
        mlog_path: Option<PathBuf>,
        transport: Transport,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `accept` accept it and remove `accept_with_session_id`.
        Self::accept_with_session_id(session, SessionId::generate(), mlog_path, transport).await
    }

    /// Accept an inbound server connection with an explicit correlation ID.
    ///
    /// `session_id` should normally be the peer-observed QUIC connection ID.
    pub async fn accept_with_session_id(
        session: web_transport::Session,
        session_id: SessionId,
        mlog_path: Option<PathBuf>,
        transport: Transport,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `accept_with_config` accept it and remove `accept_with_config_and_session_id`.
        Self::accept_with_config_and_session_id(
            session,
            session_id,
            mlog_path,
            transport,
            SessionConfig::default(),
        )
        .await
    }

    /// Accept an inbound server connection with explicit session configuration.
    ///
    /// Generates a local [`SessionId`] fallback. Use
    /// [`Self::accept_with_config_and_session_id`] when a peer-observed QUIC connection ID is
    /// available.
    pub async fn accept_with_config(
        session: web_transport::Session,
        mlog_path: Option<PathBuf>,
        transport: Transport,
        config: SessionConfig,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `accept_with_config` accept it and remove `accept_with_config_and_session_id`.
        Self::accept_with_config_and_session_id(
            session,
            SessionId::generate(),
            mlog_path,
            transport,
            config,
        )
        .await
    }

    /// Accept an inbound server connection with explicit configuration and correlation ID.
    ///
    /// `session_id` should normally be the peer-observed QUIC connection ID.
    pub async fn accept_with_config_and_session_id(
        session: web_transport::Session,
        session_id: SessionId,
        mlog_path: Option<PathBuf>,
        transport: Transport,
        config: SessionConfig,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        let mut mlog = mlog_path.and_then(|p| {
            mlog::MlogWriter::new(p)
                .map_err(
                    |e| tracing::warn!(session_id = %session_id, "Failed to create mlog: {}", e),
                )
                .ok()
        });

        let control = session.accept_bi().await?;
        let mut sender = Writer::new(session_id.clone(), control.0);
        let mut recver = Reader::new(session_id.clone(), control.1);

        let client: setup::Client = recver.decode().await?;
        tracing::debug!(
            target: "moq_transport::control",
            session_id = %session_id,
            direction = "recv",
            msg_type = "CLIENT_SETUP",
            "MoQT control message"
        );

        // For WebTransport the path arrives in the HTTP/3 CONNECT :path.
        // For raw QUIC the PATH setup parameter carries it instead.
        let wt_url_path = session.url().path();
        let wt_path = Self::normalize_connection_path(wt_url_path)?;

        let client_setup_path = if wt_path.is_none() {
            Self::decode_client_setup_path(&client.params)?
        } else {
            None
        };

        let connection_path = wt_path.or(client_setup_path);

        if connection_path.is_some() {
            tracing::debug!(
                session_id = %session_id,
                connection_path = connection_path.as_deref(),
                "Connection path resolved"
            );
        }

        if let Some(ref mut mlog) = mlog {
            let event = mlog::events::client_setup_parsed(mlog.elapsed_ms(), 0, &client);
            let _ = mlog.add_event(event);
        }

        let peer_max = max_request_id_from_params(&client.params);
        Self::log_peer_max_request_id(&session_id, peer_max);

        // The MAX_REQUEST_ID we advertise to the client.
        let our_max_request_id = config.max_request_id;
        let mut params = KeyValuePairs::default();
        params.set_intvalue(
            setup::ParameterType::MaxRequestId.into(),
            our_max_request_id,
        );

        let server = setup::Server { params };

        tracing::debug!(
            target: "moq_transport::control",
            session_id = %session_id,
            direction = "sent",
            msg_type = "SERVER_SETUP",
            "MoQT control message"
        );

        if let Some(ref mut mlog) = mlog {
            let event = mlog::events::server_setup_created(mlog.elapsed_ms(), 0, &server);
            let _ = mlog.add_event(event);
        }

        sender.encode(&server).await?;

        // Server sends odd IDs (1); peer client sends even IDs (0).
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `RequestId::new` accept it and remove `RequestId::new_with_session_id`.
        let request_id =
            RequestId::new_with_session_id(session_id.clone(), 1, peer_max, our_max_request_id, 0);
        Ok(Session::new(
            session,
            session_id,
            sender,
            recver,
            mlog,
            transport,
            connection_path,
            request_id,
        ))
    }

    /// Run Tasks for the session, including sending of control messages, receiving and processing
    /// inbound control messages, receiving and processing new inbound uni-directional QUIC streams,
    /// and receiving and processing QUIC datagrams received
    pub async fn run(self) -> Result<(), SessionError> {
        tokio::select! {
            res = Self::run_recv(self.session_id.clone(), self.recver, self.publisher.clone(), self.subscriber.clone(), self.mlog.clone(), self.request_id.clone(), self.pending_requests.clone()) => res,
            res = Self::run_send(self.session_id.clone(), self.sender, self.outgoing, self.mlog.clone()) => res,
            res = Self::run_subscribe_namespace_open(self.session_id.clone(), self.webtransport.clone(), self.subscribe_namespace_open, self.mlog.clone()) => res,
            res = Self::run_subscribe_namespace_accept(self.session_id.clone(), self.webtransport.clone(), self.publisher.clone(), self.request_id.clone(), self.mlog.clone()) => res,
            res = Self::run_streams(self.session_id.clone(), self.webtransport.clone(), self.subscriber.clone()) => res,
            res = Self::run_datagrams(self.webtransport, self.subscriber.clone()) => res,
            res = Self::run_pending_timeouts(self.session_id, self.publisher, self.subscriber, self.pending_requests) => res,
        }
    }

    /// Processes the outgoing control message queue, and sends queued messages on the control stream sender/writer.
    async fn run_send(
        session_id: SessionId,
        mut sender: Writer,
        mut outgoing: Queue<message::Message>,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        while let Some(msg) = outgoing.pop().await {
            // Emit structured tracing log for sent control messages
            Self::log_control_message(&session_id, &msg, "sent");

            // Emit mlog event for sent control messages
            if let Some(ref mlog) = mlog {
                if let Ok(mut mlog_guard) = mlog.lock() {
                    let time = mlog_guard.elapsed_ms();
                    let stream_id = 0; // Control stream is always stream 0

                    // Emit events based on message type
                    let event = match &msg {
                        Message::Subscribe(m) => {
                            Some(mlog::events::subscribe_created(time, stream_id, m))
                        }
                        Message::SubscribeOk(m) => {
                            Some(mlog::events::subscribe_ok_created(time, stream_id, m))
                        }
                        Message::Unsubscribe(m) => {
                            Some(mlog::events::unsubscribe_created(time, stream_id, m))
                        }
                        Message::PublishNamespace(m) => {
                            Some(mlog::events::publish_namespace_created(time, stream_id, m))
                        }
                        Message::GoAway(m) => {
                            Some(mlog::events::go_away_created(time, stream_id, m))
                        }
                        _ => None, // TODO: Add other message types
                    };

                    if let Some(event) = event {
                        let _ = mlog_guard.add_event(event);
                    }
                }
            }

            sender.encode(&msg).await?;
        }

        Ok(())
    }

    async fn run_subscribe_namespace_open(
        session_id: SessionId,
        webtransport: web_transport::Session,
        mut requests: Queue<OpenSubscribeNamespace>,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();
        let mut requests_done = false;

        loop {
            tokio::select! {
                request = requests.pop(), if !requests_done => {
                    match request {
                        Some(request) => {
                            let webtransport = webtransport.clone();
                            tasks.push(Self::open_subscribe_namespace(session_id.clone(), webtransport, request, mlog.clone()));
                        }
                        None => requests_done = true,
                    }
                }
                Some(res) = tasks.next(), if !tasks.is_empty() => res?,
                else => return Ok(()),
            }
        }
    }

    async fn open_subscribe_namespace(
        session_id: SessionId,
        webtransport: web_transport::Session,
        request: OpenSubscribeNamespace,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        let (send, recv) = webtransport.open_bi().await?;
        let mut writer = Writer::new(session_id.clone(), send);
        let reader = Reader::new(session_id.clone(), recv);

        let msg = Message::SubscribeNamespace(request.message.clone());
        Self::log_control_message(&session_id, &msg, "sent");
        add_mlog_event(&mlog, |time| {
            mlog::events::subscribe_namespace_created(time, 0, &request.message)
        });
        writer.encode(&msg).await?;

        let (send, recv) = SubscribeNamespace::new(request.subscriber, request.info, writer);
        if request.reply.send(Ok(send)).is_err() {
            return Ok(());
        }

        recv.run(reader, mlog).await
    }

    async fn run_subscribe_namespace_accept(
        session_id: SessionId,
        webtransport: web_transport::Session,
        publisher: Option<Publisher>,
        request_id: RequestId,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();

        loop {
            tokio::select! {
                // Only accept a new stream while below the concurrency cap; otherwise
                // apply backpressure until an in-flight stream completes (#1).
                stream = webtransport.accept_bi(), if tasks.len() < MAX_CONCURRENT_SUBSCRIBE_NAMESPACE_STREAMS => {
                    let (send, recv) = stream?;
                    let publisher = publisher.clone().ok_or(SessionError::RoleViolation)?;
                    let request_id = request_id.clone();
                    tasks.push(Self::accept_subscribe_namespace_stream(session_id.clone(), publisher, request_id, send, recv, mlog.clone()));
                }
                Some(res) = tasks.next(), if !tasks.is_empty() => res?,
            }
        }
    }

    async fn accept_subscribe_namespace_stream(
        session_id: SessionId,
        mut publisher: Publisher,
        request_id: RequestId,
        send: web_transport::SendStream,
        recv: web_transport::RecvStream,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        let writer = Writer::new(session_id.clone(), send);
        let mut reader = Reader::new(session_id.clone(), recv);
        // Bound the wait for the stream's opening SUBSCRIBE_NAMESPACE header so an
        // idle or malicious peer cannot hold an accept slot open forever (#2).
        let msg = tokio::time::timeout(
            SUBSCRIBE_NAMESPACE_HEADER_TIMEOUT,
            reader.decode::<Message>(),
        )
        .await
        .map_err(|_| {
            SessionError::ProtocolViolation(
                "timed out waiting for SUBSCRIBE_NAMESPACE header on bidirectional stream"
                    .to_string(),
            )
        })??;

        let subscribe_namespace = match msg {
            Message::SubscribeNamespace(msg) => msg,
            other => {
                return Err(SessionError::ProtocolViolation(format!(
                    "bidirectional stream began with {} instead of SUBSCRIBE_NAMESPACE",
                    other.name()
                )))
            }
        };
        let log_msg = Message::SubscribeNamespace(subscribe_namespace.clone());
        Self::log_control_message(&session_id, &log_msg, "recv");
        add_mlog_event(&mlog, |time| {
            mlog::events::subscribe_namespace_parsed(time, 0, &subscribe_namespace)
        });

        request_id.validate_incoming(subscribe_namespace.id)?;
        let recv = publisher.recv_subscribe_namespace(subscribe_namespace)?;
        recv.run(writer, reader, mlog).await
    }

    /// Receives inbound messages from the control stream reader/receiver.  Analyzes if the message
    /// is to be handled by Subscriber or Publisher logic and calls recv_message on either the
    /// Publisher or Subscriber.
    /// Receives and dispatches control messages.
    /// Handles session-level messages (GOAWAY, MAX_REQUEST_ID, REQUESTS_BLOCKED)
    /// directly and routes role-specific messages to Publisher or Subscriber.
    async fn run_recv(
        session_id: SessionId,
        mut recver: Reader,
        mut publisher: Option<Publisher>,
        mut subscriber: Option<Subscriber>,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
        request_id: RequestId,
        pending_requests: PendingRequests,
    ) -> Result<(), SessionError> {
        let mut goaway_received = false;

        loop {
            let msg: message::Message = recver.decode().await?;

            // Emit structured tracing log for received control messages
            Self::log_control_message(&session_id, &msg, "recv");

            // Emit mlog event for received control messages
            if let Some(ref mlog) = mlog {
                if let Ok(mut mlog_guard) = mlog.lock() {
                    let time = mlog_guard.elapsed_ms();
                    let stream_id = 0; // Control stream is always stream 0

                    // Emit events based on message type
                    let event = match &msg {
                        Message::Subscribe(m) => {
                            Some(mlog::events::subscribe_parsed(time, stream_id, m))
                        }
                        Message::SubscribeOk(m) => {
                            Some(mlog::events::subscribe_ok_parsed(time, stream_id, m))
                        }
                        Message::Unsubscribe(m) => {
                            Some(mlog::events::unsubscribe_parsed(time, stream_id, m))
                        }
                        Message::PublishNamespace(m) => {
                            Some(mlog::events::publish_namespace_parsed(time, stream_id, m))
                        }
                        Message::GoAway(m) => {
                            Some(mlog::events::go_away_parsed(time, stream_id, m))
                        }
                        _ => None, // TODO: Add other message types
                    };

                    if let Some(event) = event {
                        let _ = mlog_guard.add_event(event);
                    }
                }
            }

            if let Some(id) = msg.sequenced_request_id() {
                request_id.validate_incoming(id)?;
            }

            let msg = match msg {
                Message::RequestOk(msg) => {
                    Self::recv_request_ok(&session_id, &pending_requests, &mut publisher, msg)?;
                    continue;
                }
                Message::RequestError(msg) => {
                    Self::recv_request_error(
                        &session_id,
                        &pending_requests,
                        &mut publisher,
                        &mut subscriber,
                        msg,
                    )?;
                    continue;
                }
                Message::PublishOk(msg) => {
                    Self::recv_publish_ok(&session_id, &pending_requests, &mut publisher, msg)?;
                    continue;
                }
                Message::SubscribeOk(msg) => {
                    Self::recv_subscribe_ok(&session_id, &pending_requests, &mut subscriber, msg)?;
                    continue;
                }
                msg => msg,
            };

            let msg = match TryInto::<message::Publisher>::try_into(msg) {
                Ok(msg) => {
                    subscriber
                        .as_mut()
                        .ok_or(SessionError::RoleViolation)?
                        .recv_message(msg)?;
                    continue;
                }
                Err(msg) => msg,
            };

            let msg = match TryInto::<message::Subscriber>::try_into(msg) {
                Ok(msg) => {
                    publisher
                        .as_mut()
                        .ok_or(SessionError::RoleViolation)?
                        .recv_message(msg)?;
                    continue;
                }
                Err(msg) => msg,
            };

            // Session-level messages handled here (not role-specific).
            match msg {
                Message::GoAway(ref m) => {
                    // Draft-16 §9.4: receiving a second GOAWAY is PROTOCOL_VIOLATION.
                    if goaway_received {
                        return Err(SessionError::ProtocolViolation(
                            "received multiple GOAWAY messages".to_string(),
                        ));
                    }
                    goaway_received = true;
                    tracing::info!(
                        target: "moq_transport::control",
                        session_id = %session_id,
                        new_uri = %m.uri.0,
                        "received GOAWAY"
                    );
                    // TODO(itzmanish): trigger session migration.
                }
                Message::MaxRequestId(ref m) => {
                    request_id.apply_max_request_id(m)?;
                    tracing::debug!(
                        target: "moq_transport::control",
                        session_id = %session_id,
                        max_request_id = m.request_id,
                        "received MAX_REQUEST_ID"
                    );
                }
                Message::RequestsBlocked(ref m) => {
                    tracing::debug!(
                        target: "moq_transport::control",
                        session_id = %session_id,
                        max_request_id = m.max_request_id,
                        "received REQUESTS_BLOCKED"
                    );
                    // REQUESTS_BLOCKED tells us the peer's send budget is exhausted.
                    request_id.handle_requests_blocked(m)?;
                }
                other => {
                    tracing::warn!(session_id = %session_id, msg_type = other.name(), "received unhandled message type");
                    return Err(SessionError::unimplemented(&format!(
                        "message type {}",
                        other.name()
                    )));
                }
            }
        }
    }

    fn recv_request_ok(
        session_id: &SessionId,
        pending_requests: &PendingRequests,
        publisher: &mut Option<Publisher>,
        msg: message::RequestOk,
    ) -> Result<(), SessionError> {
        match pending_requests.complete(msg.id, PendingResponse::RequestOk)? {
            Some(PendingRequest::PublishNamespace) => publisher
                .as_mut()
                .ok_or(SessionError::RoleViolation)?
                .recv_request_ok(msg),
            Some(request) => Err(SessionError::ProtocolViolation(format!(
                "REQUEST_OK completed unexpected {:?} request {}",
                request, msg.id
            ))),
            None => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    request_id = msg.id,
                    "received REQUEST_OK for unknown outbound request — ignoring"
                );
                Ok(())
            }
        }
    }

    fn recv_request_error(
        session_id: &SessionId,
        pending_requests: &PendingRequests,
        publisher: &mut Option<Publisher>,
        subscriber: &mut Option<Subscriber>,
        msg: message::RequestError,
    ) -> Result<(), SessionError> {
        match pending_requests.complete(msg.id, PendingResponse::RequestError)? {
            Some(PendingRequest::PublishNamespace | PendingRequest::Publish) => publisher
                .as_mut()
                .ok_or(SessionError::RoleViolation)?
                .recv_request_error(msg),
            Some(PendingRequest::Subscribe) => subscriber
                .as_mut()
                .ok_or(SessionError::RoleViolation)?
                .recv_request_error(&msg),
            None => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    request_id = msg.id,
                    error_code = msg.error_code,
                    retry_interval = msg.retry_interval,
                    reason = ?msg.reason.0,
                    "received REQUEST_ERROR for unknown outbound request — ignoring"
                );
                Ok(())
            }
        }
    }

    fn recv_publish_ok(
        session_id: &SessionId,
        pending_requests: &PendingRequests,
        publisher: &mut Option<Publisher>,
        msg: message::PublishOk,
    ) -> Result<(), SessionError> {
        match pending_requests.complete(msg.id, PendingResponse::PublishOk)? {
            Some(PendingRequest::Publish) => publisher
                .as_mut()
                .ok_or(SessionError::RoleViolation)?
                .recv_publish_ok(msg),
            Some(request) => Err(SessionError::ProtocolViolation(format!(
                "PUBLISH_OK completed unexpected {:?} request {}",
                request, msg.id
            ))),
            None => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    request_id = msg.id,
                    "received PUBLISH_OK for unknown outbound request — ignoring"
                );
                Ok(())
            }
        }
    }

    fn recv_subscribe_ok(
        session_id: &SessionId,
        pending_requests: &PendingRequests,
        subscriber: &mut Option<Subscriber>,
        msg: message::SubscribeOk,
    ) -> Result<(), SessionError> {
        match pending_requests.complete(msg.id, PendingResponse::SubscribeOk)? {
            Some(PendingRequest::Subscribe) => subscriber
                .as_mut()
                .ok_or(SessionError::RoleViolation)?
                .recv_subscribe_ok(&msg),
            Some(request) => Err(SessionError::ProtocolViolation(format!(
                "SUBSCRIBE_OK completed unexpected {:?} request {}",
                request, msg.id
            ))),
            None => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    request_id = msg.id,
                    "received SUBSCRIBE_OK for unknown outbound request — ignoring"
                );
                Ok(())
            }
        }
    }

    async fn run_pending_timeouts(
        session_id: SessionId,
        mut publisher: Option<Publisher>,
        mut subscriber: Option<Subscriber>,
        pending_requests: PendingRequests,
    ) -> Result<(), SessionError> {
        loop {
            let Some(deadline) = pending_requests.next_deadline()? else {
                pending_requests.changed().await;
                continue;
            };

            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {},
                _ = pending_requests.changed() => continue,
            }

            for (id, request) in pending_requests.expire()? {
                tracing::warn!(
                    target: "moq_transport::control",
                    session_id = %session_id,
                    request_id = id,
                    request = ?request,
                    "outbound request timed out waiting for response"
                );
                match request {
                    PendingRequest::PublishNamespace | PendingRequest::Publish => publisher
                        .as_mut()
                        .ok_or(SessionError::RoleViolation)?
                        .recv_request_timeout(id, request)?,
                    PendingRequest::Subscribe => subscriber
                        .as_mut()
                        .ok_or(SessionError::RoleViolation)?
                        .recv_request_timeout(id, request)?,
                }
            }
        }
    }

    /// Accepts uni-directional quic streams and starts handling for them.
    /// Will read stream header to know what type of stream it is and create
    /// the appropriate stream handlers.
    async fn run_streams(
        session_id: SessionId,
        webtransport: web_transport::Session,
        subscriber: Option<Subscriber>,
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();

        loop {
            tokio::select! {
                res = webtransport.accept_uni() => {
                    let stream = res?;
                    let subscriber = subscriber.clone().ok_or(SessionError::RoleViolation)?;
                    let session_id = session_id.clone();

                    tasks.push(async move {
                        if let Err(err) = Subscriber::recv_stream(subscriber, stream).await {
                            tracing::warn!(session_id = %session_id, "failed to serve stream: {}", err);
                        };
                    });
                },
                _ = tasks.next(), if !tasks.is_empty() => {},
            };
        }
    }

    /// Receives QUIC datagrams and processes them using the Subscriber logic
    async fn run_datagrams(
        webtransport: web_transport::Session,
        mut subscriber: Option<Subscriber>,
    ) -> Result<(), SessionError> {
        loop {
            let datagram = webtransport.recv_datagram().await?;
            subscriber
                .as_mut()
                .ok_or(SessionError::RoleViolation)?
                .recv_datagram(datagram)
                .await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    #[derive(Clone, Default)]
    struct Capture(StdArc<StdMutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs(emit: impl FnOnce()) -> String {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .finish();

        tracing::subscriber::with_default(subscriber, emit);

        let output = capture.0.lock().unwrap().clone();
        String::from_utf8(output).unwrap()
    }

    // ========================================================================
    // normalize_connection_path
    // ========================================================================

    #[test]
    fn normalize_empty_and_root() {
        assert_eq!(Session::normalize_connection_path("").unwrap(), None);
        assert_eq!(Session::normalize_connection_path("/").unwrap(), None);
        assert_eq!(Session::normalize_connection_path("///").unwrap(), None);
    }

    #[test]
    fn normalize_valid_paths() {
        assert_eq!(
            Session::normalize_connection_path("/app").unwrap(),
            Some("/app".to_string())
        );
        assert_eq!(
            Session::normalize_connection_path("/tenant/stream-1").unwrap(),
            Some("/tenant/stream-1".to_string())
        );
        // Trailing slash is trimmed
        assert_eq!(
            Session::normalize_connection_path("/app/").unwrap(),
            Some("/app".to_string())
        );
    }

    #[test]
    fn normalize_rejects_missing_leading_slash() {
        assert!(Session::normalize_connection_path("app").is_err());
    }

    #[test]
    fn normalize_rejects_empty_segments() {
        assert!(Session::normalize_connection_path("/app//stream").is_err());
    }

    #[test]
    fn normalize_rejects_dot_segments() {
        assert!(Session::normalize_connection_path("/app/./stream").is_err());
        assert!(Session::normalize_connection_path("/app/../secret").is_err());
        assert!(Session::normalize_connection_path("/..").is_err());
    }

    #[test]
    fn normalize_rejects_percent_encoded_characters() {
        // %2F = '/' — would create scope ambiguity
        assert!(Session::normalize_connection_path("/foo%2Fbar").is_err());
        // %2E%2E = '..' — would bypass dot-segment check
        assert!(Session::normalize_connection_path("/%2E%2E/secret").is_err());
        // %00 = null — general injection risk
        assert!(Session::normalize_connection_path("/app/%00").is_err());
        // Uppercase hex digits
        assert!(Session::normalize_connection_path("/app/%2e%2e").is_err());
    }

    #[test]
    fn normalize_rejects_too_long_path() {
        let long_path = format!("/{}", "a".repeat(Session::MAX_CONNECTION_PATH_LEN));
        assert!(Session::normalize_connection_path(&long_path).is_err());
    }

    #[test]
    fn normalize_accepts_max_length_path() {
        // Exactly at the limit (1024 total including leading slash)
        let path = format!("/{}", "a".repeat(Session::MAX_CONNECTION_PATH_LEN - 1));
        assert!(Session::normalize_connection_path(&path).is_ok());
    }

    #[test]
    fn control_message_reasons_escape_log_forging_characters() {
        let session_id = SessionId::new("log-safety-session");
        let request_error = Message::RequestError(message::RequestError {
            id: 7,
            error_code: 1,
            retry_interval: 0,
            reason: crate::coding::ReasonPhrase("request\nforged\rline".to_string()),
        });
        let publish_done = Message::PublishDone(message::PublishDone {
            id: 8,
            status_code: 2,
            stream_count: 1,
            reason: crate::coding::ReasonPhrase("publish\nforged\rline".to_string()),
        });

        let output = capture_logs(|| {
            Session::log_control_message(&session_id, &request_error, "recv");
            Session::log_control_message(&session_id, &publish_done, "recv");
        });

        assert_eq!(
            output.lines().count(),
            2,
            "unexpected log records: {output}"
        );
        assert!(
            !output.contains('\r'),
            "raw carriage return in log: {output}"
        );
        // Our approach: reason_lossy (control chars → '?') + reason_hex (raw bytes).
        // The output itself contains '\n' as line terminators — only the peer-supplied
        // control chars must be absent, which is validated by the '\r' check above and
        // the reason_lossy assertions below.
        // '\n' (0x0a) and '\r' (0x0d) become '?' in the lossy display.
        assert!(
            output.contains(r#"reason_lossy="request?forged?line""#),
            "REQUEST_ERROR reason_lossy was not sanitized: {output}"
        );
        assert!(
            output.contains("reason_hex=726571756573740a666f726765640d6c696e65"),
            "REQUEST_ERROR reason_hex missing raw bytes: {output}"
        );
        assert!(
            output.contains(r#"reason_lossy="publish?forged?line""#),
            "PUBLISH_DONE reason_lossy was not sanitized: {output}"
        );
        assert!(
            output.contains("reason_hex=7075626c6973680a666f726765640d6c696e65"),
            "PUBLISH_DONE reason_hex missing raw bytes: {output}"
        );
    }

    #[test]
    fn control_message_reason_is_quoted_in_text_logs() {
        let message = Message::RequestError(message::RequestError {
            id: 7,
            error_code: 1,
            retry_interval: 0,
            reason: crate::coding::ReasonPhrase("ok level=ERROR fake=true".to_string()),
        });

        let output = capture_logs(|| {
            Session::log_control_message(&SessionId::generate(), &message, "recv");
        });

        assert!(
            output.contains(r#"reason_lossy="ok level=ERROR fake=true""#),
            "reason field was not quoted: {output}"
        );
    }

    #[test]
    fn subscribe_ok_for_unknown_request_id_is_ignored() {
        let pending_requests = PendingRequests::default();
        let mut subscriber = None;

        Session::recv_subscribe_ok(
            &SessionId::generate(),
            &pending_requests,
            &mut subscriber,
            message::SubscribeOk {
                id: 42,
                track_alias: 7,
                params: Default::default(),
                track_extensions: Default::default(),
            },
        )
        .unwrap();
    }

    // ========================================================================
    // SanitizedDisplay and HexDisplay
    // ========================================================================

    #[test]
    fn sanitized_display_passes_printable_ascii() {
        let s = "hello world 123 !@#";
        assert_eq!(SanitizedDisplay(s).to_string(), s);
    }

    #[test]
    fn sanitized_display_replaces_control_chars() {
        // Controls and Unicode line/paragraph separators must all become '?'.
        let input = "a\nb\rc\0d\te\x1bf\x7fg\u{0085}h\u{2028}i\u{2029}j";
        let out = SanitizedDisplay(input).to_string();
        assert!(!out.contains('\n'), "LF not sanitized: {out:?}");
        assert!(!out.contains('\r'), "CR not sanitized: {out:?}");
        assert!(!out.contains('\0'), "NUL not sanitized: {out:?}");
        assert!(!out.contains('\t'), "TAB not sanitized: {out:?}");
        assert!(!out.contains('\x1b'), "ESC not sanitized: {out:?}");
        assert!(!out.contains('\x7f'), "DEL not sanitized: {out:?}");
        assert!(!out.contains('\u{0085}'), "C1 NEL not sanitized: {out:?}");
        assert!(
            !out.contains('\u{2028}'),
            "line separator not sanitized: {out:?}"
        );
        assert!(
            !out.contains('\u{2029}'),
            "paragraph separator not sanitized: {out:?}"
        );
        // Each control char replaced by exactly one '?'.
        assert_eq!(out, "a?b?c?d?e?f?g?h?i?j");
    }

    #[test]
    fn sanitized_display_preserves_non_ascii_non_control_unicode() {
        let s = "café résumé 日本語 🎉";
        assert_eq!(SanitizedDisplay(s).to_string(), s);
    }

    #[test]
    fn hex_display_empty_string() {
        assert_eq!(HexDisplay("").to_string(), "");
    }

    #[test]
    fn hex_display_encodes_bytes_as_lowercase_hex() {
        // "AB" → 0x41 0x42 → "4142"
        assert_eq!(HexDisplay("AB").to_string(), "4142");
        // Multi-byte UTF-8: '€' = 0xE2 0x82 0xAC
        assert_eq!(HexDisplay("€").to_string(), "e282ac");
    }

    #[test]
    fn hex_display_round_trips_control_chars() {
        // Ensure the hex is faithful to the raw bytes even when SanitizedDisplay
        // would replace them.  '\n' = 0x0A, '\r' = 0x0D.
        assert_eq!(HexDisplay("\n\r").to_string(), "0a0d");
    }
}
