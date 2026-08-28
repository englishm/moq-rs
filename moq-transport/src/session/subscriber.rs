// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    collections::{hash_map, HashMap},
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::Notify;

use crate::{
    coding::{Decode, KeyValuePairs, TrackName, TrackNamespace, TrackNamespacePrefix},
    data,
    message::{self, Message, SubscribeOptions},
    mlog,
    serve::{self, FullTrackName, ServeError},
};

use crate::watch::Queue;

use super::{
    DoneOutcome, NameRegistry, PublishReceived, PublishReceivedRecv, PublishedNamespace,
    PublishedNamespaceRecv, Reader, RequestId, Session, SessionError, Subscribe,
    SubscribeNamespace, SubscribeNamespaceInfo, SubscribeRecv, Writer,
};

// Default timeout for waiting for subscribe aliases to become available via SUBSCRIBE_OK (1 second)
const DEFAULT_ALIAS_WAIT_TIME_MS: u64 = 1000;

/// How long to keep a subscription alive after PUBLISH_DONE when the announced
/// Stream Count has not been reached (draft-18 §10.11).
///
/// §10.11 asks for "at least the larger of SUBGROUP_DELIVERY_TIMEOUT or
/// OBJECT_DELIVERY_TIMEOUT", but leaves the value undefined when neither is
/// negotiated, which is the common case. This fixed backstop bounds the state a
/// peer can hold open by never sending the streams it announced.
const PUBLISH_DONE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How many subscriptions may drain at once before PUBLISH_DONE is applied
/// immediately instead.
///
/// A draining subscription outlives its request stream by design, so without a
/// cap a peer could hold unbounded state by ending subscriptions it never sent
/// the announced streams for. §10.11 permits discarding state early ("A
/// subscriber MAY discard subscription state earlier, at the cost of
/// potentially not delivering some late objects"), which is the right trade
/// once a session is this far outside normal behaviour.
const MAX_CONCURRENT_DRAINS: usize = 256;

/// Rolls back a SUBSCRIBE_NAMESPACE prefix reservation if the request never
/// gets off the ground.
///
/// `subscribe_namespace()` reserves the prefix before opening the request
/// stream so concurrent callers see the overlap immediately. If the open then
/// fails, dropping this guard releases the reservation. On success the guard is
/// [`disarm`](Self::disarm)ed and cleanup passes to `SubscribeNamespaceRecv`,
/// whose `Drop` removes the entry when the subscription ends.
struct SubscribeNamespaceCleanup {
    subscriber: Subscriber,
    request_id: u64,
    active: bool,
}

impl SubscribeNamespaceCleanup {
    fn new(subscriber: Subscriber, request_id: u64) -> Self {
        Self {
            subscriber,
            request_id,
            active: true,
        }
    }

    fn disarm(mut self) {
        self.active = false;
    }
}

impl Drop for SubscribeNamespaceCleanup {
    fn drop(&mut self) {
        if self.active {
            self.subscriber.remove_subscribe_namespace(self.request_id);
        }
    }
}

/// Which subscription owns a given Track Alias (draft-16 §10.1).
///
/// SUBSCRIBE and PUBLISH share one session-scoped alias namespace, so a single
/// registry keyed by alias resolves inbound streams and datagrams to the right
/// receiver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackOrigin {
    /// Alias belongs to an outbound SUBSCRIBE; carries the subscribe request id.
    Subscribe(u64),
    /// Alias belongs to an inbound PUBLISH; carries the PUBLISH request id.
    Publish(u64),
}

impl TrackOrigin {
    fn request_id(self) -> u64 {
        match self {
            Self::Subscribe(id) | Self::Publish(id) => id,
        }
    }
}

#[derive(Default)]
struct TrackAliasRegistry {
    by_alias: HashMap<u64, TrackOrigin>,
    by_request_id: HashMap<u64, u64>,
}

impl TrackAliasRegistry {
    fn contains_alias(&self, alias: u64) -> bool {
        self.by_alias.contains_key(&alias)
    }

    fn get(&self, alias: u64) -> Option<TrackOrigin> {
        self.by_alias.get(&alias).copied()
    }

    fn insert(&mut self, alias: u64, origin: TrackOrigin) -> Result<(), SessionError> {
        if self.by_alias.contains_key(&alias) {
            return Err(SessionError::Duplicate);
        }

        if let Some(old_alias) = self.by_request_id.insert(origin.request_id(), alias) {
            self.by_alias.remove(&old_alias);
        }
        self.by_alias.insert(alias, origin);
        Ok(())
    }

    fn remove_by_request_id(&mut self, request_id: u64) -> Option<TrackOrigin> {
        let alias = self.by_request_id.remove(&request_id)?;
        self.by_alias.remove(&alias)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.by_alias.is_empty() && self.by_request_id.is_empty()
    }
}

// TODO remove Clone.
#[derive(Clone)]
pub struct Subscriber {
    /// Active inbound PUBLISH_NAMESPACE messages, keyed by namespace.
    published_namespaces: Arc<Mutex<HashMap<TrackNamespace, PublishedNamespaceRecv>>>,

    /// Queue of inbound PUBLISH_NAMESPACE events waiting to be consumed by the application.
    published_namespace_queue: Queue<PublishedNamespace>,

    /// The currently active outbound subscribes, keyed by request id.
    subscribes: Arc<Mutex<HashMap<u64, SubscribeRecv>>>,

    /// Prefixes of active outbound SUBSCRIBE_NAMESPACE requests, keyed by
    /// request id. Used to reject locally-overlapping prefixes (§5.1).
    subscribe_namespaces: Arc<Mutex<HashMap<u64, TrackNamespacePrefix>>>,

    /// Track Alias to owning subscription, for routing inbound streams and
    /// datagrams. Shared by outbound SUBSCRIBE and inbound PUBLISH.
    track_alias_map: Arc<Mutex<TrackAliasRegistry>>,

    /// Notify when `track_alias_map` is updated, for stream and datagram
    /// routing that can arrive before the alias is registered.
    track_alias_notify: Arc<Notify>,

    /// Tracks this endpoint subscribes to, for the §5.1 duplicate check.
    subscriber_names: Arc<Mutex<NameRegistry>>,

    /// Transport-side state for inbound PUBLISH requests, keyed by request id.
    publishes_received: Arc<Mutex<HashMap<u64, PublishReceivedRecv>>>,

    /// Inbound PUBLISH requests waiting to be consumed by the application.
    publish_received_queue: Queue<PublishReceived>,

    /// The queue we will write any outbound control messages we want to send, the session run_send task
    /// will process the queue and send the message on the control stream.
    outgoing: Queue<Message>,

    /// WebTransport session, used to open bidi streams for requests (draft-18).
    webtransport: web_transport::Session,

    /// Shared with Publisher so all requests within a session use unique IDs.
    /// When we need a new Request Id for sending a request, we can get it from here.
    /// The manager is shared with the Publisher, so the session uses unique request ids
    /// for all requests generated.  If we initiated the QUIC connection then request
    /// IDs start at 0 and increment by 2 (even numbers).  If we accepted an inbound
    /// QUIC connection then request IDs start at 1 and increment by 2 (odd numbers).
    request_id: RequestId,

    /// Optional mlog writer for logging transport events
    mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,

    /// Channel for sending bidi reader futures to `Session::run`, which polls
    /// them cooperatively under structured concurrency (no task is spawned).
    bidi_task_tx: super::BidiTaskSender,
}

impl Subscriber {
    pub(super) fn new(
        outgoing: Queue<Message>,
        webtransport: web_transport::Session,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
        request_id: RequestId,
        bidi_task_tx: super::BidiTaskSender,
    ) -> Self {
        Self {
            published_namespaces: Default::default(),
            published_namespace_queue: Default::default(),
            subscribes: Default::default(),
            subscribe_namespaces: Default::default(),
            outgoing,
            webtransport,
            request_id,
            mlog,
            track_alias_map: Default::default(),
            track_alias_notify: Arc::new(Notify::new()),
            subscriber_names: Default::default(),
            publishes_received: Default::default(),
            publish_received_queue: Default::default(),
            bidi_task_tx,
        }
    }

    /// Create an inbound/server QUIC connection, by accepting a bi-directional QUIC stream for control messages.
    pub async fn accept(
        session: web_transport::Session,
        transport: super::Transport,
    ) -> Result<(Session, Self), SessionError> {
        let (session, _, subscriber) = Session::accept(session, None, transport).await?;
        Ok((session, subscriber.ok_or(SessionError::Internal)?))
    }

    /// Create an outbound/client QUIC connection, by opening a bi-directional QUIC stream for control messages.
    pub async fn connect(
        session: web_transport::Session,
        transport: super::Transport,
    ) -> Result<(Session, Self), SessionError> {
        let (session, _, subscriber) = Session::connect(session, None, transport).await?;
        Ok((session, subscriber))
    }

    /// Wait for the next inbound PUBLISH_NAMESPACE from the peer, if any.
    pub async fn published_namespace(&mut self) -> Option<PublishedNamespace> {
        self.published_namespace_queue.pop().await
    }

    fn add_mlog_event<F>(&self, make_event: F)
    where
        F: FnOnce(f64) -> mlog::Event,
    {
        if let Some(ref mlog) = self.mlog {
            if let Ok(mut mlog) = mlog.lock() {
                let event = make_event(mlog.elapsed_ms());
                let _ = mlog.add_event(event);
            }
        }
    }

    fn log_request_ok_parsed(&self, request_kind: &str, msg: &message::RequestOk) {
        self.add_mlog_event(|time| mlog::events::request_ok_parsed(time, 0, request_kind, msg));
    }

    fn log_request_error_parsed(&self, request_kind: &str, msg: &message::RequestError) {
        self.add_mlog_event(|time| mlog::events::request_error_parsed(time, 0, request_kind, msg));
    }

    fn log_request_error_created(&self, request_kind: &str, msg: &message::RequestError) {
        self.add_mlog_event(|time| mlog::events::request_error_created(time, 0, request_kind, msg));
    }

    pub(super) fn send_request_ok(&mut self, request_kind: &str, msg: message::RequestOk) {
        self.add_mlog_event(|time| mlog::events::request_ok_created(time, 0, request_kind, &msg));
        self.send_message(msg);
    }

    pub(super) fn send_request_error(&mut self, request_kind: &str, msg: message::RequestError) {
        self.log_request_error_created(request_kind, &msg);
        self.send_message(msg);
    }

    /// Allocate the next outbound request ID.
    fn get_next_request_id(&mut self) -> Result<u64, SessionError> {
        self.request_id.allocate()
    }

    /// Open a bidirectional request stream (draft-18 §10), send a request
    /// message, and return the request stream's Writer (send side) together
    /// with a Reader for the response on the same stream.
    ///
    /// The Writer is returned (not dropped here) so the caller can keep the
    /// send side open and explicitly finish it on error paths, mirroring
    /// publisher.rs which holds its request writer in scope.
    async fn open_request_stream(
        &self,
        msg: &message::Message,
    ) -> Result<(Writer, Reader), SessionError> {
        // Validate and encode before allocating a QUIC stream, so malformed
        // local parameters cannot consume a request-stream slot.
        let frame = super::encode_request_frame(msg)?;

        let (send_stream, recv_stream) = self.webtransport.open_bi().await?;
        let mut writer = Writer::new(send_stream);
        writer.write(&frame).await?;
        Ok((writer, Reader::new(recv_stream)))
    }

    /// Send a TRACK_STATUS request for a track.
    pub fn track_status(
        &mut self,
        track_namespace: &TrackNamespace,
        track_name: impl Into<TrackName>,
    ) {
        let id = match self.get_next_request_id() {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "could not send TRACK_STATUS: request ID limit reached");
                return;
            }
        };
        self.send_message(message::TrackStatus {
            id,
            track_namespace: track_namespace.clone(),
            track_name: track_name.into(),
            params: Default::default(),
        });
        // TODO(itzmanish): make async and wait for response?
    }

    /// Subscribe to a track by creating a new subscribe request to the publisher.  Block until subscription is closed.
    pub async fn subscribe(&mut self, track: serve::TrackWriter) -> Result<(), ServeError> {
        let subscribe = self.subscribe_open(track).await?;
        subscribe.closed().await
    }

    /// Subscribe to a track and wait until the publisher acknowledges it.
    ///
    /// Draft-18: sends SUBSCRIBE on a new bidi request stream and reads
    /// the response (REQUEST_OK / REQUEST_ERROR) from the same stream.
    ///
    /// The caller must drive `Session::run` concurrently: the bidi response
    /// reader is polled by `run`, so the returned acknowledgement only
    /// resolves while `run` is being polled.
    pub async fn subscribe_open(
        &mut self,
        track: serve::TrackWriter,
    ) -> Result<Subscribe, ServeError> {
        self.subscribe_open_with_params(track, KeyValuePairs::default())
            .await
    }

    /// Like [`Self::subscribe_open`], but with SUBSCRIBE parameters.
    ///
    /// Draft-18 moved subscriber priority, group order, forward and the
    /// subscription filter out of fixed fields and into parameters, and added
    /// RENDEZVOUS_TIMEOUT (§10.2.6) for holding a subscription open until a
    /// publisher appears. None of that is reachable without a way to attach
    /// parameters, so this is the API that makes those features usable; the
    /// no-parameter form above delegates here.
    pub async fn subscribe_open_with_params(
        &mut self,
        track: serve::TrackWriter,
        params: KeyValuePairs,
    ) -> Result<Subscribe, ServeError> {
        let request_id = self
            .get_next_request_id()
            .map_err(|e| ServeError::internal_ctx(format!("request ID limit: {}", e)))?;

        // §5.1: at most one subscriber-role subscription per track. Outbound
        // SUBSCRIBE and inbound PUBLISH both make this endpoint the subscriber,
        // so they share one registry. Reserved before the stream is opened;
        // `remove_subscribe` releases it.
        let full_name = FullTrackName {
            namespace: track.namespace.clone(),
            name: track.name.clone(),
        };
        {
            let mut names = self
                .subscriber_names
                .lock()
                .map_err(|_| ServeError::internal_ctx("subscriber_names lock poisoned"))?;
            if names.contains_name(&full_name) {
                return Err(ServeError::Duplicate);
            }
            names.insert(full_name, request_id);
        }

        let (send, recv, subscribe) = match Subscribe::new(self.clone(), request_id, track, params)
        {
            Ok(subscribe) => subscribe,
            Err(err) => {
                if let Ok(mut names) = self.subscriber_names.lock() {
                    names.remove_by_request_id(request_id);
                }
                return Err(ServeError::internal_ctx(format!(
                    "invalid subscribe parameters: {err}"
                )));
            }
        };

        // Open a bidi stream and send the SUBSCRIBE message BEFORE
        // registering in the subscribes map — avoids a leaked entry if
        // open_request_stream fails. The wire message is the one built by
        // Subscribe::new, so it is not reconstructed here. The request writer
        // (send side) is held here so error paths can finish it explicitly.
        let subscribe_msg: Message = subscribe.into();
        let (mut request_writer, mut response_reader) =
            match self.open_request_stream(&subscribe_msg).await {
                Ok(streams) => streams,
                Err(e) => {
                    if let Ok(mut names) = self.subscriber_names.lock() {
                        names.remove_by_request_id(request_id);
                    }
                    return Err(ServeError::internal_ctx(format!(
                        "failed to open request stream: {}",
                        e
                    )));
                }
            };

        // Register the response state. If the lock is poisoned after the stream
        // is open, cleanly finish the send side (FIN) before bailing instead of
        // silently dropping it — mirrors the publisher.rs error-path handling.
        match self.subscribes.lock() {
            Ok(mut subscribes) => {
                subscribes.insert(request_id, recv);
            }
            Err(_) => {
                tracing::warn!(
                    request_id,
                    "subscribes lock poisoned after bidi stream open; finishing stream"
                );
                if let Ok(mut names) = self.subscriber_names.lock() {
                    names.remove_by_request_id(request_id);
                }
                let _ = request_writer.finish();
                return Err(ServeError::internal_ctx("subscribe lock poisoned"));
            }
        }

        // Hand a reader future for bidi stream responses (draft-18) to
        // Session::run, which polls it cooperatively (structured concurrency).
        // No task is spawned; the future is dropped/cancelled on session exit.
        let mut subscriber_clone = self.clone();
        let _ = self.bidi_task_tx.send(Box::pin(async move {
            let result = loop {
                match Session::decode_bidi_response(&mut response_reader, request_id).await {
                    Ok(msg) => {
                        if let Ok(pub_msg) = TryInto::<message::Publisher>::try_into(msg) {
                            // Returning rather than breaking lets Session::run decide:
                            // a protocol violation closes the session, anything else is
                            // logged there and only this stream ends.
                            if let Err(err) = subscriber_clone.recv_message(pub_msg) {
                                break Err(err);
                            }
                        }
                    }
                    Err(err) if err.is_stream_ended() => break Ok(()),
                    Err(err) => break Err(err),
                }
            };

            // Runs on every exit path. Harmless once the subscription is
            // established, since the entry is gone by then.
            subscriber_clone.abort_subscribe(request_id);
            result
        }));

        // Cleanly finish (FIN) the request stream's send side now that the
        // SUBSCRIBE has been flushed; we never write again on this stream.
        // Placed before `send.ok().await?` so the FIN is sent on every exit
        // path that holds the writer — both the happy path and a failed ack —
        // rather than letting the drop emit RESET_STREAM(0).
        if let Err(err) = request_writer.finish() {
            tracing::debug!(request_id, error = %err, "failed to FIN SUBSCRIBE request stream");
        }

        send.ok().await?;
        Ok(send)
    }

    /// Subscribe to namespace announcements under a prefix (draft-18 §10.18).
    ///
    /// The request gets its own bidirectional stream. REQUEST_OK / REQUEST_ERROR
    /// and the subsequent NAMESPACE / NAMESPACE_DONE feed all arrive on that
    /// stream, so the returned handle only progresses while `Session::run` is
    /// being polled.
    pub async fn subscribe_namespace(
        &mut self,
        namespace_prefix: TrackNamespacePrefix,
        subscribe_options: SubscribeOptions,
        params: KeyValuePairs,
    ) -> Result<SubscribeNamespace, SessionError> {
        let request_id = self.get_next_request_id()?;

        {
            let mut prefixes = self
                .subscribe_namespaces
                .lock()
                .map_err(|_| SessionError::Internal)?;
            if prefixes
                .values()
                .any(|existing| existing.overlaps(&namespace_prefix))
            {
                return Err(SessionError::Serve(ServeError::Duplicate));
            }
            prefixes.insert(request_id, namespace_prefix.clone());
        }
        let cleanup = SubscribeNamespaceCleanup::new(self.clone(), request_id);

        let msg = message::SubscribeNamespace {
            id: request_id,
            track_namespace_prefix: namespace_prefix.clone(),
            subscribe_options,
            params,
        };
        self.add_mlog_event(|time| mlog::events::subscribe_namespace_created(time, 0, &msg));

        let (writer, reader) = self
            .open_request_stream(&Message::SubscribeNamespace(msg))
            .await?;

        let info = SubscribeNamespaceInfo {
            request_id,
            namespace_prefix,
            subscribe_options,
        };
        let (send, recv) = SubscribeNamespace::new(self.clone(), info, writer);

        // The response reader is polled by `Session::run` (structured
        // concurrency): no task is spawned, so it cannot outlive the session.
        let mlog = self.mlog.clone();
        if self
            .bidi_task_tx
            .send(Box::pin(async move {
                match recv.run(reader, mlog).await {
                    Ok(()) => Ok(()),
                    Err(err) if err.is_stream_ended() => {
                        tracing::debug!(request_id, error = %err, "SUBSCRIBE_NAMESPACE response reader ended");
                        Ok(())
                    }
                    Err(err) => Err(err),
                }
            }))
            .is_err()
        {
            return Err(SessionError::Internal);
        }

        cleanup.disarm();
        Ok(send)
    }

    /// Drop the prefix reservation for an outbound SUBSCRIBE_NAMESPACE.
    pub(super) fn remove_subscribe_namespace(&self, request_id: u64) {
        if let Ok(mut prefixes) = self.subscribe_namespaces.lock() {
            prefixes.remove(&request_id);
        }
    }

    /// Send a message to the publisher via the control stream.
    pub(super) fn send_message<M: Into<message::Subscriber>>(&mut self, msg: M) {
        let msg = msg.into();

        // Remove our entry on terminal state.
        // Draft-16: PUBLISH_NAMESPACE_CANCEL carries Request ID, so look up
        // the namespace by iterating the map.
        if let message::Subscriber::PublishNamespaceCancel(msg) = &msg {
            let _ = self.drop_publish_namespace(msg.id);
        }

        // TODO report dropped messages?
        let _ = self.outgoing.push(msg.into());
    }

    /// Receive a message from the publisher via the control stream.
    pub(super) fn recv_message(&mut self, msg: message::Publisher) -> Result<(), SessionError> {
        match &msg {
            message::Publisher::PublishNamespace(msg) => self.recv_publish_namespace(msg)?,
            message::Publisher::PublishNamespaceDone(msg) => {
                self.recv_publish_namespace_done(msg)?;
            }
            message::Publisher::Publish(msg) => self.recv_publish(msg)?,
            message::Publisher::PublishDone(msg) => self.recv_publish_done(msg)?,
            message::Publisher::SubscribeOk(msg) => self.recv_subscribe_ok(msg)?,
            // Draft-16 shared responses (REQUEST_OK / REQUEST_ERROR).
            message::Publisher::RequestOk(msg) => self.recv_request_ok(msg)?,
            message::Publisher::RequestError(msg) => self.recv_request_error(msg)?,
            // FETCH_OK is part of draft-16, but FETCH is not implemented here yet.
            message::Publisher::FetchOk(msg) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    request_id = msg.id,
                    "received FETCH_OK for unsupported FETCH — ignoring"
                );
            }
        }

        Ok(())
    }

    /// Handle reception of an inbound PUBLISH_NAMESPACE from the publisher.
    fn recv_publish_namespace(
        &mut self,
        msg: &message::PublishNamespace,
    ) -> Result<(), SessionError> {
        let mut published_namespaces = self
            .published_namespaces
            .lock()
            .map_err(|_| SessionError::Internal)?;

        // Duplicate PUBLISH_NAMESPACE for the same namespace within a session is invalid.
        let entry = match published_namespaces.entry(msg.track_namespace.clone()) {
            hash_map::Entry::Occupied(_) => return Err(SessionError::Duplicate),
            hash_map::Entry::Vacant(entry) => entry,
        };

        let (published_ns, recv) =
            PublishedNamespace::new(self.clone(), msg.id, msg.track_namespace.clone());
        if let Err(published_ns) = self.published_namespace_queue.push(published_ns) {
            published_ns.close(ServeError::Cancel)?;
            return Ok(());
        }
        entry.insert(recv);

        Ok(())
    }

    /// Handle reception of PUBLISH_NAMESPACE_DONE from the publisher.
    fn recv_publish_namespace_done(
        &mut self,
        msg: &message::PublishNamespaceDone,
    ) -> Result<(), SessionError> {
        // Draft-16 §9.22: PUBLISH_NAMESPACE_DONE carries Request ID, not namespace.
        if let Some(recv) = self.drop_publish_namespace(msg.id) {
            recv.recv_done()?;
        }
        Ok(())
    }

    /// Handle the reception of a SubscribeOk message from the publisher.
    fn recv_subscribe_ok(&mut self, msg: &message::SubscribeOk) -> Result<(), SessionError> {
        if let Some(subscribe) = self
            .subscribes
            .lock()
            .map_err(|_| SessionError::Internal)?
            .get_mut(&msg.id)
        {
            // Track Aliases are session-scoped (§10.1), so the alias in
            // SUBSCRIBE_OK must not already be bound by another SUBSCRIBE or an
            // inbound PUBLISH.
            {
                let mut aliases = self
                    .track_alias_map
                    .lock()
                    .map_err(|_| SessionError::Internal)?;
                if aliases.contains_alias(msg.track_alias) {
                    return Err(SessionError::Duplicate);
                }
                aliases.insert(msg.track_alias, TrackOrigin::Subscribe(msg.id))?;
            }

            // Notify waiting tasks that the alias map has been updated
            self.track_alias_notify.notify_waiters();

            // Notify the subscribe of the successful subscription
            subscribe.ok(msg.track_alias)?;
        }

        Ok(())
    }

    /// Remove a subscribe from the active map, along with its alias and name
    /// reservations.
    /// Only releases the alias and name reservations when a subscription was
    /// actually removed. Request IDs are unique per session but the two maps
    /// are keyed independently, so clearing unconditionally would let a
    /// speculative lookup strip an inbound PUBLISH's reservations.
    pub(super) fn remove_subscribe(&self, id: u64) -> Option<SubscribeRecv> {
        let subscribe = self
            .subscribes
            .lock()
            .ok()
            .and_then(|mut s| s.remove(&id))?;
        if let Ok(mut aliases) = self.track_alias_map.lock() {
            aliases.remove_by_request_id(id);
        }
        if let Ok(mut names) = self.subscriber_names.lock() {
            names.remove_by_request_id(id);
        }
        Some(subscribe)
    }

    /// Handle an outbound SUBSCRIBE whose request stream ended.
    ///
    /// If nothing has been delivered yet this fails the request, so an
    /// application awaiting `Subscribe::ok()` does not wait for the life of the
    /// session on a request that can never be answered. If data is still
    /// arriving the subscription is drained instead, so in-flight Objects are
    /// not discarded. A no-op once the subscription has been removed by other
    /// means.
    pub(super) fn abort_subscribe(&mut self, id: u64) {
        // PUBLISH_DONE is terminal on the request stream, so this also runs
        // after a normal end of subscription. Data streams are independent of
        // it, so a subscription still draining — against an announced Stream
        // Count or against a stream it is mid-way through reading — is left for
        // the drain (or its timeout) to finish, or its in-flight Objects are
        // discarded.
        let outcome = match self.subscribes.lock() {
            Ok(mut subscribes) => match subscribes.get_mut(&id) {
                Some(recv) => recv.abort(ServeError::Cancel),
                None => return,
            },
            Err(_) => {
                tracing::error!(request_id = id, "subscribes lock poisoned");
                return;
            }
        };

        match outcome {
            DoneOutcome::AlreadyDraining => {
                tracing::debug!(
                    request_id = id,
                    "SUBSCRIBE request stream closed while draining; keeping state for in-flight streams"
                );
            }
            DoneOutcome::DrainArmed => self.begin_drain(
                id,
                Subscriber::expire_subscribe_drain,
                "SUBSCRIBE request stream closed with a stream still being read",
            ),
            DoneOutcome::Finished => {
                tracing::debug!(
                    request_id = id,
                    "SUBSCRIBE request stream closed before the publisher responded"
                );
                self.remove_subscribe(id);
            }
        }
    }

    /// Handle the reception of a PublishDone message from the publisher.
    ///
    /// PUBLISH_DONE terminates either a SUBSCRIBE-created subscription or a
    /// PUBLISH-created one. The request id alone does not say which, so both
    /// maps are checked.
    fn recv_publish_done(&mut self, msg: &message::PublishDone) -> Result<(), SessionError> {
        // `remove_subscribe` also releases the alias and name reservations for
        // the request ID, so it must only run for an ID that really belongs to
        // a SUBSCRIBE. Calling it speculatively would strip an inbound
        // PUBLISH's reservations out from under it.
        let is_subscribe = self
            .subscribes
            .lock()
            .map_err(|_| SessionError::Internal)?
            .contains_key(&msg.id);
        if is_subscribe {
            // Same §10.11 drain as the PUBLISH path below: a subscription is
            // only finished once the streams it announced have been received
            // and closed.
            let outcome = {
                let mut subscribes = self.subscribes.lock().map_err(|_| SessionError::Internal)?;
                match subscribes.get_mut(&msg.id) {
                    Some(recv) => {
                        recv.recv_done(ServeError::Closed(msg.status_code), msg.stream_count)
                    }
                    None => DoneOutcome::Finished,
                }
            };

            match outcome {
                DoneOutcome::Finished => {
                    self.remove_subscribe(msg.id);
                }
                DoneOutcome::AlreadyDraining => tracing::debug!(
                    target: "moq_transport::control",
                    request_id = msg.id,
                    "duplicate PUBLISH_DONE while draining — ignoring"
                ),
                DoneOutcome::DrainArmed => self.begin_drain(
                    msg.id,
                    Subscriber::expire_subscribe_drain,
                    "PUBLISH_DONE received with streams outstanding",
                ),
            }
            return Ok(());
        }

        // §10.11: the subscription is only finished once the Stream Count
        // announced by PUBLISH_DONE has been received. Keep the entry (and its
        // Track Alias) in place while streams are outstanding, otherwise
        // in-flight streams are routed nowhere and their Objects are dropped.
        let outcome = {
            let mut publishes = self
                .publishes_received
                .lock()
                .map_err(|_| SessionError::Internal)?;
            match publishes.get_mut(&msg.id) {
                Some(recv) => recv.recv_done(msg.status_code, msg.stream_count),
                None => {
                    tracing::debug!(
                        target: "moq_transport::control",
                        request_id = msg.id,
                        "received PUBLISH_DONE for unknown subscription — ignoring"
                    );
                    return Ok(());
                }
            }
        };

        match outcome {
            DoneOutcome::Finished => self.remove_publish_received_state(msg.id)?,
            // A second PUBLISH_DONE must not arm a second timer.
            DoneOutcome::AlreadyDraining => tracing::debug!(
                target: "moq_transport::control",
                request_id = msg.id,
                "duplicate PUBLISH_DONE while draining — ignoring"
            ),
            DoneOutcome::DrainArmed => self.begin_drain(
                msg.id,
                Subscriber::expire_publish_done_drain,
                "PUBLISH_DONE received with streams outstanding",
            ),
        }

        Ok(())
    }

    /// Subscriptions currently waiting on streams announced by PUBLISH_DONE.
    fn draining_count(&self) -> usize {
        let publishes = self
            .publishes_received
            .lock()
            .map(|map| map.values().filter(|recv| recv.is_draining()).count())
            .unwrap_or(0);
        let subscribes = self
            .subscribes
            .lock()
            .map(|map| map.values().filter(|recv| recv.is_draining()).count())
            .unwrap_or(0);
        publishes + subscribes
    }

    /// End a draining SUBSCRIBE-created subscription that is still waiting for
    /// streams announced by PUBLISH_DONE.
    fn expire_subscribe_drain(&self, request_id: u64) {
        let expired = match self.subscribes.lock() {
            Ok(mut subscribes) => subscribes
                .get_mut(&request_id)
                .is_some_and(|recv| recv.drain_timeout()),
            Err(_) => {
                tracing::error!(request_id, "subscribes lock poisoned; cannot expire drain");
                return;
            }
        };

        if !expired {
            return;
        }

        tracing::warn!(
            request_id,
            timeout_ms = PUBLISH_DONE_DRAIN_TIMEOUT.as_millis() as u64,
            "PUBLISH_DONE Stream Count was never reached; ending subscription on timer"
        );
        self.remove_subscribe(request_id);
    }

    /// Record a data stream arriving for a SUBSCRIBE-created subscription
    /// (§10.11 Stream Count). Must be paired with
    /// `note_subscribe_stream_finished`.
    #[must_use]
    fn note_subscribe_stream_received(&self, request_id: u64) -> bool {
        match self.subscribes.lock() {
            Ok(mut subscribes) => match subscribes.get_mut(&request_id) {
                Some(recv) => {
                    recv.note_stream_received();
                    true
                }
                None => false,
            },
            Err(_) => {
                tracing::error!(request_id, "subscribes lock poisoned");
                false
            }
        }
    }

    /// Record a data stream finishing, releasing the subscription if it was the
    /// last thing a deferred teardown waited on.
    fn note_subscribe_stream_finished(&self, request_id: u64) {
        let finished = match self.subscribes.lock() {
            Ok(mut subscribes) => subscribes
                .get_mut(&request_id)
                .is_some_and(|recv| recv.note_stream_finished()),
            Err(_) => {
                tracing::error!(request_id, "subscribes lock poisoned");
                return;
            }
        };

        if finished {
            tracing::debug!(
                request_id,
                "every stream announced by PUBLISH_DONE has arrived; ending subscription"
            );
            self.remove_subscribe(request_id);
        }
    }

    /// Bound how long a draining subscription can stay alive.
    ///
    /// §10.11 requires a timeout because the publisher may have over-counted,
    /// reset a stream before its SUBGROUP_HEADER, or declared that it could not
    /// count its streams at all.
    ///
    /// Request IDs are never reused within a session (see
    /// `RequestId::validate_incoming`), so this timer cannot end a later
    /// subscription that happens to share the ID.
    /// Start draining a subscription, or end it now if too many are already
    /// draining.
    ///
    /// Every path that defers teardown goes through here: a draining
    /// subscription outlives its request stream by design, so the cap is the
    /// only thing bounding how much state a peer can hold open by never sending
    /// the streams it announced — or by never sending PUBLISH_DONE at all.
    fn begin_drain(&self, request_id: u64, expire: fn(&Subscriber, u64), reason: &str) {
        if self.draining_count() > MAX_CONCURRENT_DRAINS {
            tracing::warn!(
                request_id,
                reason,
                "too many draining subscriptions; ending this one immediately"
            );
            expire(self, request_id);
            return;
        }

        tracing::debug!(
            target: "moq_transport::control",
            request_id,
            reason,
            "draining subscription"
        );
        self.arm_drain(request_id, expire);
    }

    fn arm_drain(&self, request_id: u64, expire: fn(&Subscriber, u64)) {
        let session = self.clone();
        let drain = async move {
            tokio::time::sleep(PUBLISH_DONE_DRAIN_TIMEOUT).await;
            expire(&session, request_id);
            Ok(())
        };

        // Run under `Session::run` like every other background task here, so it
        // is cancelled with the session instead of outliving it.
        if self.bidi_task_tx.send(Box::pin(drain)).is_err() {
            tracing::debug!(
                request_id,
                "session is shutting down; ending draining subscription now"
            );
            expire(self, request_id);
        }
    }

    /// Record a data stream arriving for an inbound PUBLISH (§10.11 Stream
    /// Count). Must be paired with `note_publish_stream_finished`.
    #[must_use]
    fn note_publish_stream_received(&self, request_id: u64) -> bool {
        match self.publishes_received.lock() {
            Ok(mut publishes) => match publishes.get_mut(&request_id) {
                Some(recv) => {
                    recv.note_stream_received();
                    true
                }
                None => false,
            },
            Err(_) => {
                tracing::error!(request_id, "publishes_received lock poisoned");
                false
            }
        }
    }

    /// Record a data stream for an inbound PUBLISH finishing, and release the
    /// subscription if it was the last thing a deferred teardown waited on.
    fn note_publish_stream_finished(&self, request_id: u64) {
        let finished = match self.publishes_received.lock() {
            Ok(mut publishes) => publishes
                .get_mut(&request_id)
                .is_some_and(|recv| recv.note_stream_finished()),
            Err(_) => {
                tracing::error!(request_id, "publishes_received lock poisoned");
                return;
            }
        };

        if finished {
            tracing::debug!(
                request_id,
                "every stream announced by PUBLISH_DONE has arrived; ending subscription"
            );
            if let Err(err) = self.remove_publish_received_state(request_id) {
                tracing::error!(request_id, error = %err, "failed to remove drained PUBLISH state");
            }
        }
    }

    /// End a subscription that is still waiting for streams announced by
    /// PUBLISH_DONE. No-op if the streams already arrived and the state was
    /// released.
    fn expire_publish_done_drain(&self, request_id: u64) {
        let expired = match self.publishes_received.lock() {
            Ok(mut publishes) => publishes
                .get_mut(&request_id)
                .is_some_and(|recv| recv.drain_timeout()),
            Err(_) => {
                tracing::error!(
                    request_id,
                    "inbound PUBLISH map lock poisoned; cannot expire drain"
                );
                return;
            }
        };

        if !expired {
            return;
        }

        tracing::warn!(
            request_id,
            timeout_ms = PUBLISH_DONE_DRAIN_TIMEOUT.as_millis() as u64,
            "PUBLISH_DONE Stream Count was never reached; ending subscription on timer"
        );
        if let Err(err) = self.remove_publish_received_state(request_id) {
            tracing::error!(request_id, error = %err, "failed to remove drained PUBLISH state");
        }
    }

    /// Handle REQUEST_OK from the publisher.
    ///
    /// REQUEST_OK is the shared positive response for REQUEST_UPDATE, TRACK_STATUS,
    /// SUBSCRIBE_NAMESPACE, and PUBLISH_NAMESPACE.  SUBSCRIBE uses its own dedicated
    /// SUBSCRIBE_OK message (§9.10) and does not come through this handler.
    /// Full routing for the other request types is wired up (TODO itzmanish).
    fn recv_request_ok(&mut self, msg: &message::RequestOk) -> Result<(), SessionError> {
        self.log_request_ok_parsed("unknown", msg);
        tracing::debug!(
            target: "moq_transport::control",
            request_id = msg.id,
            "received REQUEST_OK"
        );
        // TODO(itzmanish): route to the correct pending request type by ID.
        Ok(())
    }

    /// Handle REQUEST_ERROR from the publisher.
    ///
    /// Routes to the matching active subscribe (via request ID) if one
    /// exists, otherwise logs and ignores.  Full per-flow routing is
    /// wired up (TODO itzmanish).
    fn recv_request_error(&mut self, msg: &message::RequestError) -> Result<(), SessionError> {
        // Route to a matching subscribe if present.
        if let Some(subscribe) = self.remove_subscribe(msg.id) {
            self.log_request_error_parsed("subscribe", msg);
            let err = Self::request_error_to_serve_error(msg);
            subscribe.error(err)?;
        } else {
            self.log_request_error_parsed("unknown", msg);
        }

        tracing::debug!(
            target: "moq_transport::control",
            request_id = msg.id,
            error_code = msg.error_code,
            retry_interval = msg.retry_interval,
            reason = %msg.reason.0,
            "received REQUEST_ERROR"
        );
        Ok(())
    }

    /// Handle an inbound PUBLISH (draft-16 §9.13).
    ///
    /// This establishes a publisher-initiated subscription: the peer offers a
    /// track and this endpoint becomes its subscriber.
    fn recv_publish(&mut self, msg: &message::Publish) -> Result<(), SessionError> {
        // First-cut policy: reject non-empty TrackExtensions. They are not
        // carried through TrackReader/TrackWriter yet, so accepting them would
        // silently drop relay-visible metadata (§8.6).
        if !msg.track_extensions.is_empty() {
            self.send_request_error(
                "publish",
                message::RequestError {
                    id: msg.id,
                    error_code: message::RequestErrorCode::NotSupported as u64,
                    retry_interval: 0,
                    reason: crate::coding::ReasonPhrase(
                        "track extensions not supported".to_string(),
                    ),
                },
            );
            return Ok(());
        }

        let full_name = FullTrackName {
            namespace: msg.track_namespace.clone(),
            name: msg.track_name.clone(),
        };

        // Parse FORWARD and LARGEST_OBJECT before reserving any session state,
        // so malformed parameters cannot leave stale alias or name entries.
        let initial_forward = msg
            .params
            .forward()
            .map_err(SessionError::Decode)?
            .unwrap_or(true);
        let largest_location = msg.params.largest_object().map_err(SessionError::Decode)?;

        // Reserve the track name first. The duplicate-subscription check runs
        // before the alias check on purpose: a duplicate PUBLISH for the same
        // track is a request error, not a session-closing alias collision.
        {
            let mut names = self
                .subscriber_names
                .lock()
                .map_err(|_| SessionError::Internal)?;
            if names.contains_name(&full_name) {
                drop(names);
                self.send_request_error(
                    "publish",
                    message::RequestError {
                        id: msg.id,
                        error_code: message::RequestErrorCode::DuplicateSubscription as u64,
                        retry_interval: 0,
                        reason: crate::coding::ReasonPhrase("duplicate subscription".to_string()),
                    },
                );
                return Ok(());
            }

            names.insert(full_name, msg.id);
        }

        // Reserve the alias without holding subscriber_names, so cleanup and
        // inbound PUBLISH handling have no lock-order dependency.
        let alias_result = match self.track_alias_map.lock() {
            Ok(mut aliases) => {
                if aliases.contains_alias(msg.track_alias) {
                    // §9.13: a duplicate Track Alias for a different track
                    // closes the session.
                    Err(SessionError::Duplicate)
                } else {
                    aliases.insert(msg.track_alias, TrackOrigin::Publish(msg.id))
                }
            }
            Err(_) => Err(SessionError::Internal),
        };
        if let Err(err) = alias_result {
            if let Ok(mut names) = self.subscriber_names.lock() {
                names.remove_by_request_id(msg.id);
            }
            return Err(err);
        }

        // Allocate the track. The transport owns the writer; the application
        // gets the reader from PublishReceived::ok.
        let (writer, reader) =
            serve::Track::new(msg.track_namespace.clone(), msg.track_name.clone()).produce();

        let (publish_received, recv) = PublishReceivedRecv::produce(
            self.clone(),
            msg.id,
            msg.track_alias,
            msg.track_namespace.clone(),
            msg.track_name.clone(),
            initial_forward,
            largest_location,
            writer,
            reader,
        );

        // The alias is live before the handle is queued, so Object streams that
        // race the PUBLISH (§5.1 permits delivery before PUBLISH_OK) resolve.
        self.track_alias_notify.notify_waiters();

        match self.publishes_received.lock() {
            Ok(mut publishes_received) => {
                publishes_received.insert(msg.id, recv);
            }
            Err(_) => {
                self.clear_subscription_reservations(msg.id)?;
                return Err(SessionError::Internal);
            }
        }

        tracing::debug!(
            target: "moq_transport::control",
            request_id = msg.id,
            track_alias = msg.track_alias,
            namespace = %msg.track_namespace,
            name = %msg.track_name,
            forward = initial_forward,
            "received PUBLISH"
        );

        // If the application is no longer listening, dropping the handle sends
        // REQUEST_ERROR back to the publisher.
        if self.publish_received_queue.push(publish_received).is_err() {
            self.remove_publish_received(msg.id);
        }

        Ok(())
    }

    /// Wait for the next inbound PUBLISH from the peer, if any.
    ///
    /// The returned [`PublishReceived`] must be accepted with
    /// [`PublishReceived::ok`] or dropped to reject.
    pub async fn publish_received(&mut self) -> Option<PublishReceived> {
        self.publish_received_queue.pop().await
    }

    /// Abandon an inbound PUBLISH whose request stream closed without
    /// PUBLISH_DONE.
    ///
    /// Without this the track writer, the Track Alias, and the name
    /// reservation would all stay held for the life of the session, and the
    /// application's `PublishReceived::closed()` would never resolve.
    pub(super) fn abort_publish_received(&self, request_id: u64) {
        // PUBLISH_DONE is terminal on the request stream, so this runs
        // immediately after a normal end-of-subscription too. Data streams are
        // independent of the request stream, so a subscription that is still
        // draining — against an announced Stream Count or against a stream it
        // is mid-way through reading — is left for the drain (or its timeout)
        // to finish. Tearing down here would discard Objects already in flight
        // and lose the publisher's status code.
        let outcome = match self.publishes_received.lock() {
            Ok(mut map) => match map.get_mut(&request_id) {
                Some(recv) => recv.abort(message::PublishDoneCode::InternalError as u64),
                None => return,
            },
            Err(_) => {
                tracing::error!(request_id, "publishes_received lock poisoned");
                return;
            }
        };

        match outcome {
            DoneOutcome::AlreadyDraining => tracing::debug!(
                request_id,
                "PUBLISH request stream closed while draining; keeping state for in-flight streams"
            ),
            DoneOutcome::DrainArmed => self.begin_drain(
                request_id,
                Subscriber::expire_publish_done_drain,
                "PUBLISH request stream closed with a stream still being read",
            ),
            DoneOutcome::Finished => {
                tracing::debug!(
                    request_id,
                    "PUBLISH request stream closed without PUBLISH_DONE"
                );
                if let Err(err) = self.remove_publish_received_state(request_id) {
                    tracing::error!(request_id, error = %err, "failed to release inbound PUBLISH reservations");
                }
            }
        }
    }

    /// Remove all subscriber-side state for an inbound PUBLISH.
    ///
    /// Called by `PublishReceived::drop` when the app did not call `ok()`.
    pub(super) fn remove_publish_received(&self, request_id: u64) {
        if let Err(err) = self.remove_publish_received_state(request_id) {
            tracing::error!(request_id, error = %err, "failed to remove inbound PUBLISH state");
        }
    }

    fn remove_publish_received_state(&self, request_id: u64) -> Result<(), SessionError> {
        self.publishes_received
            .lock()
            .map_err(|_| SessionError::Internal)?
            .remove(&request_id);
        self.clear_subscription_reservations(request_id)
    }

    /// Release the alias and track-name reservations held by one subscription.
    fn clear_subscription_reservations(&self, request_id: u64) -> Result<(), SessionError> {
        self.track_alias_map
            .lock()
            .map_err(|_| SessionError::Internal)?
            .remove_by_request_id(request_id);
        self.subscriber_names
            .lock()
            .map_err(|_| SessionError::Internal)?
            .remove_by_request_id(request_id);
        Ok(())
    }

    /// Map a REQUEST_ERROR to a semantic ServeError so callers see
    /// meaningful variants (e.g. NotFound) instead of opaque error codes.
    fn request_error_to_serve_error(msg: &message::RequestError) -> ServeError {
        use message::RequestErrorCode;
        match msg.error_code {
            c if c == RequestErrorCode::DoesNotExist as u64 => {
                ServeError::not_found_ctx(msg.reason.0.clone())
            }
            c if c == RequestErrorCode::InternalError as u64 => {
                ServeError::internal_ctx(msg.reason.0.clone())
            }
            // Preserve TIMEOUT as a semantic variant rather than an opaque code.
            c if c == RequestErrorCode::Timeout as u64 => ServeError::Timeout,
            c if c == RequestErrorCode::DuplicateSubscription as u64 => ServeError::Duplicate,
            c if c == RequestErrorCode::NotSupported as u64 => {
                ServeError::NotImplemented(msg.reason.0.clone())
            }
            code => ServeError::Closed(code),
        }
    }

    fn drop_publish_namespace(&mut self, id: u64) -> Option<PublishedNamespaceRecv> {
        if let Ok(mut ns) = self.published_namespaces.lock() {
            let key = ns
                .iter()
                .find(|(_k, v)| v.request_id == id)
                .map(|(k, _)| k.clone());
            if let Some(key) = key {
                return ns.remove(&key);
            }
        }
        None
    }

    /// Resolve a Track Alias to its owning subscription, waiting up to the
    /// given timeout for it to appear. With `None`, check once and return.
    async fn get_track_origin_by_alias(
        &self,
        track_alias: u64,
        timeout_ms: Option<u64>,
    ) -> Result<Option<TrackOrigin>, SessionError> {
        // If no timeout specified, don't wait
        let timeout_ms = match timeout_ms {
            Some(ms) => ms,
            None => {
                // Just check once
                return match self.track_alias_map.lock() {
                    Ok(aliases) => Ok(aliases.get(track_alias)),
                    Err(_) => {
                        tracing::error!(
                            target: "moq_transport::control",
                            track_alias,
                            "track alias map lock poisoned"
                        );
                        Err(SessionError::Internal)
                    }
                };
            }
        };

        // Wait for it to appear, checking after each notification
        let timeout_duration = Duration::from_millis(timeout_ms);
        tokio::time::timeout(timeout_duration, async {
            loop {
                // Register for notification before checking, to close the
                // window where the alias lands between check and wait.
                let notified = self.track_alias_notify.notified();

                let origin = match self.track_alias_map.lock() {
                    Ok(aliases) => aliases.get(track_alias),
                    Err(_) => {
                        tracing::error!(
                            target: "moq_transport::control",
                            track_alias,
                            "track alias map lock poisoned"
                        );
                        return Err(SessionError::Internal);
                    }
                };

                if let Some(origin) = origin {
                    return Ok(Some(origin));
                }

                // Alias not present yet, wait for notification
                notified.await;
            }
        })
        .await
        .unwrap_or(Ok(None))
    }

    /// Open the subgroup writer for whichever subscription owns this alias.
    fn open_subgroup_writer(
        &self,
        origin: TrackOrigin,
        header: &data::SubgroupHeader,
    ) -> Result<serve::SubgroupWriter, SessionError> {
        match origin {
            TrackOrigin::Subscribe(subscribe_id) => {
                let mut map = self.subscribes.lock().map_err(|_| SessionError::Internal)?;
                let recv = map.get_mut(&subscribe_id).ok_or_else(|| {
                    ServeError::not_found_ctx(format!(
                        "subscribe_id={} not found for track_alias={}",
                        subscribe_id, header.track_alias
                    ))
                })?;
                Ok(recv.subgroup(header.clone())?)
            }
            TrackOrigin::Publish(publish_id) => {
                let mut map = self
                    .publishes_received
                    .lock()
                    .map_err(|_| SessionError::Internal)?;
                let recv = map.get_mut(&publish_id).ok_or_else(|| {
                    ServeError::not_found_ctx(format!(
                        "publish_id={} not found for track_alias={}",
                        publish_id, header.track_alias
                    ))
                })?;
                Ok(recv.subgroup(header.clone())?)
            }
        }
    }

    /// Handle reception of a new stream from the QUIC session.
    pub(super) async fn recv_stream(
        mut self,
        stream: web_transport::RecvStream,
    ) -> Result<(), SessionError> {
        tracing::trace!("[SUBSCRIBER] recv_stream: new stream received, decoding header");
        let mut reader = Reader::new(stream);

        // Decode the stream header
        let stream_header: data::StreamHeader = reader.decode().await?;
        tracing::trace!(
            "[SUBSCRIBER] recv_stream: decoded stream header type={:?}",
            stream_header.header_type
        );

        // No fetch support yet
        if !stream_header.header_type.is_subgroup() {
            return Err(SessionError::unimplemented("non-SUBGROUP stream types"));
        }

        // Log subgroup header parsed/received
        if let Some(ref subgroup_header) = stream_header.subgroup_header {
            if let Some(ref mlog) = self.mlog {
                if let Ok(mut mlog_guard) = mlog.lock() {
                    let time = mlog_guard.elapsed_ms();
                    let stream_id = 0; // TODO: Placeholder, need actual QUIC stream ID
                    let event = mlog::subgroup_header_parsed(time, stream_id, subgroup_header);
                    let _ = mlog_guard.add_event(event);
                }
            }
        }

        // Peer-parsed: `is_subgroup()` gates this today, but never panic on
        // decoded input (RFC-022).
        let track_alias = stream_header
            .subgroup_header
            .as_ref()
            .ok_or(SessionError::Internal)?
            .track_alias;
        tracing::trace!(
            "[SUBSCRIBER] recv_stream: stream for subscription track_alias={}",
            track_alias
        );

        let mlog = self.mlog.clone();
        let res = self.recv_stream_inner(reader, stream_header, mlog).await;
        if let Err(SessionError::Serve(err)) = &res {
            tracing::warn!(
                "[SUBSCRIBER] recv_stream: stream processing error for track_alias={}: {:?}",
                track_alias,
                err
            );
            // The writer is closed, so we should terminate.
            // TODO it would be nice to do this immediately when the Writer is closed.
            if let Some(TrackOrigin::Subscribe(subscribe_id)) =
                self.get_track_origin_by_alias(track_alias, None).await?
            {
                if let Some(subscribe) = self.remove_subscribe(subscribe_id) {
                    subscribe.error(err.clone())?;
                }
            }
        }

        res
    }

    /// Continue handling the reception of a new stream from the QUIC session.
    async fn recv_stream_inner(
        &mut self,
        reader: Reader,
        stream_header: data::StreamHeader,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        // Peer-parsed: `is_subgroup()` gates this today, but never panic on
        // decoded input (RFC-022).
        let track_alias = stream_header
            .subgroup_header
            .as_ref()
            .ok_or(SessionError::Internal)?
            .track_alias;
        tracing::trace!(
            "[SUBSCRIBER] recv_stream_inner: processing stream for track_alias={}",
            track_alias
        );

        let Some(origin) = self
            .get_track_origin_by_alias(track_alias, Some(DEFAULT_ALIAS_WAIT_TIME_MS))
            .await?
        else {
            return Err(SessionError::Serve(ServeError::not_found_ctx(format!(
                "subscription track_alias={} not found",
                track_alias
            ))));
        };

        tracing::trace!("[SUBSCRIBER] recv_stream_inner: receiving subgroup data");
        let subgroup_header = stream_header
            .subgroup_header
            .ok_or(SessionError::Internal)?;

        // §10.11 counts every data stream the publisher opened, "including
        // streams that contained no Objects (e.g., an empty Subgroup)", so the
        // stream is counted here rather than when a first Object opens a
        // subgroup writer. The subscription is then held open until the stream
        // has been read, which is what lets a PUBLISH_DONE that arrives
        // mid-stream still deliver the Objects already in flight.
        // Only balance the decrement below when the increment actually landed;
        // a stream can resolve its alias before the subscription is in the map.
        let counted = match origin {
            TrackOrigin::Publish(publish_id) => self.note_publish_stream_received(publish_id),
            TrackOrigin::Subscribe(subscribe_id) => {
                self.note_subscribe_stream_received(subscribe_id)
            }
        };

        let res = self
            .recv_subgroup(
                stream_header.header_type,
                subgroup_header,
                origin,
                reader,
                mlog,
            )
            .await;

        // Balances the `note_*_stream_received` above on every path, so a
        // failed stream cannot leave the subscription waiting on it forever.
        if counted {
            match origin {
                TrackOrigin::Publish(publish_id) => self.note_publish_stream_finished(publish_id),
                TrackOrigin::Subscribe(subscribe_id) => {
                    self.note_subscribe_stream_finished(subscribe_id)
                }
            }
        }
        res?;

        tracing::trace!(
            "[SUBSCRIBER] recv_stream_inner: completed processing stream for track_alias={}",
            track_alias
        );
        Ok(())
    }

    /// If new stream is a Subgroup stream, handle reception of subgroup objects and payloads.
    async fn recv_subgroup(
        &mut self,
        stream_header_type: data::StreamHeaderType,
        mut subgroup_header: data::SubgroupHeader,
        origin: TrackOrigin,
        mut reader: Reader,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        tracing::trace!(
            "[SUBSCRIBER] recv_subgroup: starting - group_id={}, subgroup_id={:?}, priority={}",
            subgroup_header.group_id,
            subgroup_header.subgroup_id,
            subgroup_header.publisher_priority
        );

        let mut object_count = 0;
        let mut previous_object_id: Option<u64> = None;
        let mut subgroup_writer: Option<serve::SubgroupWriter> = None;
        while !reader.done().await? {
            tracing::trace!(
                "[SUBSCRIBER] recv_subgroup: reading object #{} (has_ext_headers={})",
                object_count + 1,
                stream_header_type.has_extension_headers()
            );

            // Need to be able to decode the subgroup object conditionally based on the stream header type
            // read the object payload length into remaining_bytes
            let (mut remaining_bytes, object_id_delta, status, decoded_object) =
                match stream_header_type.has_extension_headers() {
                    true => {
                        let object = reader.decode::<data::SubgroupObjectExt>().await?;
                        tracing::trace!(
                        "[SUBSCRIBER] recv_subgroup: object #{} with extension headers - object_id_delta={}, payload_length={}, status={:?}, extension_headers={:?}",
                        object_count + 1,
                        object.object_id_delta,
                        object.payload_length,
                        object.status,
                        object.extension_headers
                    );

                        // Check for known draft-14 extension types

                        // Check for Immutable Extensions (type 0xB = 11)
                        if object.extension_headers.has(0xB) {
                            tracing::trace!(
                                "[SUBSCRIBER] recv_subgroup: object #{} contains IMMUTABLE EXTENSIONS (type 0xB) - will be forwarded",
                                object_count + 1
                            );
                            if let Some(immutable_ext) = object.extension_headers.get(0xB) {
                                tracing::trace!(
                                    "[SUBSCRIBER] recv_subgroup: immutable extension details: {:?}",
                                    immutable_ext
                                );
                            }
                        }

                        // Check for Prior Group ID Gap (type 0x3C = 60)
                        if object.extension_headers.has(0x3C) {
                            tracing::trace!(
                                "[SUBSCRIBER] recv_subgroup: object #{} contains PRIOR GROUP ID GAP (type 0x3C)",
                                object_count + 1
                            );
                            if let Some(gap_ext) = object.extension_headers.get(0x3C) {
                                tracing::trace!(
                                    "[SUBSCRIBER] recv_subgroup: prior group id gap details: {:?}",
                                    gap_ext
                                );
                            }
                        }

                        let obj_copy = object.clone();
                        (
                            object.payload_length,
                            object.object_id_delta,
                            object.status,
                            Some(obj_copy),
                        )
                    }
                    false => {
                        let object = reader.decode::<data::SubgroupObject>().await?;
                        tracing::trace!(
                        "[SUBSCRIBER] recv_subgroup: object #{} - object_id_delta={}, payload_length={}, status={:?}",
                        object_count + 1,
                        object.object_id_delta,
                        object.payload_length,
                        object.status
                    );
                        (
                            object.payload_length,
                            object.object_id_delta,
                            object.status,
                            None,
                        )
                    }
                };

            let current_object_id = match previous_object_id {
                Some(previous) => previous
                    .checked_add(object_id_delta)
                    .and_then(|value| value.checked_add(1))
                    .ok_or_else(|| {
                        SessionError::ProtocolViolation("subgroup object id overflow".to_string())
                    })?,
                None => object_id_delta,
            };
            previous_object_id = Some(current_object_id);

            // Extract extension headers if present
            let extension_headers = decoded_object
                .as_ref()
                .map(|obj| obj.extension_headers.clone());

            if status.is_some_and(|status| status != data::ObjectStatus::NormalObject)
                && extension_headers
                    .as_ref()
                    .is_some_and(|headers| !headers.is_empty())
            {
                return Err(SessionError::ProtocolViolation(
                    "non-normal object status with extension headers".to_string(),
                ));
            }

            if subgroup_writer.is_none() {
                if stream_header_type.uses_first_object_id_as_subgroup_id() {
                    subgroup_header.subgroup_id = Some(current_object_id);
                }

                subgroup_writer = Some(self.open_subgroup_writer(origin, &subgroup_header)?);
            }

            // Log subgroup object parsed/received
            if let Some(ref mlog) = mlog {
                if let Ok(mut mlog_guard) = mlog.lock() {
                    let time = mlog_guard.elapsed_ms();
                    let stream_id = 0; // TODO: Placeholder, need actual QUIC stream ID
                    let event = if let Some(obj_ext) = decoded_object {
                        mlog::subgroup_object_ext_parsed(
                            time,
                            stream_id,
                            subgroup_header.group_id,
                            subgroup_header.subgroup_id.unwrap_or(0),
                            current_object_id,
                            &obj_ext,
                        )
                    } else {
                        // For non-extension objects, create a temporary SubgroupObject for logging
                        let temp_obj = data::SubgroupObject {
                            object_id_delta,
                            payload_length: remaining_bytes,
                            status,
                        };
                        mlog::subgroup_object_parsed(
                            time,
                            stream_id,
                            subgroup_header.group_id,
                            subgroup_header.subgroup_id.unwrap_or(0),
                            current_object_id,
                            &temp_obj,
                        )
                    };
                    let _ = mlog_guard.add_event(event);
                }
            }

            // Pass extension headers through to the serve layer
            // TODO SLG - object_id_delta and object status are still being ignored

            let subgroup_writer = subgroup_writer.as_mut().ok_or(SessionError::Internal)?;
            let mut object_writer = subgroup_writer.create(remaining_bytes, extension_headers)?;
            tracing::trace!(
                "[SUBSCRIBER] recv_subgroup: reading payload for object #{} ({} bytes)",
                object_count + 1,
                remaining_bytes
            );

            let mut chunks_read = 0;
            while remaining_bytes > 0 {
                let data = reader
                    .read_chunk(remaining_bytes)
                    .await?
                    .ok_or_else(|| {
                        tracing::error!(
                            "[SUBSCRIBER] recv_subgroup: ERROR - stream ended with {} bytes remaining for object #{}",
                            remaining_bytes,
                            object_count + 1
                        );
                        SessionError::WrongSize
                    })?;
                tracing::trace!(
                    "[SUBSCRIBER] recv_subgroup: received payload chunk #{} for object #{} ({} bytes, {} remaining)",
                    chunks_read + 1,
                    object_count + 1,
                    data.len(),
                    remaining_bytes - data.len()
                );
                remaining_bytes -= data.len();
                object_writer.write(data)?;
                chunks_read += 1;
            }

            tracing::trace!(
                "[SUBSCRIBER] recv_subgroup: completed object #{} ({} chunks)",
                object_count + 1,
                chunks_read
            );
            object_count += 1;
        }

        tracing::trace!(
            "[SUBSCRIBER] recv_subgroup: completed subgroup (group_id={}, subgroup_id={}, {} objects received)",
            subgroup_header.group_id,
            subgroup_header.subgroup_id.unwrap_or(0),
            object_count
        );

        Ok(())
    }

    /// Handle reception of a datagram from the QUIC session.
    pub async fn recv_datagram(&mut self, datagram: bytes::Bytes) -> Result<(), SessionError> {
        let mut cursor = io::Cursor::new(datagram);
        let datagram = data::Datagram::decode(&mut cursor)?;

        if let Some(ref mlog) = self.mlog {
            if let Ok(mut mlog_guard) = mlog.lock() {
                let time = mlog_guard.elapsed_ms();
                let stream_id = 0; // TODO: Placeholder, need actual QUIC stream ID
                let _ =
                    mlog_guard.add_event(mlog::object_datagram_parsed(time, stream_id, &datagram));
            }
        }

        // Check for extension headers in the datagram
        if let Some(ref ext_headers) = datagram.extension_headers {
            tracing::trace!(
                "[SUBSCRIBER] recv_datagram: datagram contains extension headers: {:?}",
                ext_headers
            );

            // Check for known draft-14 extension types

            // Check for Immutable Extensions (type 0xB = 11)
            if ext_headers.has(0xB) {
                tracing::trace!(
                    "[SUBSCRIBER] recv_datagram: datagram contains IMMUTABLE EXTENSIONS (type 0xB)"
                );
                if let Some(immutable_ext) = ext_headers.get(0xB) {
                    tracing::trace!(
                        "[SUBSCRIBER] recv_datagram: immutable extension details: {:?}",
                        immutable_ext
                    );
                }
            }

            // Check for Prior Group ID Gap (type 0x3C = 60)
            if ext_headers.has(0x3C) {
                tracing::trace!(
                    "[SUBSCRIBER] recv_datagram: datagram contains PRIOR GROUP ID GAP (type 0x3C)"
                );
                if let Some(gap_ext) = ext_headers.get(0x3C) {
                    tracing::trace!(
                        "[SUBSCRIBER] recv_datagram: prior group id gap details: {:?}",
                        gap_ext
                    );
                }
            }
        }

        // Route to whichever subscription owns this track alias.
        let origin = self
            .get_track_origin_by_alias(datagram.track_alias, Some(DEFAULT_ALIAS_WAIT_TIME_MS))
            .await?;

        match origin {
            Some(TrackOrigin::Subscribe(subscribe_id)) => {
                if let Some(subscribe) = self
                    .subscribes
                    .lock()
                    .ok()
                    .as_mut()
                    .and_then(|s| s.get_mut(&subscribe_id))
                {
                    tracing::trace!(
                        "[SUBSCRIBER] recv_datagram (SUBSCRIBE): track_alias={}, group_id={}, object_id={}, publisher_priority={}, status={}, payload_length={}",
                        datagram.track_alias,
                        datagram.group_id,
                        datagram.object_id.unwrap_or(0),
                        datagram.publisher_priority,
                        datagram.status.as_ref().map_or("None".to_string(), |s| format!("{:?}", s)),
                        datagram.payload.as_ref().map_or(0, |p| p.len()));
                    subscribe.datagram(datagram)?;
                }
            }
            Some(TrackOrigin::Publish(publish_id)) => {
                if let Some(recv) = self
                    .publishes_received
                    .lock()
                    .ok()
                    .as_mut()
                    .and_then(|m| m.get_mut(&publish_id))
                {
                    tracing::trace!(
                        "[SUBSCRIBER] recv_datagram (PUBLISH): track_alias={}, group_id={}, object_id={}, publisher_priority={}, status={}, payload_length={}",
                        datagram.track_alias,
                        datagram.group_id,
                        datagram.object_id.unwrap_or(0),
                        datagram.publisher_priority,
                        datagram.status.as_ref().map_or("None".to_string(), |s| format!("{:?}", s)),
                        datagram.payload.as_ref().map_or(0, |p| p.len()));
                    recv.datagram(datagram)?;
                }
            }
            None => {
                tracing::warn!(
                    "[SUBSCRIBER] recv_datagram: discarded due to unknown track_alias: track_alias={}, group_id={}, object_id={}, publisher_priority={}, status={}, payload_length={}",
                    datagram.track_alias,
                    datagram.group_id,
                    datagram.object_id.unwrap_or(0),
                    datagram.publisher_priority,
                    datagram.status.as_ref().map_or("None".to_string(), |s| format!("{:?}", s)),
                    datagram.payload.as_ref().map_or(0, |p| p.len()));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::loopback_session;

    fn test_subscriber(session: web_transport::Session) -> Subscriber {
        let outgoing = Queue::default().split().0;
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        Subscriber::new(outgoing, session, None, RequestId::new(0, 1), bidi_task_tx)
    }

    /// Like `test_subscriber`, but keeps the background-task receiver alive.
    ///
    /// `test_subscriber` drops it, which makes the session look like it is
    /// shutting down; anything that defers work to a background task then runs
    /// inline instead.
    type BidiTaskRx = tokio::sync::mpsc::UnboundedReceiver<
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SessionError>> + Send>>,
    >;

    fn test_subscriber_with_tasks(session: web_transport::Session) -> (Subscriber, BidiTaskRx) {
        let outgoing = Queue::default().split().0;
        let (bidi_task_tx, bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Subscriber::new(outgoing, session, None, RequestId::new(0, 1), bidi_task_tx),
            bidi_task_rx,
        )
    }

    fn test_track(name: &str) -> serve::TrackWriter {
        let (writer, _reader) =
            serve::Track::new(TrackNamespace::from_utf8_path("test/ns"), name).produce();
        writer
    }

    #[test]
    fn request_frame_rejects_invalid_params_before_stream_open() {
        let mut params = KeyValuePairs::default();
        params.set_bytesvalue(0x100, vec![1]);
        let msg = Message::Subscribe(message::Subscribe {
            id: 0,
            track_namespace: TrackNamespace::from_utf8_path("test/ns"),
            track_name: "video".into(),
            params,
        });

        assert!(matches!(
            super::super::encode_request_frame(&msg),
            Err(SessionError::Encode(
                crate::coding::EncodeError::InvalidValue
            ))
        ));
    }

    /// `Subscribe::drop` must remove the request's entry from the subscribes map
    /// (the real Drop impl calls `Subscriber::remove_subscribe`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_subscribe_removes_recv_state() {
        let subscriber = test_subscriber(loopback_session().await);
        let request_id = 4;

        let (subscribe, recv, _msg) = Subscribe::new(
            subscriber.clone(),
            request_id,
            test_track("video"),
            Default::default(),
        )
        .unwrap();
        // Mimic subscribe_open: register the recv state under the request id.
        subscriber
            .subscribes
            .lock()
            .unwrap()
            .insert(request_id, recv);
        assert!(
            subscriber
                .subscribes
                .lock()
                .unwrap()
                .contains_key(&request_id),
            "precondition: subscribe should be registered"
        );

        drop(subscribe);

        assert!(
            !subscriber
                .subscribes
                .lock()
                .unwrap()
                .contains_key(&request_id),
            "Subscribe::drop should remove the subscribes-map entry"
        );
    }

    /// `remove_subscribe` must clear the subscribes map and release the alias.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_subscribe_clears_alias_map() {
        let subscriber = test_subscriber(loopback_session().await);
        let request_id = 6;
        let track_alias = 42;

        let (subscribe, mut recv, _msg) = Subscribe::new(
            subscriber.clone(),
            request_id,
            test_track("audio"),
            Default::default(),
        )
        .unwrap();
        // Record the track alias while the Subscribe (the state's send half) is
        // still alive, so the recv state accepts the mutation. Then drop the
        // handle — its Drop runs against the still-empty map (a no-op) — and
        // drive remove_subscribe directly via the registered recv state.
        recv.ok(track_alias).unwrap();
        drop(subscribe);
        subscriber
            .subscribes
            .lock()
            .unwrap()
            .insert(request_id, recv);
        subscriber
            .track_alias_map
            .lock()
            .unwrap()
            .insert(track_alias, TrackOrigin::Subscribe(request_id))
            .unwrap();

        let removed = subscriber.remove_subscribe(request_id);

        assert!(
            removed.is_some(),
            "remove_subscribe should return the removed recv state"
        );
        assert!(
            !subscriber
                .subscribes
                .lock()
                .unwrap()
                .contains_key(&request_id),
            "remove_subscribe should clear the subscribes map"
        );
        assert!(
            subscriber.track_alias_map.lock().unwrap().is_empty(),
            "remove_subscribe should release the track alias"
        );
    }

    /// An inbound PUBLISH must claim its Track Alias so racing Object streams
    /// route to the PUBLISH receiver, not to a SUBSCRIBE.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recv_publish_registers_alias_and_queues_handle() {
        let mut subscriber = test_subscriber(loopback_session().await);

        subscriber
            .recv_publish(&message::Publish {
                id: 1,
                track_namespace: TrackNamespace::from_utf8_path("test/ns"),
                track_name: "video".into(),
                track_alias: 7,
                params: Default::default(),
                track_extensions: Default::default(),
            })
            .unwrap();

        assert_eq!(
            subscriber.track_alias_map.lock().unwrap().get(7),
            Some(TrackOrigin::Publish(1))
        );
        assert!(subscriber
            .publishes_received
            .lock()
            .unwrap()
            .contains_key(&1));

        let publish = subscriber.publish_received().await.expect("queued PUBLISH");
        assert_eq!(publish.track_alias(), 7);
        assert_eq!(publish.name(), &TrackName::from("video"));
    }

    /// A second PUBLISH for a track we already subscribe to is rejected as a
    /// request error, leaving the first subscription intact (§5.1).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_publish_for_same_track_is_rejected() {
        let mut subscriber = test_subscriber(loopback_session().await);
        let publish = |id, alias| message::Publish {
            id,
            track_namespace: TrackNamespace::from_utf8_path("test/ns"),
            track_name: "video".into(),
            track_alias: alias,
            params: Default::default(),
            track_extensions: Default::default(),
        };

        subscriber.recv_publish(&publish(1, 7)).unwrap();
        subscriber.recv_publish(&publish(3, 9)).unwrap();

        assert!(
            !subscriber
                .publishes_received
                .lock()
                .unwrap()
                .contains_key(&3),
            "duplicate PUBLISH must not create a second subscription"
        );
        assert_eq!(subscriber.track_alias_map.lock().unwrap().get(9), None);
    }

    /// A PUBLISH request stream that dies without PUBLISH_DONE must still
    /// release its state, or the track writer, alias, and name reservation
    /// leak for the life of the session and `closed()` never resolves.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborting_inbound_publish_releases_state_and_unblocks_closed() {
        let mut subscriber = test_subscriber(loopback_session().await);
        subscriber
            .recv_publish(&message::Publish {
                id: 1,
                track_namespace: TrackNamespace::from_utf8_path("test/ns"),
                track_name: "video".into(),
                track_alias: 7,
                params: Default::default(),
                track_extensions: Default::default(),
            })
            .unwrap();
        let publish = subscriber.publish_received().await.expect("queued PUBLISH");

        subscriber.abort_publish_received(1);

        assert!(subscriber.publishes_received.lock().unwrap().is_empty());
        assert!(subscriber.track_alias_map.lock().unwrap().is_empty());
        assert!(
            publish.closed().await.is_err(),
            "an aborted PUBLISH must surface as a failure, not a clean end"
        );
    }

    /// An outbound SUBSCRIBE claims the track name, so a PUBLISH offering the
    /// same track is rejected rather than creating a second subscriber-role
    /// subscription for it (§5.1).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publish_for_already_subscribed_track_is_rejected() {
        let mut subscriber = test_subscriber(loopback_session().await);
        subscriber.subscriber_names.lock().unwrap().insert(
            FullTrackName {
                namespace: TrackNamespace::from_utf8_path("test/ns"),
                name: "video".into(),
            },
            0,
        );

        subscriber
            .recv_publish(&message::Publish {
                id: 1,
                track_namespace: TrackNamespace::from_utf8_path("test/ns"),
                track_name: "video".into(),
                track_alias: 7,
                params: Default::default(),
                track_extensions: Default::default(),
            })
            .unwrap();

        assert!(subscriber.publishes_received.lock().unwrap().is_empty());
        assert_eq!(subscriber.track_alias_map.lock().unwrap().get(7), None);
    }

    /// Track Aliases are session-scoped (§10.1), so a second PUBLISH reusing a
    /// live alias for a different track is a session-closing condition, not a
    /// per-request error. Draft-18 assigns DUPLICATE_TRACK_ALIAS (0x5).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_track_alias_closes_the_session() {
        let mut subscriber = test_subscriber(loopback_session().await);

        subscriber
            .recv_publish(&message::Publish {
                id: 1,
                track_namespace: TrackNamespace::from_utf8_path("test/ns"),
                track_name: "first".into(),
                track_alias: 7,
                params: Default::default(),
                track_extensions: Default::default(),
            })
            .unwrap();

        let err = subscriber
            .recv_publish(&message::Publish {
                id: 2,
                track_namespace: TrackNamespace::from_utf8_path("test/ns"),
                track_name: "second".into(),
                track_alias: 7,
                params: Default::default(),
                track_extensions: Default::default(),
            })
            .expect_err("reusing a live alias must be rejected");

        assert!(matches!(err, SessionError::Duplicate));
        assert!(err.is_session_fatal());
        assert_eq!(err.code(), 0x5);

        // The collision must not disturb the binding that already owned the alias.
        assert_eq!(
            subscriber.track_alias_map.lock().unwrap().get(7),
            Some(TrackOrigin::Publish(1))
        );
    }

    /// PUBLISH_DONE must tear down the inbound PUBLISH state, including its
    /// alias and name reservations.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publish_done_clears_inbound_publish_state() {
        let mut subscriber = test_subscriber(loopback_session().await);
        subscriber
            .recv_publish(&message::Publish {
                id: 1,
                track_namespace: TrackNamespace::from_utf8_path("test/ns"),
                track_name: "video".into(),
                track_alias: 7,
                params: Default::default(),
                track_extensions: Default::default(),
            })
            .unwrap();

        subscriber
            .recv_publish_done(&message::PublishDone {
                id: 1,
                status_code: message::PublishDoneCode::TrackEnded as u64,
                stream_count: 0,
                reason: crate::coding::ReasonPhrase(String::new()),
            })
            .unwrap();

        assert!(subscriber.publishes_received.lock().unwrap().is_empty());
        assert!(subscriber.track_alias_map.lock().unwrap().is_empty());
    }

    /// §5.1 allows only one subscriber-role subscription per track, so the
    /// track name is derived from the request ID: a test that registers several
    /// inbound PUBLISHes at once would otherwise be rejected as a duplicate.
    fn inbound_publish(subscriber: &mut Subscriber, id: u64, track_alias: u64) {
        subscriber
            .recv_publish(&message::Publish {
                id,
                track_namespace: TrackNamespace::from_utf8_path("test/ns"),
                track_name: format!("video-{id}").into(),
                track_alias,
                params: Default::default(),
                track_extensions: Default::default(),
            })
            .unwrap();
    }

    fn publish_done(subscriber: &mut Subscriber, id: u64, stream_count: u64) {
        subscriber
            .recv_publish_done(&message::PublishDone {
                id,
                status_code: message::PublishDoneCode::TrackEnded as u64,
                stream_count,
                reason: crate::coding::ReasonPhrase(String::new()),
            })
            .unwrap();
    }

    /// §10.11: a subscription with streams still outstanding keeps its state,
    /// including the Track Alias, so the in-flight streams can still be routed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publish_done_keeps_state_while_streams_are_outstanding() {
        let (mut subscriber, _tasks) = test_subscriber_with_tasks(loopback_session().await);
        inbound_publish(&mut subscriber, 1, 7);

        publish_done(&mut subscriber, 1, 1);

        assert!(
            !subscriber.publishes_received.lock().unwrap().is_empty(),
            "state must survive so the announced stream can still be delivered"
        );
        assert!(
            !subscriber.track_alias_map.lock().unwrap().is_empty(),
            "the Track Alias must survive so the stream can be routed"
        );
    }

    /// PUBLISH_DONE is terminal on the request stream, so the request-stream
    /// teardown runs immediately afterwards. It must not cancel a drain, or
    /// the in-flight streams it is waiting for are lost.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborting_a_draining_publish_keeps_it_alive() {
        let (mut subscriber, _tasks) = test_subscriber_with_tasks(loopback_session().await);
        inbound_publish(&mut subscriber, 1, 7);
        publish_done(&mut subscriber, 1, 1);

        subscriber.abort_publish_received(1);

        assert!(
            !subscriber.publishes_received.lock().unwrap().is_empty(),
            "the request stream ending must not cancel an in-progress drain"
        );
        assert!(!subscriber.track_alias_map.lock().unwrap().is_empty());
    }

    /// Once the announced streams have been received and closed, everything
    /// keyed by the request ID is released; leaking the name reservation would
    /// make the track unpublishable for the rest of the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completing_a_drain_releases_all_subscription_state() {
        let (mut subscriber, _tasks) = test_subscriber_with_tasks(loopback_session().await);
        inbound_publish(&mut subscriber, 1, 7);
        publish_done(&mut subscriber, 1, 1);

        assert!(subscriber.note_publish_stream_received(1));
        subscriber.note_publish_stream_finished(1);

        assert!(
            subscriber.publishes_received.lock().unwrap().is_empty(),
            "the inbound PUBLISH entry must be released"
        );
        assert!(
            subscriber.track_alias_map.lock().unwrap().is_empty(),
            "the Track Alias must be released"
        );
        assert!(
            subscriber.subscriber_names.lock().unwrap().is_empty(),
            "the track name reservation must be released so it can be republished"
        );
    }

    /// The drain timeout must run as a session-owned background task, so it is
    /// cancelled with the session rather than outliving it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_drain_arms_exactly_one_session_owned_timer() {
        let (mut subscriber, mut tasks) = test_subscriber_with_tasks(loopback_session().await);
        inbound_publish(&mut subscriber, 1, 7);

        publish_done(&mut subscriber, 1, 1);
        // A duplicate must not arm a second timer.
        publish_done(&mut subscriber, 1, 1);

        let task = tasks.try_recv().expect("the drain is armed as a task");
        assert!(
            tasks.try_recv().is_err(),
            "a duplicate PUBLISH_DONE must not arm a second timer"
        );

        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("the drain timer completes")
            .expect("the drain task succeeds");

        assert!(subscriber.publishes_received.lock().unwrap().is_empty());
        assert!(subscriber.track_alias_map.lock().unwrap().is_empty());
    }

    /// A peer must not be able to pin unbounded state by ending subscriptions
    /// whose announced streams it never sends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn draining_subscriptions_are_capped() {
        let (mut subscriber, _tasks) = test_subscriber_with_tasks(loopback_session().await);

        // Peer-initiated request IDs are odd for a client session (§10.1).
        for n in 0..(MAX_CONCURRENT_DRAINS as u64 + 20) {
            let id = n * 2 + 1;
            inbound_publish(&mut subscriber, id, id + 1000);
            assert!(
                !subscriber.publishes_received.lock().unwrap().is_empty(),
                "the inbound PUBLISH must be registered for the cap to mean anything"
            );
            publish_done(&mut subscriber, id, 1);
        }

        assert!(
            subscriber.draining_count() <= MAX_CONCURRENT_DRAINS + 1,
            "draining subscriptions must stay bounded, got {}",
            subscriber.draining_count()
        );
    }

    /// A peer that never sends PUBLISH_DONE must not escape the drain cap by
    /// resetting the request stream instead — that path also defers teardown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_subscriptions_are_capped_too() {
        let (mut subscriber, _tasks) = test_subscriber_with_tasks(loopback_session().await);

        // Peer-initiated request IDs are odd for a client session (§10.1).
        for n in 0..(MAX_CONCURRENT_DRAINS as u64 + 20) {
            let id = n * 2 + 1;
            inbound_publish(&mut subscriber, id, id + 1000);
            // A data stream is mid-transfer, then the request stream dies.
            assert!(subscriber.note_publish_stream_received(id), "id={id}");
            subscriber.abort_publish_received(id);
        }

        assert!(
            subscriber.draining_count() <= MAX_CONCURRENT_DRAINS + 1,
            "draining subscriptions must stay bounded, got {}",
            subscriber.draining_count()
        );
    }

    /// The drain timeout is the backstop for a publisher that announced streams
    /// it never sent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_timeout_releases_a_stalled_subscription() {
        let (mut subscriber, _tasks) = test_subscriber_with_tasks(loopback_session().await);
        inbound_publish(&mut subscriber, 1, 7);
        publish_done(&mut subscriber, 1, 4);

        subscriber.expire_publish_done_drain(1);

        assert!(subscriber.publishes_received.lock().unwrap().is_empty());
        assert!(subscriber.track_alias_map.lock().unwrap().is_empty());
        assert!(subscriber.subscriber_names.lock().unwrap().is_empty());
    }
}
