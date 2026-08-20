// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

mod error;
mod publish_namespace;
mod publish_received;
mod published;
mod published_namespace;
mod publisher;
mod reader;
mod request_id;
mod stream_drain;
mod subscribe;
mod subscribe_namespace;
mod subscribed;
mod subscribed_namespace;
mod subscriber;
mod track_status_requested;
mod writer;

pub use error::*;
pub use publish_namespace::*;
pub use publish_received::PublishReceived;
pub(crate) use publish_received::PublishReceivedRecv;
pub(crate) use published::{split_published_state, PublishedRecv, RequestStreamSink};
pub use published::{Published, PublishedInfo};
pub use published_namespace::*;
pub use publisher::*;
pub use request_id::RequestId;
pub(crate) use stream_drain::{DoneOutcome, StreamDrain};
pub use subscribe::*;
pub use subscribe_namespace::*;
pub use subscribed::*;
pub use subscribed_namespace::*;
pub use subscriber::*;
pub use track_status_requested::*;

use reader::*;
use writer::*;

use futures::{stream::FuturesUnordered, StreamExt};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::coding::{Encode, KeyValuePairs, Value};
use crate::message::Message;
use crate::mlog;
use crate::watch::Queue;
use crate::{message, setup};
use std::path::PathBuf;

/// Registry mapping bidi-request IDs to a channel for forwarding responses.
/// When `run_send` pops a response message from the outgoing queue, it checks
/// this map: if the response's target request ID has a registered sender, the
/// response is forwarded to the bidi handler task that owns the Writer.
type BidiResponseMap = Arc<Mutex<HashMap<u64, tokio::sync::mpsc::UnboundedSender<Message>>>>;

/// A bidi response reader future handed from Publisher/Subscriber to
/// `Session::run`. Boxed so the publisher and subscriber reader loops can
/// share one channel and one `FuturesUnordered` type.
type BidiReaderFuture = Pin<Box<dyn Future<Output = Result<(), SessionError>> + Send + 'static>>;

/// Channel for bidi response reader futures. Publisher/Subscriber send boxed
/// futures here; `Session::run` polls them cooperatively under structured
/// concurrency — no task is spawned, so a reader cannot outlive the session.
type BidiTaskSender = tokio::sync::mpsc::UnboundedSender<BidiReaderFuture>;

/// The transport protocol negotiated for this MoQT connection.
///
/// MoQT can run over either WebTransport (HTTP/3 + QUIC) or raw QUIC.
/// The transport type affects protocol behavior — for example, the PATH
/// parameter is only sent in SETUP for raw QUIC connections,
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
    /// ALPN: "moqt-16". Path carried in SETUP PATH parameter.
    RawQuic,
}

/// Session-level configuration supplied at connect/accept time.
///
/// Draft-18 removed MAX_REQUEST_ID and REQUESTS_BLOCKED, so there is no longer
/// a negotiated request budget to advertise in SETUP. `max_request_id` is
/// retained so callers that were written against the draft-16 API (including
/// the relay's `--max-request-id` flag) keep working, but it is not sent on the
/// wire and does not bound anything on this branch. Inbound request IDs are
/// still bounded per session by `request_id::MAX_REQUEST_IDS_PER_SESSION`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct SessionConfig {
    /// Accepted for source compatibility with draft-16 callers. Unused.
    pub max_request_id: u64,
}

/// The draft-16 default, kept so `SessionConfig::default()` compares equal to
/// what existing callers construct.
const DEFAULT_MAX_REQUEST_ID: u64 = 100;

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

    /// Session-level request ID manager.
    /// Publisher and Subscriber share one outbound request ID sequence.
    request_id: RequestId,

    /// Optional mlog writer for MoQ Transport events
    /// Wrapped in Arc<Mutex<>> to share across send/recv tasks when enabled
    mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,

    /// The transport protocol negotiated for this connection.
    transport: Transport,

    /// The connection path, derived from the WebTransport URL path or SETUP PATH parameter.
    /// For incoming connections: extracted during accept() from the WebTransport CONNECT URL
    /// (takes precedence) or the SETUP PATH parameter (key 0x1).
    /// For outgoing connections: auto-extracted from the session URL in connect().
    connection_path: Option<String>,

    /// Receiver for bidi reader futures.
    /// Polled by Session::run; dropping FuturesUnordered cancels all in-flight futures.
    bidi_task_rx: tokio::sync::mpsc::UnboundedReceiver<BidiReaderFuture>,

    /// Maps bidi-request IDs to their response stream writers (draft-18).
    bidi_response_map: BidiResponseMap,
}

impl Session {
    const MAX_CONNECTION_PATH_LEN: usize = 1024;

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
    /// 2. The SETUP PATH parameter (key 0x1), used for raw QUIC connections.
    ///
    /// Returns `None` if no path was present or if the path was just "/".
    pub fn connection_path(&self) -> Option<&str> {
        self.connection_path.as_deref()
    }

    /// Log a control message with structured fields for observability.
    /// Uses target "moq_transport::control" so it can be filtered independently.
    fn log_control_message(msg: &Message, direction: &str) {
        match msg {
            Message::Subscribe(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
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
                    direction,
                    msg_type = "UNSUBSCRIBE",
                    subscribe_id = m.id,
                    "MoQT control message"
                );
            }
            Message::PublishNamespace(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
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
                    direction,
                    msg_type = "PUBLISH_NAMESPACE_DONE",
                    request_id = m.id,
                    "MoQT control message"
                );
            }
            Message::Namespace(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    direction,
                    msg_type = "NAMESPACE",
                    namespace_suffix = %m.track_namespace_suffix,
                    "MoQT control message"
                );
            }
            Message::NamespaceDone(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    direction,
                    msg_type = "NAMESPACE_DONE",
                    namespace_suffix = %m.track_namespace_suffix,
                    "MoQT control message"
                );
            }
            Message::PublishNamespaceCancel(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    direction,
                    msg_type = "PUBLISH_NAMESPACE_CANCEL",
                    request_id = m.id,
                    error_code = m.error_code,
                    reason = %m.reason_phrase.0,
                    "MoQT control message"
                );
            }
            Message::TrackStatus(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
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
                    direction,
                    msg_type = "FETCH_CANCEL",
                    request_id = m.id,
                    "MoQT control message"
                );
            }
            Message::Publish(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
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
                    direction,
                    msg_type = "PUBLISH_OK",
                    request_id = m.id,
                    "MoQT control message"
                );
            }
            Message::PublishDone(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    direction,
                    msg_type = "PUBLISH_DONE",
                    request_id = m.id,
                    status_code = m.status_code,
                    stream_count = m.stream_count,
                    "MoQT control message"
                );
            }
            Message::GoAway(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    direction,
                    msg_type = "GOAWAY",
                    uri = %m.uri.0,
                    "MoQT control message"
                );
            }
            Message::RequestOk(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    direction,
                    msg_type = "REQUEST_OK",
                    request_id = m.id,
                    "MoQT control message"
                );
            }
            Message::RequestError(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    direction,
                    msg_type = "REQUEST_ERROR",
                    request_id = m.id,
                    error_code = m.error_code,
                    retry_interval = m.retry_interval,
                    "MoQT control message"
                );
            }
            Message::RequestUpdate(m) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    direction,
                    msg_type = "REQUEST_UPDATE",
                    request_id = m.id,
                    existing_request_id = m.existing_request_id,
                    "MoQT control message"
                );
            }
        }
    }

    fn new(
        webtransport: web_transport::Session,
        sender: Writer,
        recver: Reader,
        mlog: Option<mlog::MlogWriter>,
        transport: Transport,
        connection_path: Option<String>,
        request_id: RequestId,
    ) -> (Self, Option<Publisher>, Option<Subscriber>) {
        let outgoing = Queue::default().split();

        // Wrap mlog in Arc<Mutex<>> for sharing across tasks
        let mlog_shared = mlog.map(|m| Arc::new(Mutex::new(m)));

        let (bidi_task_tx, bidi_task_rx) =
            tokio::sync::mpsc::unbounded_channel::<BidiReaderFuture>();

        let publisher = Some(Publisher::new(
            outgoing.0.clone(),
            webtransport.clone(),
            mlog_shared.clone(),
            request_id.clone(),
            bidi_task_tx.clone(),
        ));
        let subscriber = Some(Subscriber::new(
            outgoing.0,
            webtransport.clone(),
            mlog_shared.clone(),
            request_id.clone(),
            bidi_task_tx,
        ));

        let session = Self {
            webtransport,
            sender,
            recver,
            publisher: publisher.clone(),
            subscriber: subscriber.clone(),
            outgoing: outgoing.1,
            request_id,
            mlog: mlog_shared,
            transport,
            connection_path,
            bidi_task_rx,
            bidi_response_map: Arc::new(Mutex::new(HashMap::new())),
        };

        (session, publisher, subscriber)
    }

    /// Create an outbound/client QUIC connection.
    ///
    /// Opens a unidirectional control stream, sends SETUP with
    /// parameters only (version is agreed via ALPN), and waits for SETUP.
    ///
    /// For native `moqt://` connections the PATH and AUTHORITY parameters are
    /// sent automatically.  For WebTransport the path is carried in the HTTP/3
    /// CONNECT URL so PATH is not sent.
    pub async fn connect(
        session: web_transport::Session,
        mlog_path: Option<PathBuf>,
        transport: Transport,
    ) -> Result<(Session, Publisher, Subscriber), SessionError> {
        Self::connect_with_config(session, mlog_path, transport, SessionConfig::default()).await
    }

    /// Create an outbound/client QUIC connection with explicit session config.
    ///
    /// See [`SessionConfig`]: draft-18 has no negotiated request budget, so the
    /// config currently carries nothing that affects the connection. The entry
    /// point exists so callers can keep passing one.
    pub async fn connect_with_config(
        session: web_transport::Session,
        mlog_path: Option<PathBuf>,
        transport: Transport,
        _config: SessionConfig,
    ) -> Result<(Session, Publisher, Subscriber), SessionError> {
        let url = session.url().clone();
        let url_path = url.path();
        let path = Self::normalize_connection_path(url_path)?;

        let mlog = mlog_path.and_then(|p| {
            mlog::MlogWriter::new(p)
                .map_err(|e| tracing::warn!("Failed to create mlog: {}", e))
                .ok()
        });

        // Open our unidirectional control send stream.
        let send_stream = session.open_uni().await?;
        let mut sender = Writer::new(send_stream);

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

        let client = setup::Setup { params };

        tracing::debug!(
            target: "moq_transport::control",
            direction = "sent",
            msg_type = "SETUP",
            ?transport,
            path = path.as_deref(),
            "MoQT control message"
        );
        sender.encode(&client).await?;

        // Accept the peer's unidirectional control stream.
        let recv_stream = session.accept_uni().await?;
        let mut recver = Reader::new(recv_stream);
        let _server: setup::Setup = recver.decode().await?;
        tracing::debug!(
            target: "moq_transport::control",
            direction = "recv",
            msg_type = "SETUP (recv)",
            "MoQT control message"
        );

        // Client sends even IDs (0); peer server sends odd IDs (1).
        let request_id = RequestId::new(0, 1);
        let session = Session::new(session, sender, recver, mlog, transport, path, request_id);
        Ok((session.0, session.1.unwrap(), session.2.unwrap()))
    }

    /// Accept an inbound server connection.
    ///
    /// Opens a unidirectional control stream and accepts the peer.s, decodes SETUP,
    /// sends SETUP with parameters only.  Version is already agreed
    /// via ALPN before this is called.
    pub async fn accept(
        session: web_transport::Session,
        mlog_path: Option<PathBuf>,
        transport: Transport,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        Self::accept_with_config(session, mlog_path, transport, SessionConfig::default()).await
    }

    /// Accept an inbound server connection with explicit session config.
    ///
    /// See [`SessionConfig`] for why the config is currently inert under draft-18.
    pub async fn accept_with_config(
        session: web_transport::Session,
        mlog_path: Option<PathBuf>,
        transport: Transport,
        _config: SessionConfig,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        let mut mlog = mlog_path.and_then(|p| {
            mlog::MlogWriter::new(p)
                .map_err(|e| tracing::warn!("Failed to create mlog: {}", e))
                .ok()
        });

        // Open our unidirectional control send stream.
        let send_stream = session.open_uni().await?;
        let mut sender = Writer::new(send_stream);

        // Accept the peer's unidirectional control stream.
        let recv_stream = session.accept_uni().await?;
        let mut recver = Reader::new(recv_stream);

        let client: setup::Setup = recver.decode().await?;
        tracing::debug!(
            target: "moq_transport::control",
            direction = "recv",
            msg_type = "SETUP",
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
                connection_path = connection_path.as_deref(),
                "Connection path resolved"
            );
        }

        if let Some(ref mut mlog) = mlog {
            let event = mlog::events::client_setup_parsed(mlog.elapsed_ms(), 0, &client);
            let _ = mlog.add_event(event);
        }

        let params = KeyValuePairs::default();

        let server = setup::Setup { params };

        tracing::debug!(
            target: "moq_transport::control",
            direction = "sent",
            msg_type = "SETUP (recv)",
            "MoQT control message"
        );

        if let Some(ref mut mlog) = mlog {
            let event = mlog::events::server_setup_created(mlog.elapsed_ms(), 0, &server);
            let _ = mlog.add_event(event);
        }

        sender.encode(&server).await?;

        // Server sends odd IDs (1); peer client sends even IDs (0).
        let request_id = RequestId::new(1, 0);
        Ok(Session::new(
            session,
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
        let mut bidi_task_rx = self.bidi_task_rx;
        let mut reader_tasks: FuturesUnordered<BidiReaderFuture> = FuturesUnordered::new();

        let result = tokio::select! {
            res = Self::run_recv(self.recver, self.publisher.clone(), self.subscriber.clone(), self.mlog.clone(), self.request_id.clone(), self.outgoing.clone()) => res,
            res = Self::run_send(self.sender, self.outgoing, self.mlog.clone(), self.bidi_response_map.clone()) => res,
            res = Self::run_bidi_requests(self.webtransport.clone(), self.publisher.clone(), self.subscriber.clone(), self.request_id.clone(), self.bidi_response_map.clone(), self.mlog.clone()) => res,
            res = Self::run_streams(self.webtransport.clone(), self.subscriber.clone()) => res,
            res = Self::run_datagrams(self.webtransport, self.subscriber) => res,
            // Collect bidi reader futures and poll them cooperatively as part of
            // this select. They progress only while run() is polled, so they
            // cannot outlive the session (true structured concurrency).
            res = async {
                loop {
                    tokio::select! {
                        fut = bidi_task_rx.recv() => {
                            match fut {
                                Some(f) => reader_tasks.push(f),
                                None => break, // all senders dropped
                            }
                        }
                        Some(res) = reader_tasks.next() => {
                            // A reader ending is normal; a protocol violation is not.
                            // Surfacing the latter here is what closes the session with
                            // the registered code instead of logging and carrying on.
                            if let Err(err) = res {
                                if err.is_session_fatal() {
                                    return Err(err);
                                }
                                tracing::debug!(error = %err, "bidi response reader ended");
                            }
                        }
                    }
                }
                Ok(())
            } => res,
        };

        // Dropping reader_tasks (FuturesUnordered<BidiReaderFuture>) drops every
        // in-flight reader future, cancelling them — no task abort needed since
        // nothing was ever spawned onto the runtime.
        drop(reader_tasks);

        result
    }

    /// Processes the outgoing control message queue. Response messages targeting
    /// a bidi request stream are redirected there (draft-18); everything else
    /// goes to the control stream.
    async fn run_send(
        mut sender: Writer,
        mut outgoing: Queue<message::Message>,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
        bidi_response_map: BidiResponseMap,
    ) -> Result<(), SessionError> {
        while let Some(msg) = outgoing.pop().await {
            Self::log_control_message(&msg, "sent");

            if let Some(ref mlog) = mlog {
                if let Ok(mut mlog_guard) = mlog.lock() {
                    let time = mlog_guard.elapsed_ms();
                    // Draft-18: Subscribe and PublishNamespace travel on bidi
                    // request streams, not the control stream. Use the request
                    // ID as the stream identifier for mlog; only GoAway still
                    // uses stream 0 (control).
                    let event = match &msg {
                        Message::Subscribe(m) => {
                            Some(mlog::events::subscribe_created(time, m.id, m))
                        }
                        Message::SubscribeOk(m) => {
                            Some(mlog::events::subscribe_ok_created(time, m.id, m))
                        }
                        Message::Unsubscribe(m) => {
                            Some(mlog::events::unsubscribe_created(time, m.id, m))
                        }
                        Message::PublishNamespace(m) => {
                            Some(mlog::events::publish_namespace_created(time, m.id, m))
                        }
                        Message::GoAway(m) => Some(mlog::events::go_away_created(time, 0, m)),
                        _ => None,
                    };
                    if let Some(event) = event {
                        let _ = mlog_guard.add_event(event);
                    }
                }
            }

            // Draft-18: response messages with a target request ID belong on
            // the bidi stream, never the control stream.
            if let Some(target_id) = msg.response_target_id() {
                let tx_opt = bidi_response_map
                    .lock()
                    .map_err(|_| SessionError::Internal)?
                    .get(&target_id)
                    .cloned();
                if let Some(tx) = tx_opt {
                    if tx.send(msg).is_err() {
                        tracing::warn!(target_id, "bidi response channel closed, dropping message");
                    }
                } else {
                    tracing::warn!(
                        target_id,
                        "bidi response map entry gone, dropping late response"
                    );
                }
                continue; // never fall through to control stream for bidi-only messages
            }

            // Only control-stream messages (no response_target_id) reach here.
            sender.encode(&msg).await?;
        }

        Ok(())
    }

    /// Accept incoming bidirectional request streams (draft-18 §10).
    /// Each peer-initiated bidi stream carries one request message followed
    /// by responses/follow-ups on the same stream.
    /// Maximum number of bidi request handler tasks running concurrently.
    /// Provides back-pressure when a peer opens many streams at once.
    const MAX_CONCURRENT_BIDI_STREAMS: usize = 128;

    /// Upper bound on how long a bidi request handler waits, applied to two
    /// phases only: (a) the initial request read, and (b) the response phase of
    /// one-shot request types (TRACK_STATUS, REQUEST_UPDATE).
    ///
    /// Long-lived request types (SUBSCRIBE, PUBLISH_NAMESPACE, PUBLISH,
    /// SUBSCRIBE_NAMESPACE, FETCH) intentionally keep their bidi stream open
    /// until an explicit terminal message, so they are deliberately NOT bounded
    /// here — bounding them would break the protocol. For those, the QUIC
    /// connection idle timeout (configured by the transport layer; e.g.
    /// moq-native-ietf sets 10s) reclaims a slot held by a dead peer.
    ///
    /// This timeout closes the remaining gap: an *alive but misbehaving* peer
    /// that keeps the connection alive (so the QUIC idle timeout never fires)
    /// could otherwise pin a `MAX_CONCURRENT_BIDI_STREAMS` slot forever by
    /// opening a stream and never sending a complete request, or by stalling a
    /// one-shot request after it is accepted. Both are bounded below.
    const BIDI_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    async fn run_bidi_requests(
        webtransport: web_transport::Session,
        publisher: Option<Publisher>,
        subscriber: Option<Subscriber>,
        request_id: RequestId,
        bidi_response_map: BidiResponseMap,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();

        loop {
            tokio::select! {
                res = webtransport.accept_bi(), if tasks.len() < Self::MAX_CONCURRENT_BIDI_STREAMS => {
                    let (send_stream, recv_stream) = res?;
                    let mut pub_clone = publisher.clone();
                    let mut sub_clone = subscriber.clone();
                    let rid = request_id.clone();
                    let map = bidi_response_map.clone();
                    let mlog = mlog.clone();

                    tasks.push(async move {
                        Self::handle_bidi_request(
                            send_stream, recv_stream,
                            &mut pub_clone, &mut sub_clone, &rid, &map, mlog,
                        ).await
                    });
                }
                Some(res) = tasks.next() => {
                    // Same split as the outbound readers: an inbound request stream
                    // ending is routine, but a peer that violated a MUST rule has to
                    // take the session down, which cannot happen if we only log here.
                    if let Err(err) = res {
                        if err.is_session_fatal() {
                            return Err(err);
                        }
                        tracing::debug!(error = %err, "bidi request stream ended");
                    }
                }
            }
        }
    }

    /// Handle a single bidi request stream: decode the request, dispatch
    /// to handlers, then wait for responses and write them back on the
    /// same stream (without Request ID, per draft-18).
    async fn handle_bidi_request(
        send_stream: web_transport::SendStream,
        recv_stream: web_transport::RecvStream,
        publisher: &mut Option<Publisher>,
        subscriber: &mut Option<Subscriber>,
        request_id: &RequestId,
        bidi_response_map: &BidiResponseMap,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        let mut reader = Reader::new(recv_stream);
        let mut writer = Writer::new(send_stream);

        // Read the first (request) message from the bidi stream. Bound this so a
        // peer that opens a bidi stream but never sends a complete request
        // cannot pin a concurrency slot indefinitely.
        let msg: Message =
            match tokio::time::timeout(Self::BIDI_REQUEST_TIMEOUT, reader.decode()).await {
                Ok(Ok(msg)) => msg,
                Ok(Err(e)) => {
                    // A conformant peer always sends a decodable request as the
                    // first message on a bidi stream. A decode failure here means
                    // a malformed or wire-incompatible request (e.g. a parameter
                    // value-encoding mismatch) that would otherwise be dropped
                    // invisibly at debug level — surface it at warn so this class
                    // of interop break is observable in production.
                    tracing::warn!(error = %e, "failed to decode inbound request; dropping stream");
                    return Err(e);
                }
                Err(_) => {
                    tracing::debug!("bidi request read timed out; reclaiming slot");
                    return Ok(());
                }
            };

        // SUBSCRIBE_NAMESPACE owns its stream for the whole life of the request:
        // the reply is not one terminal message but an open-ended NAMESPACE /
        // NAMESPACE_DONE feed. Hand both halves straight to the publisher-side
        // handler rather than routing through `bidi_response_map`, which can
        // only key on messages that carry a Request ID — NAMESPACE does not.
        if let Message::SubscribeNamespace(msg) = msg {
            if let Some(ref mlog) = mlog {
                if let Ok(mut mlog) = mlog.lock() {
                    let time = mlog.elapsed_ms();
                    let _ = mlog.add_event(mlog::events::subscribe_namespace_parsed(time, 0, &msg));
                }
            }

            request_id.validate_incoming(msg.id)?;
            let recv = publisher
                .as_mut()
                .ok_or(SessionError::RoleViolation)?
                .recv_subscribe_namespace(msg)?;
            return recv.run(writer, reader, mlog).await;
        }

        // Classify the request. One-shot request/response flows are bounded in
        // the response phase below; long-lived flows (SUBSCRIBE, PUBLISH,
        // PUBLISH_NAMESPACE, FETCH) may legitimately stay open until an explicit
        // terminal message and are therefore not bounded.
        let bounded_response = matches!(&msg, Message::TrackStatus(_) | Message::RequestUpdate(_));

        let req_id = msg.sequenced_request_id();

        // Validate request ID sequencing and register a response channel.
        let mut rx = if let Some(id) = req_id {
            request_id.validate_incoming(id)?;

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
            bidi_response_map
                .lock()
                .map_err(|_| SessionError::Internal)?
                .insert(id, tx);
            Some(rx)
        } else {
            None
        };

        // Dispatch to the appropriate role handler (same as run_recv).
        // Capture the result so cleanup runs unconditionally on error.
        let mut dispatch_result = (|| -> Result<(), SessionError> {
            let msg = match TryInto::<message::Publisher>::try_into(msg) {
                Ok(msg) => {
                    subscriber
                        .as_mut()
                        .ok_or(SessionError::RoleViolation)?
                        .recv_message(msg)?;
                    return Ok(());
                }
                Err(msg) => msg,
            };
            match TryInto::<message::Subscriber>::try_into(msg) {
                Ok(msg) => {
                    publisher
                        .as_mut()
                        .ok_or(SessionError::RoleViolation)?
                        .recv_message(msg)?;
                }
                Err(msg) => {
                    tracing::warn!(
                        msg_type = msg.name(),
                        "unexpected message on bidi request stream"
                    );
                }
            }
            Ok(())
        })();

        // Wait for responses only if dispatch succeeded; otherwise the
        // handlers didn't register anything to respond to.
        if dispatch_result.is_ok() {
            if let Some(id) = req_id {
                if let Some(ref mut rx) = rx {
                    let pump = async {
                        while let Some(response) = rx.recv().await {
                            // One-shot requests (TRACK_STATUS, REQUEST_UPDATE) get
                            // exactly one reply, so REQUEST_OK ends them just as
                            // REQUEST_ERROR does. Draft-18 requires the stream be
                            // FINned after TRACK_STATUS_OK or REQUEST_ERROR
                            // (§10.14). Treating only the error as terminal held the
                            // handler slot until BIDI_REQUEST_TIMEOUT, so 128 cheap
                            // TRACK_STATUS requests could occupy every slot for 30s
                            // even when the application answered immediately.
                            let is_terminal = matches!(
                                response,
                                Message::RequestError(_)
                                    | Message::PublishDone(_)
                                    | Message::PublishNamespaceDone(_)
                                    | Message::Unsubscribe(_)
                            ) || (bounded_response
                                && matches!(response, Message::RequestOk(_)));
                            if let Err(e) = Self::encode_bidi_response(&mut writer, &response).await
                            {
                                tracing::warn!(error = %e, "failed to write bidi response");
                                break;
                            }
                            if is_terminal {
                                // Give Quinn's connection driver a scheduling
                                // opportunity to transmit the STREAM data before
                                // the Writer is dropped (which sends FIN).
                                tokio::time::sleep(std::time::Duration::ZERO).await;
                                break;
                            }
                        }
                    };

                    let follow_ups =
                        Self::read_request_follow_ups(&mut reader, id, publisher, subscriber);

                    // Whichever side finishes first ends the request, so the
                    // other is cancelled. `follow_ups` is not cancellation-safe
                    // (a partly-read frame is lost), which is fine only because
                    // neither `reader` nor `writer` is used again afterwards.
                    let both = async {
                        tokio::select! {
                            () = pump => Ok(()),
                            res = follow_ups => res,
                        }
                    };

                    // One-shot requests are bounded so a stalled peer cannot pin a
                    // slot after the request is accepted. Long-lived requests wait
                    // as long as needed; the QUIC idle timeout backstops a dead peer.
                    // A protocol violation seen while reading follow-ups has to
                    // reach the caller, which closes the session. Timing out is
                    // not a violation: the slot is reclaimed and the session lives.
                    dispatch_result = if bounded_response {
                        match tokio::time::timeout(Self::BIDI_REQUEST_TIMEOUT, both).await {
                            Ok(res) => res,
                            Err(_) => {
                                tracing::debug!(
                                    ?req_id,
                                    "one-shot bidi request timed out awaiting response; reclaiming slot"
                                );
                                Ok(())
                            }
                        }
                    } else {
                        both.await
                    };
                }
            }
        }

        // The request stream is over. Release everything keyed by this request
        // ID — a stream that dies without a terminal message would otherwise
        // leak its state (and leave the application blocked on `closed()`) for
        // the life of the session. Runs on both success and error paths.
        if let Some(id) = req_id {
            if let Some(subscriber) = subscriber.as_ref() {
                subscriber.abort_publish_received(id);
            }
            if let Ok(mut map) = bidi_response_map.lock() {
                map.remove(&id);
            }
        }

        // Explicitly finish the stream and yield for Quinn to flush.
        let _ = writer.finish();
        tokio::task::yield_now().await;

        dispatch_result
    }

    /// Read messages the requester sends after its opening request on the same
    /// bidi stream, and dispatch them like any other inbound control message.
    ///
    /// Draft-18 puts the whole life of a request on one stream, so terminal
    /// follow-ups such as PUBLISH_DONE (ending an inbound PUBLISH) and
    /// UNSUBSCRIBE (ending an inbound SUBSCRIBE) arrive here rather than on the
    /// control stream. Like every other message after the request, they omit
    /// the Request ID, which is why the stream's known `request_id` is injected.
    ///
    /// Returns when the request ends: the requester reset the stream, or sent
    /// something terminal. A clean FIN of the requester's send half is not an
    /// ending, so this parks instead: this endpoint FINs its own SUBSCRIBE stream
    /// immediately after sending the request and a conformant peer may do the
    /// same, and the caller keeps the response side running.
    ///
    /// `Err` means the peer violated the protocol, which the caller turns into a
    /// session close.
    async fn read_request_follow_ups(
        reader: &mut Reader,
        request_id: u64,
        publisher: &mut Option<Publisher>,
        subscriber: &mut Option<Subscriber>,
    ) -> Result<(), SessionError> {
        loop {
            let msg = match Self::decode_bidi_response(reader, request_id).await {
                Ok(msg) => msg,
                // A reset or STOP_SENDING ends the request itself, so returning
                // here is what releases the handler slot. Parking on this would
                // let a peer pin one slot per reset stream.
                Err(err) if err.is_stream_error() => {
                    tracing::debug!(request_id, error = %err, "request stream reset by peer");
                    return Ok(());
                }
                // A clean FIN of the requester's send half is different: it only
                // means no more requests are coming on this stream, not that the
                // request is over. This endpoint FINs its own SUBSCRIBE stream
                // right after sending, and a conformant peer may too, so parking
                // keeps the response side alive.
                Err(err) if err.is_stream_ended() => {
                    tracing::trace!(request_id, error = %err, "request stream send half finished");
                    std::future::pending::<()>().await;
                    return Ok(());
                }
                // Draft-18 requires closing the session on an unknown message
                // type, so an undecodable frame is not something to skip past.
                Err(err) => return Err(err),
            };

            let terminal = matches!(
                msg,
                Message::PublishDone(_)
                    | Message::PublishNamespaceDone(_)
                    | Message::Unsubscribe(_)
            );

            let dispatched = match TryInto::<message::Publisher>::try_into(msg) {
                Ok(msg) => subscriber
                    .as_mut()
                    .ok_or(SessionError::RoleViolation)
                    .and_then(|s| s.recv_message(msg)),
                Err(msg) => match TryInto::<message::Subscriber>::try_into(msg) {
                    Ok(msg) => publisher
                        .as_mut()
                        .ok_or(SessionError::RoleViolation)
                        .and_then(|p| p.recv_message(msg)),
                    Err(msg) => {
                        tracing::warn!(
                            request_id,
                            msg_type = msg.name(),
                            "unexpected follow-up on bidi request stream"
                        );
                        Ok(())
                    }
                },
            };

            if let Err(err) = dispatched {
                // Session::run decides: a protocol violation closes the session,
                // anything else only ends this request.
                return Err(err);
            }

            if terminal {
                return Ok(());
            }
        }
    }

    /// Encode a response message to a bidi stream, omitting the Request ID
    /// field per draft-18 (the stream identity provides the association).
    /// Build the wire frame for a bidi response message (type + length +
    /// payload with Request ID omitted). Separated from the async writer
    /// so tests can verify the encoding without a QUIC stream.
    fn encode_bidi_response_frame(msg: &Message) -> Result<bytes::BytesMut, SessionError> {
        use bytes::BufMut;

        // Encode the payload (all fields EXCEPT Request ID, which is
        // implicit from the bidi stream identity in draft-18).
        let mut payload = bytes::BytesMut::new();
        match msg {
            Message::RequestOk(m) => {
                m.params.encode(&mut payload)?;
            }
            Message::RequestError(m) => {
                m.error_code.encode(&mut payload)?;
                m.retry_interval.encode(&mut payload)?;
                m.reason.encode(&mut payload)?;
            }
            Message::SubscribeOk(m) => {
                m.track_alias.encode(&mut payload)?;
                m.params.encode(&mut payload)?;
                m.track_extensions.encode(&mut payload)?;
            }
            Message::PublishDone(m) => {
                m.status_code.encode(&mut payload)?;
                m.stream_count.encode(&mut payload)?;
                m.reason.encode(&mut payload)?;
            }
            // id-only messages: payload is empty (id omitted on bidi).
            Message::PublishNamespaceDone(_) | Message::Unsubscribe(_) => {}
            // NAMESPACE / NAMESPACE_DONE never carried a Request ID, so their
            // payload is unchanged on a bidi stream.
            Message::Namespace(m) => {
                m.track_namespace_suffix.encode(&mut payload)?;
            }
            Message::NamespaceDone(m) => {
                m.track_namespace_suffix.encode(&mut payload)?;
            }
            Message::PublishOk(m) => {
                m.params.encode(&mut payload)?;
            }
            Message::FetchOk(m) => {
                m.end_of_track.encode(&mut payload)?;
                m.end_location.encode(&mut payload)?;
                m.params.encode(&mut payload)?;
                m.track_extensions.encode(&mut payload)?;
            }
            other => {
                tracing::warn!(
                    msg_type = other.name(),
                    "unexpected message type in encode_bidi_response — not a bidi response message"
                );
                return Err(SessionError::Internal);
            }
        };

        let msg_type = msg.id();

        if payload.len() > u16::MAX as usize {
            return Err(crate::coding::EncodeError::MsgBoundsExceeded.into());
        }
        let mut frame = bytes::BytesMut::new();
        msg_type.encode(&mut frame)?;
        (payload.len() as u16).encode(&mut frame)?;
        frame.put(payload);
        Ok(frame)
    }

    async fn encode_bidi_response(writer: &mut Writer, msg: &Message) -> Result<(), SessionError> {
        let frame = Self::encode_bidi_response_frame(msg)?;
        writer.write(&frame).await?;
        Ok(())
    }

    /// Decode a response message from a bidi request stream (draft-18).
    ///
    /// Response messages omit the Request ID field — the stream identity
    /// provides the association. The caller supplies the known `request_id`
    /// which is injected into the decoded `Message`.
    pub(super) async fn decode_bidi_response(
        reader: &mut Reader,
        request_id: u64,
    ) -> Result<Message, SessionError> {
        use crate::coding::{Decode, DecodeError, ReasonPhrase};

        let msg_type: u64 = reader.decode().await?;
        let msg_len: u16 = reader.decode().await?;
        let len = msg_len as usize;

        let mut payload = bytes::BytesMut::new();
        while payload.len() < len {
            let remaining = len - payload.len();
            match reader.read_chunk(remaining).await? {
                Some(chunk) => payload.extend_from_slice(&chunk),
                None => return Err(DecodeError::More(remaining).into()),
            }
        }
        let mut buf = &payload[..];

        use message::wire_id;

        match msg_type {
            wire_id::RequestError => {
                let error_code = u64::decode(&mut buf)?;
                let retry_interval = u64::decode(&mut buf)?;
                let reason = ReasonPhrase::decode(&mut buf)?;
                Ok(Message::RequestError(message::RequestError {
                    id: request_id,
                    error_code,
                    retry_interval,
                    reason,
                }))
            }
            wire_id::RequestOk => {
                let params = crate::coding::KeyValuePairs::decode(&mut buf)?;
                Ok(Message::RequestOk(message::RequestOk {
                    id: request_id,
                    params,
                }))
            }
            wire_id::SubscribeOk => {
                let track_alias = u64::decode(&mut buf)?;
                let params = crate::coding::KeyValuePairs::decode(&mut buf)?;
                let track_extensions = message::TrackExtensions::decode(&mut buf)?;
                Ok(Message::SubscribeOk(message::SubscribeOk {
                    id: request_id,
                    track_alias,
                    params,
                    track_extensions,
                }))
            }
            wire_id::PublishNamespaceDone => Ok(Message::PublishNamespaceDone(
                message::PublishNamespaceDone { id: request_id },
            )),
            wire_id::PublishDone => {
                let status_code = u64::decode(&mut buf)?;
                let stream_count = u64::decode(&mut buf)?;
                let reason = ReasonPhrase::decode(&mut buf)?;
                Ok(Message::PublishDone(message::PublishDone {
                    id: request_id,
                    status_code,
                    stream_count,
                    reason,
                }))
            }
            wire_id::Unsubscribe => Ok(Message::Unsubscribe(message::Unsubscribe {
                id: request_id,
            })),
            // NAMESPACE / NAMESPACE_DONE have no Request ID field, so nothing
            // is injected — they decode exactly as they do on any other stream.
            wire_id::Namespace => {
                let track_namespace_suffix = crate::coding::TrackNamespacePrefix::decode(&mut buf)?;
                Ok(Message::Namespace(message::Namespace {
                    track_namespace_suffix,
                }))
            }
            wire_id::NamespaceDone => {
                let track_namespace_suffix = crate::coding::TrackNamespacePrefix::decode(&mut buf)?;
                Ok(Message::NamespaceDone(message::NamespaceDone {
                    track_namespace_suffix,
                }))
            }
            wire_id::PublishOk => {
                let params = crate::coding::KeyValuePairs::decode(&mut buf)?;
                Ok(Message::PublishOk(message::PublishOk {
                    id: request_id,
                    params,
                }))
            }
            wire_id::FetchOk => {
                let end_of_track = bool::decode(&mut buf)?;
                let end_location = crate::coding::Location::decode(&mut buf)?;
                let params = crate::coding::KeyValuePairs::decode(&mut buf)?;
                let track_extensions = message::TrackExtensions::decode(&mut buf)?;
                Ok(Message::FetchOk(message::FetchOk {
                    id: request_id,
                    end_of_track,
                    end_location,
                    params,
                    track_extensions,
                }))
            }
            other => {
                tracing::warn!(msg_type = other, "unexpected bidi response message type");
                Err(SessionError::unimplemented(&format!(
                    "bidi response type 0x{:x}",
                    other
                )))
            }
        }
    }

    /// Receives inbound messages from the control stream reader/receiver.
    /// Handles session-level messages (GOAWAY) directly and routes
    /// role-specific messages to Publisher or Subscriber.
    async fn run_recv(
        mut recver: Reader,
        mut publisher: Option<Publisher>,
        mut subscriber: Option<Subscriber>,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
        request_id: RequestId,
        _outgoing: Queue<Message>,
    ) -> Result<(), SessionError> {
        let mut goaway_received = false;

        loop {
            let msg: message::Message = recver.decode().await?;

            // Emit structured tracing log for received control messages
            Self::log_control_message(&msg, "recv");

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
                        new_uri = %m.uri.0,
                        "received GOAWAY"
                    );
                    // TODO(itzmanish): trigger session migration.
                }
                other => {
                    tracing::warn!(msg_type = other.name(), "received unhandled message type");
                    return Err(SessionError::unimplemented(&format!(
                        "message type {}",
                        other.name()
                    )));
                }
            }
        }
    }

    /// Accepts uni-directional quic streams and starts handling for them.
    /// Will read stream header to know what type of stream it is and create
    /// the appropriate stream handlers.
    async fn run_streams(
        webtransport: web_transport::Session,
        subscriber: Option<Subscriber>,
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();

        loop {
            tokio::select! {
                res = webtransport.accept_uni() => {
                    let stream = res?;
                    let subscriber = subscriber.clone().ok_or(SessionError::RoleViolation)?;

                    tasks.push(async move {
                        if let Err(err) = Subscriber::recv_stream(subscriber, stream).await {
                            tracing::warn!("failed to serve stream: {}", err);
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

/// Shared helpers for session-layer unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    /// Establish a real `web_transport::Session` over an in-process QUIC
    /// loopback so tests can construct a `Publisher`/`Subscriber` and exercise
    /// the real cleanup paths.
    ///
    /// Most tests never send anything over it — they only touch in-memory maps
    /// and queues — but `Publisher` and `Subscriber` hold a concrete
    /// `web_transport::Session`, so a live connection is the only honest way to
    /// build one. The accepted server side is parked for the lifetime of the test
    /// to keep the client session established.
    pub(crate) async fn loopback_session() -> web_transport::Session {
        use web_transport::quinn::{ClientBuilder, ServerBuilder};

        // Under `cargo test --workspace`, feature unification links both the
        // `ring` and `aws-lc-rs` rustls providers, leaving no unambiguous
        // process default. Install one explicitly (idempotent across tests).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("generate self-signed certificate");
        let cert_der = cert.cert.der().clone();
        let key = cert
            .key_pair
            .serialize_der()
            .try_into()
            .expect("serialize private key");

        let mut server = ServerBuilder::new()
            .with_addr("127.0.0.1:0".parse().expect("server addr"))
            .with_certificate(vec![cert_der], key)
            .expect("build loopback server");
        let addr = server.local_addr().expect("server local addr");

        tokio::spawn(async move {
            if let Some(request) = server.accept().await {
                // Hold the accepted server session open for the lifetime of the
                // test so the client side stays established; the spawned task is
                // cancelled at runtime shutdown when the test returns.
                if let Ok(_session) = request.ok().await {
                    std::future::pending::<()>().await;
                }
            }
        });

        let client = ClientBuilder::new()
            .dangerous()
            .with_no_certificate_verification()
            .expect("build loopback client");
        let url = url::Url::parse(&format!("https://127.0.0.1:{}/", addr.port()))
            .expect("parse loopback url");

        // Fail fast rather than hang CI if the loopback handshake ever stalls.
        tokio::time::timeout(std::time::Duration::from_secs(10), client.connect(url))
            .await
            .expect("loopback connect timed out")
            .expect("connect loopback session")
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ========================================================================
    // encode_bidi_response — verify wire format (no Request ID)
    // ========================================================================

    /// Helper: calls the production `encode_bidi_response_frame` and
    /// returns the raw bytes. No duplicate encoding logic.
    fn encode_bidi_response_bytes(msg: &Message) -> Vec<u8> {
        Session::encode_bidi_response_frame(msg).unwrap().to_vec()
    }

    #[test]
    fn encode_bidi_request_ok_omits_request_id() {
        use message::wire_id;
        let msg = Message::RequestOk(message::RequestOk {
            id: 42, // should NOT appear on the wire
            params: crate::coding::KeyValuePairs::default(),
        });
        let bytes = encode_bidi_response_bytes(&msg);
        // type (1 byte) + length 0x0001 (2 bytes) + params_count=0 (1 byte) = 4 bytes
        assert_eq!(bytes[0], wire_id::RequestOk as u8);
        assert_eq!(bytes.len(), 4);
    }

    #[test]
    fn encode_bidi_request_error_omits_request_id() {
        use message::wire_id;
        let msg = Message::RequestError(message::RequestError {
            id: 99, // should NOT appear on the wire
            error_code: 0x10,
            retry_interval: 0,
            reason: crate::coding::ReasonPhrase("nf".to_string()),
        });
        let bytes = encode_bidi_response_bytes(&msg);
        assert_eq!(bytes[0], wire_id::RequestError as u8);
        // No 99 (0x63) anywhere in the output
        assert!(
            !bytes.contains(&99),
            "Request ID must not appear in bidi encoding"
        );
    }

    /// NAMESPACE and NAMESPACE_DONE carry no Request ID field, so their bidi
    /// framing must be byte-identical to the ordinary message encoding.
    #[test]
    fn encode_bidi_namespace_matches_plain_message_encoding() {
        for msg in [
            Message::Namespace(message::Namespace {
                track_namespace_suffix: crate::coding::TrackNamespacePrefix::from_utf8_path(
                    "meeting=123",
                ),
            }),
            Message::NamespaceDone(message::NamespaceDone {
                track_namespace_suffix: crate::coding::TrackNamespacePrefix::from_utf8_path(
                    "meeting=123",
                ),
            }),
        ] {
            let mut plain = bytes::BytesMut::new();
            msg.encode(&mut plain).unwrap();

            assert_eq!(encode_bidi_response_bytes(&msg), plain.to_vec());
        }
    }

    #[test]
    fn response_target_id_covers_responses_only() {
        // Response messages should return Some(id)
        assert!(Message::RequestOk(message::RequestOk {
            id: 1,
            params: Default::default(),
        })
        .response_target_id()
        .is_some());
        assert!(Message::RequestError(message::RequestError {
            id: 1,
            error_code: 0,
            retry_interval: 0,
            reason: Default::default(),
        })
        .response_target_id()
        .is_some());

        // Request messages should return None
        assert!(Message::Subscribe(message::Subscribe {
            id: 1,
            track_namespace: crate::coding::TrackNamespace::from_utf8_path("t"),
            track_name: "n".into(),
            params: Default::default(),
        })
        .response_target_id()
        .is_none());
        assert!(Message::GoAway(message::GoAway {
            uri: crate::coding::SessionUri(String::new()),
        })
        .response_target_id()
        .is_none());
    }

    #[test]
    fn encode_bidi_publish_done_omits_request_id() {
        use message::wire_id;
        let msg = Message::PublishDone(message::PublishDone {
            id: 77,
            status_code: 0,
            stream_count: 3,
            reason: crate::coding::ReasonPhrase("done".to_string()),
        });
        let bytes = encode_bidi_response_bytes(&msg);
        assert_eq!(bytes[0], wire_id::PublishDone as u8);
        assert!(
            !bytes.contains(&77),
            "Request ID must not appear in bidi encoding"
        );
    }

    #[test]
    fn encode_bidi_publish_ok_omits_request_id() {
        use message::wire_id;
        let msg = Message::PublishOk(message::PublishOk {
            id: 55,
            params: crate::coding::KeyValuePairs::default(),
        });
        let bytes = encode_bidi_response_bytes(&msg);
        assert_eq!(bytes[0], wire_id::PublishOk as u8);
        assert!(
            !bytes.contains(&55),
            "Request ID must not appear in bidi encoding"
        );
    }

    #[test]
    fn encode_bidi_fetch_ok_omits_request_id() {
        use message::wire_id;
        let msg = Message::FetchOk(message::FetchOk {
            id: 88,
            end_of_track: true,
            end_location: crate::coding::Location::new(5, 10),
            params: crate::coding::KeyValuePairs::default(),
            track_extensions: Default::default(),
        });
        let bytes = encode_bidi_response_bytes(&msg);
        assert_eq!(bytes[0], wire_id::FetchOk as u8);
        assert!(
            !bytes.contains(&88),
            "Request ID must not appear in bidi encoding"
        );
    }
}
