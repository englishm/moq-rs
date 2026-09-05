// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    collections::{hash_map, HashMap, HashSet},
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::Notify;

use crate::{
    coding::{Decode, KeyValuePairs, TrackName, TrackNamespace, TrackNamespacePrefix},
    data,
    message::{self, Message, RequestErrorCode, SubscribeOptions},
    mlog,
    serve::{self, FullTrackName, ServeError},
};

use crate::watch::Queue;

use super::{
    OpenSubscribeNamespace, PendingRequest, PendingRequests, PublishReceived, PublishReceivedRecv,
    PublishedNamespace, PublishedNamespaceRecv, Reader, RequestId, RequestIdAllocation, Session,
    SessionConfig, SessionError, SessionId, Subscribe, SubscribeNamespace, SubscribeRecv,
};

// Default timeout for waiting for subscribe aliases to become available via SUBSCRIBE_OK (1 second)
const DEFAULT_ALIAS_WAIT_TIME_MS: u64 = 1000;

// TODO remove Clone.
#[derive(Clone)]
pub struct Subscriber {
    /// Active inbound PUBLISH_NAMESPACE messages, keyed by namespace.
    published_namespaces: Arc<Mutex<HashMap<TrackNamespace, PublishedNamespaceRecv>>>,

    /// Queue of inbound PUBLISH_NAMESPACE events waiting to be consumed by the application.
    published_namespace_queue: Queue<PublishedNamespace>,

    /// The currently active outbound subscribes, keyed by request id.
    subscribes: Arc<Mutex<HashMap<u64, SubscribeRecv>>>,

    /// Outbound TRACK_STATUS requests awaiting a shared REQUEST_OK / REQUEST_ERROR response.
    track_statuses: Arc<Mutex<HashSet<u64>>>,

    /// Unified map of Track Alias → owning subscription (draft-16 §10.1).
    ///
    /// Track Aliases are session-scoped and shared between SUBSCRIBE-initiated
    /// and PUBLISH-initiated subscriptions, so a single map keyed by alias
    /// resolves an inbound stream/datagram to the correct receiver.
    track_alias_map: Arc<Mutex<TrackAliasRegistry>>,

    /// Notify when `track_alias_map` is updated (for stream/datagram routing
    /// that may arrive before SUBSCRIBE_OK populates the alias).
    track_alias_notify: Arc<Notify>,

    /// Active inbound PUBLISH subscriptions, keyed by PUBLISH request id.
    /// The transport writes Objects into the TrackWriter stored here;
    /// the application receives the TrackReader from PublishReceived::ok.
    publishes_received: Arc<Mutex<HashMap<u64, PublishReceivedRecv>>>,

    /// Queue of inbound PUBLISH events waiting for the application to accept.
    publish_received_queue: Queue<PublishReceived>,

    /// Active outbound SUBSCRIBE_NAMESPACE requests, keyed by Request ID.
    subscribe_namespaces: Arc<Mutex<HashMap<u64, TrackNamespacePrefix>>>,

    /// Queue used to ask the session task to open a dedicated bidi stream.
    subscribe_namespace_open: Queue<OpenSubscribeNamespace>,

    /// Tracks which (namespace, name) pairs this endpoint is subscribed to
    /// (as subscriber role) — covers both outbound SUBSCRIBE and inbound PUBLISH.
    /// Used for §5.1 same-role duplicate-subscription enforcement.
    subscriber_names: Arc<Mutex<SubscriberNameRegistry>>,

    /// The queue we will write any outbound control messages we want to send, the session run_send task
    /// will process the queue and send the message on the control stream.
    outgoing: Queue<Message>,

    /// Shared with Publisher so all requests within a session use unique IDs.
    /// When we need a new Request Id for sending a request, we can get it from here.
    /// The manager is shared with the Publisher, so the session uses unique request ids
    /// for all requests generated.  If we initiated the QUIC connection then request
    /// IDs start at 0 and increment by 2 (even numbers).  If we accepted an inbound
    /// QUIC connection then request IDs start at 1 and increment by 2 (odd numbers).
    request_id: RequestId,

    /// Tracks outbound subscriber-role requests waiting for OK/error responses.
    pending_requests: PendingRequests,

    /// Optional mlog writer for logging transport events
    mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,

    /// Correlation id of the owning session, tagged onto this subscriber's log records.
    session_id: SessionId,
}

/// RAII guard that rolls back a SUBSCRIBE_NAMESPACE prefix reservation on failure.
///
/// `subscribe_namespace()` inserts the request's prefix into the shared
/// `subscribe_namespaces` map up front (used for §5.1 overlap detection) before the
/// dedicated stream is opened. If the open then fails, dropping this guard removes
/// that reservation so a failed request does not permanently block overlapping
/// subscriptions. On success the guard is [`disarm`](Self::disarm)ed, transferring
/// cleanup ownership to the returned `SubscribeNamespace` handle, whose
/// `SubscribeNamespaceRecv::drop` performs the eventual removal.
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

    /// Cancel the rollback once the request has succeeded and ownership of the
    /// reservation has passed to the returned `SubscribeNamespace` handle. Without
    /// this, the guard's `Drop` would immediately remove the just-registered prefix
    /// on the success path, tearing down the live subscription's state.
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
/// `track_alias_map` keyed by alias resolves inbound streams/datagrams to the
/// correct receiver.
#[derive(Clone, Copy, Debug)]
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

    fn insert(&mut self, alias: u64, origin: TrackOrigin) -> Result<(), ()> {
        if self.by_alias.contains_key(&alias) {
            return Err(());
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

#[derive(Default)]
struct SubscriberNameRegistry {
    by_name: HashMap<FullTrackName, u64>,
    by_request_id: HashMap<u64, FullTrackName>,
}

impl SubscriberNameRegistry {
    fn contains_name(&self, name: &FullTrackName) -> bool {
        self.by_name.contains_key(name)
    }

    fn insert(&mut self, name: FullTrackName, request_id: u64) {
        if let Some(old_name) = self.by_request_id.insert(request_id, name.clone()) {
            self.by_name.remove(&old_name);
        }
        if let Some(old_id) = self.by_name.insert(name, request_id) {
            self.by_request_id.remove(&old_id);
        }
    }

    fn remove_by_request_id(&mut self, request_id: u64) -> Option<FullTrackName> {
        let name = self.by_request_id.remove(&request_id)?;
        self.by_name.remove(&name);
        Some(name)
    }
}

impl Subscriber {
    pub(super) fn new(
        outgoing: Queue<Message>,
        subscribe_namespace_open: Queue<OpenSubscribeNamespace>,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
        request_id: RequestId,
        pending_requests: PendingRequests,
        session_id: SessionId,
    ) -> Self {
        Self {
            published_namespaces: Default::default(),
            published_namespace_queue: Default::default(),
            subscribes: Default::default(),
            track_statuses: Default::default(),
            track_alias_map: Default::default(),
            track_alias_notify: Arc::new(Notify::new()),
            publishes_received: Default::default(),
            publish_received_queue: Default::default(),
            subscribe_namespaces: Default::default(),
            subscribe_namespace_open,
            subscriber_names: Default::default(),
            outgoing,
            request_id,
            pending_requests,
            mlog,
            session_id,
        }
    }

    /// Correlation id of the session this subscriber belongs to.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Create an inbound/server QUIC connection, by accepting a bi-directional QUIC stream for control messages.
    ///
    /// Generates a local correlation ID. Use [`Self::accept_with_session_id`] when a peer-observed
    /// QUIC connection ID is available.
    pub async fn accept(
        session: web_transport::Session,
        transport: super::Transport,
    ) -> Result<(Session, Self), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `accept` accept it and remove `accept_with_session_id`.
        Self::accept_with_session_id(session, SessionId::generate(), transport).await
    }

    /// Accept a configured subscriber session using a generated local correlation ID.
    ///
    /// Use [`Self::accept_with_config_and_session_id`] when a peer-observed QUIC connection ID is
    /// available.
    pub async fn accept_with_config(
        session: web_transport::Session,
        transport: super::Transport,
        config: SessionConfig,
    ) -> Result<(Session, Self), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `accept_with_config` accept it and remove `accept_with_config_and_session_id`.
        Self::accept_with_config_and_session_id(session, SessionId::generate(), transport, config)
            .await
    }

    /// Accept a subscriber session using a peer-observed QUIC connection ID.
    pub async fn accept_with_session_id(
        session: web_transport::Session,
        session_id: SessionId,
        transport: super::Transport,
    ) -> Result<(Session, Self), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `accept_with_config` accept it and remove `accept_with_config_and_session_id`.
        Self::accept_with_config_and_session_id(
            session,
            session_id,
            transport,
            SessionConfig::default(),
        )
        .await
    }

    /// Accept a configured subscriber session using a peer-observed QUIC connection ID.
    pub async fn accept_with_config_and_session_id(
        session: web_transport::Session,
        session_id: SessionId,
        transport: super::Transport,
        config: SessionConfig,
    ) -> Result<(Session, Self), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `accept_with_config` accept it and remove `accept_with_config_and_session_id`.
        let (session, _, subscriber) = Session::accept_with_config_and_session_id(
            session, session_id, None, transport, config,
        )
        .await?;
        let subscriber = subscriber.ok_or(SessionError::Internal)?;
        Ok((session, subscriber))
    }

    /// Create an outbound/client QUIC connection, by opening a bi-directional QUIC stream for control messages.
    ///
    /// Generates a local correlation ID. Use [`Self::connect_with_session_id`] when a peer-observed
    /// QUIC connection ID is available.
    pub async fn connect(
        session: web_transport::Session,
        transport: super::Transport,
    ) -> Result<(Session, Self), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `connect` accept it and remove `connect_with_session_id`.
        Self::connect_with_session_id(session, SessionId::generate(), transport).await
    }

    /// Create a configured subscriber session using a generated local correlation ID.
    ///
    /// Use [`Self::connect_with_config_and_session_id`] when a peer-observed QUIC connection ID is
    /// available.
    pub async fn connect_with_config(
        session: web_transport::Session,
        transport: super::Transport,
        config: SessionConfig,
    ) -> Result<(Session, Self), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `connect_with_config` accept it and remove `connect_with_config_and_session_id`.
        Self::connect_with_config_and_session_id(session, SessionId::generate(), transport, config)
            .await
    }

    /// Create a subscriber session using a peer-observed QUIC connection ID.
    pub async fn connect_with_session_id(
        session: web_transport::Session,
        session_id: SessionId,
        transport: super::Transport,
    ) -> Result<(Session, Self), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `connect_with_config` accept it and remove `connect_with_config_and_session_id`.
        Self::connect_with_config_and_session_id(
            session,
            session_id,
            transport,
            SessionConfig::default(),
        )
        .await
    }

    /// Create a configured subscriber session using a peer-observed QUIC connection ID.
    pub async fn connect_with_config_and_session_id(
        session: web_transport::Session,
        session_id: SessionId,
        transport: super::Transport,
        config: SessionConfig,
    ) -> Result<(Session, Self), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `connect_with_config` accept it and remove `connect_with_config_and_session_id`.
        let (session, _, subscriber) = Session::connect_with_config_and_session_id(
            session, session_id, None, transport, config,
        )
        .await?;
        Ok((session, subscriber))
    }

    /// Wait for the next inbound PUBLISH_NAMESPACE from the peer, if any.
    pub async fn published_namespace(&mut self) -> Option<PublishedNamespace> {
        self.published_namespace_queue.pop().await
    }

    /// Wait for the next inbound PUBLISH from the peer, if any.
    ///
    /// The returned [`PublishReceived`] must be accepted with
    /// [`PublishReceived::ok`] or dropped to reject.
    pub async fn publish_received(&mut self) -> Option<PublishReceived> {
        self.publish_received_queue.pop().await
    }

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

        let (reply, recv) = futures::channel::oneshot::channel();
        let open = OpenSubscribeNamespace::new(
            self.clone(),
            request_id,
            namespace_prefix,
            subscribe_options,
            params,
            reply,
        );

        if self.subscribe_namespace_open.push(open).is_err() {
            return Err(SessionError::Internal);
        }

        match recv.await {
            Ok(Ok(subscribe_namespace)) => {
                cleanup.disarm();
                Ok(subscribe_namespace)
            }
            Ok(Err(err)) => Err(err),
            Err(_) => Err(SessionError::Internal),
        }
    }

    /// Remove all subscriber-side state for an inbound PUBLISH.
    ///
    /// Called by `PublishReceived::drop` when the app did not call `ok()`.
    pub(super) fn remove_publish_received(&self, request_id: u64) {
        if let Err(err) = self.remove_publish_received_state(request_id) {
            tracing::error!(session_id = %self.session_id, request_id, error = %err, "failed to remove inbound PUBLISH state");
        }
    }

    fn remove_publish_received_state(&self, request_id: u64) -> Result<(), SessionError> {
        self.publishes_received
            .lock()
            .map_err(|_| SessionError::Internal)?
            .remove(&request_id);
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

    pub(super) fn remove_subscribe_namespace(&self, request_id: u64) {
        if let Ok(mut prefixes) = self.subscribe_namespaces.lock() {
            prefixes.remove(&request_id);
        }
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

    /// Allocate the next outbound request ID, enforcing the peer-advertised maximum.
    ///
    /// Returns `Err(TooManyRequests)` if no budget remains and also sends
    /// REQUESTS_BLOCKED if not already sent for this limit.
    fn get_next_request_id(&mut self) -> Result<u64, SessionError> {
        match self.request_id.allocate()? {
            RequestIdAllocation::Allocated(id) => Ok(id),
            blocked @ RequestIdAllocation::Blocked { .. } => {
                if let Some(msg) = blocked.requests_blocked() {
                    let _ = self.outgoing.push(msg.into());
                }
                Err(SessionError::TooManyRequests)
            }
        }
    }

    pub fn track_status(
        &mut self,
        track_namespace: &TrackNamespace,
        track_name: impl Into<TrackName>,
    ) {
        let id = match self.get_next_request_id() {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(session_id = %self.session_id, error = %e, "could not send TRACK_STATUS: request ID limit reached");
                return;
            }
        };
        if let Ok(mut track_statuses) = self.track_statuses.lock() {
            track_statuses.insert(id);
        } else {
            tracing::warn!(session_id = %self.session_id, "could not track outbound TRACK_STATUS: lock poisoned");
            return;
        }
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
    pub async fn subscribe_open(
        &mut self,
        track: serve::TrackWriter,
    ) -> Result<Subscribe, ServeError> {
        let request_id = self
            .get_next_request_id()
            .map_err(|e| ServeError::internal_ctx(format!("request ID limit: {}", e)))?;

        // §5.1: enforce single subscriber-role subscription per track.
        // Both outbound SUBSCRIBE and inbound PUBLISH make this endpoint the
        // subscriber for the track.
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
            names.insert(full_name.clone(), request_id);
        }

        if let Err(err) = self
            .pending_requests
            .insert(request_id, PendingRequest::Subscribe)
        {
            if let Ok(mut names) = self.subscriber_names.lock() {
                names.remove_by_request_id(request_id);
            }
            return Err(ServeError::internal_ctx(format!(
                "pending request insert: {}",
                err
            )));
        }

        let (mut send, recv) = Subscribe::new(self.clone(), request_id, track);
        match self.subscribes.lock() {
            Ok(mut subscribes) => {
                subscribes.insert(request_id, recv);
            }
            Err(_) => {
                let _ = self.pending_requests.remove(request_id);
                if let Ok(mut names) = self.subscriber_names.lock() {
                    names.remove_by_request_id(request_id);
                }
                return Err(ServeError::internal_ctx("subscribe lock poisoned"));
            }
        }
        send.send_request();
        send.ok().await?;
        Ok(send)
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
            // PUBLISH: publisher-initiated subscription (draft-16 §9.13).
            message::Publisher::Publish(msg) => self.recv_publish(msg)?,
            // PUBLISH_DONE terminates either a SUBSCRIBE-created or PUBLISH-created
            // subscription.  The request id alone does not tell us which map to look
            // in; we check both (§9.15).
            message::Publisher::PublishDone(msg) => self.recv_publish_done(msg)?,
            message::Publisher::SubscribeOk(msg) => self.recv_subscribe_ok(msg)?,
            // Draft-16 shared responses (REQUEST_OK / REQUEST_ERROR).
            message::Publisher::RequestOk(msg) => self.recv_request_ok(msg)?,
            message::Publisher::RequestError(msg) => self.recv_request_error(msg)?,
            // FETCH_OK is part of draft-16, but FETCH is not implemented here yet.
            message::Publisher::FetchOk(msg) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %self.session_id,
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

    /// Handle the reception of a SUBSCRIBE_OK message from the publisher.
    pub(super) fn recv_subscribe_ok(
        &mut self,
        msg: &message::SubscribeOk,
    ) -> Result<(), SessionError> {
        if let Some(subscribe) = self
            .subscribes
            .lock()
            .map_err(|_| SessionError::Internal)?
            .get_mut(&msg.id)
        {
            // Track Aliases are session-scoped (§10.1).  The SUBSCRIBE_OK alias
            // must not collide with any alias already bound by a SUBSCRIBE or a
            // PUBLISH subscription.
            {
                let mut aliases = self
                    .track_alias_map
                    .lock()
                    .map_err(|_| SessionError::Internal)?;
                if aliases.contains_alias(msg.track_alias) {
                    return Err(SessionError::Duplicate);
                }
                aliases
                    .insert(msg.track_alias, TrackOrigin::Subscribe(msg.id))
                    .map_err(|_| SessionError::Duplicate)?;
            }

            // Notify waiting tasks that the alias map has been updated.
            self.track_alias_notify.notify_waiters();

            // Notify the subscribe of the successful subscription.
            subscribe.ok(msg.track_alias)?;
        }

        Ok(())
    }

    /// Remove a subscribe from our map of active subscribes, the alias map, and subscriber_names.
    pub(super) fn remove_subscribe(&mut self, id: u64) -> Option<SubscribeRecv> {
        let _ = self.pending_requests.remove(id);
        let subscribe = self.subscribes.lock().ok().and_then(|mut s| s.remove(&id));
        if let Ok(mut alias_map) = self.track_alias_map.lock() {
            alias_map.remove_by_request_id(id);
        }
        if let Ok(mut names) = self.subscriber_names.lock() {
            names.remove_by_request_id(id);
        }
        subscribe
    }

    /// Handle an inbound PUBLISH from the publisher (draft-16 §9.13).
    ///
    /// This establishes a publisher-initiated subscription.  The endpoint
    /// becomes the subscriber for this track.
    fn recv_publish(&mut self, msg: &message::Publish) -> Result<(), SessionError> {
        // First-cut policy: reject non-empty TrackExtensions.
        // We do not yet propagate track extensions through the serve model, so
        // accepting them would silently drop relay-visible metadata (§8.6).
        // TODO(itzmanish): lift this restriction once TrackExtensions are
        // carried through TrackReader/TrackWriter.
        if !msg.track_extensions.is_empty() {
            self.send_request_error(
                "publish",
                message::RequestError {
                    id: msg.id,
                    error_code: RequestErrorCode::NotSupported as u64,
                    retry_interval: 0,
                    reason: crate::coding::ReasonPhrase(
                        "track extensions not supported".to_string(),
                    ),
                },
            );
            return Ok(());
        }

        // §5.1: enforce single subscriber-role subscription per track.
        // Both outbound SUBSCRIBE and inbound PUBLISH make this endpoint the
        // subscriber for the given (namespace, name).
        let full_name = FullTrackName {
            namespace: msg.track_namespace.clone(),
            name: msg.track_name.clone(),
        };

        // Parse FORWARD and LARGEST_OBJECT from the PUBLISH params before
        // reserving session state, so malformed parameters cannot leave stale
        // alias/name entries behind.
        let initial_forward = msg
            .params
            .forward()
            .map_err(SessionError::Decode)?
            .unwrap_or(true);
        let largest_location = msg.params.largest_object().map_err(SessionError::Decode)?;

        // Reserve the track name before queueing the PublishReceived. The
        // duplicate-subscription check intentionally runs before the alias
        // check so a duplicate PUBLISH for the same track is rejected as a
        // request error instead of a duplicate-alias session close.
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
                        error_code: RequestErrorCode::DuplicateSubscription as u64,
                        retry_interval: 0,
                        reason: crate::coding::ReasonPhrase("duplicate subscription".to_string()),
                    },
                );
                return Ok(());
            }

            names.insert(full_name.clone(), msg.id);
        }

        // Then reserve the alias without holding subscriber_names, avoiding a
        // lock-order dependency between cleanup and inbound PUBLISH handling.
        let alias_result = match self.track_alias_map.lock() {
            Ok(mut aliases) => {
                if aliases.contains_alias(msg.track_alias) {
                    // §9.13: duplicate Track Alias for a different track closes the session.
                    Err(SessionError::Duplicate)
                } else {
                    aliases
                        .insert(msg.track_alias, TrackOrigin::Publish(msg.id))
                        .map_err(|_| SessionError::Duplicate)
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

        // Allocate the track.  The transport owns the writer; the application
        // receives the reader via PublishReceived::ok.
        let (writer, reader) =
            crate::serve::Track::new(msg.track_namespace.clone(), msg.track_name.clone()).produce();

        // Build both handles sharing the same state.
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

        // The alias was registered before queueing the PublishReceived so that
        // Object streams racing the PUBLISH (§5.1 allows pre-PUBLISH_OK
        // delivery) can be resolved immediately.
        self.track_alias_notify.notify_waiters();

        // Store the transport recv handle keyed by request id.
        match self.publishes_received.lock() {
            Ok(mut publishes_received) => {
                publishes_received.insert(msg.id, recv);
            }
            Err(_) => {
                if let Ok(mut aliases) = self.track_alias_map.lock() {
                    aliases.remove_by_request_id(msg.id);
                }
                if let Ok(mut names) = self.subscriber_names.lock() {
                    names.remove_by_request_id(msg.id);
                }
                return Err(SessionError::Internal);
            }
        }

        tracing::debug!(
            target: "moq_transport::control",
            session_id = %self.session_id,
            request_id = msg.id,
            track_alias = msg.track_alias,
            namespace = %msg.track_namespace,
            name = %msg.track_name,
            forward = initial_forward,
            "received PUBLISH"
        );

        // If the application is no longer listening, drop the PublishReceived
        // which sends REQUEST_ERROR back to the publisher.
        if self.publish_received_queue.push(publish_received).is_err() {
            // Queue is closed; clean up state we just inserted.
            self.remove_publish_received(msg.id);
        }

        Ok(())
    }

    /// Handle PUBLISH_DONE from the publisher (draft-16 §9.15).
    ///
    /// PUBLISH_DONE terminates either a SUBSCRIBE-created subscription
    /// (publisher sends PUBLISH_DONE after SUBSCRIBE was sent to it) or a
    /// PUBLISH-created subscription (publisher sends PUBLISH_DONE to end its
    /// push).  The request id alone does not tell us which map to look in,
    /// so we check both.
    fn recv_publish_done(&mut self, msg: &message::PublishDone) -> Result<(), SessionError> {
        // Check SUBSCRIBE-initiated subscriptions first.
        if let Some(subscribe) = self.remove_subscribe(msg.id) {
            let err = if msg.status_code == message::PublishDoneCode::TrackEnded as u64 {
                ServeError::Done
            } else {
                ServeError::Closed(msg.status_code)
            };
            subscribe.error(err)?;
            return Ok(());
        }

        // Then check PUBLISH-initiated subscriptions.
        let recv = self
            .publishes_received
            .lock()
            .map_err(|_| SessionError::Internal)?
            .remove(&msg.id);
        if let Some(mut recv) = recv {
            recv.recv_done(msg.status_code);
            self.track_alias_map
                .lock()
                .map_err(|_| SessionError::Internal)?
                .remove_by_request_id(msg.id);
            self.subscriber_names
                .lock()
                .map_err(|_| SessionError::Internal)?
                .remove_by_request_id(msg.id);
        } else {
            tracing::debug!(
                target: "moq_transport::control",
                session_id = %self.session_id,
                request_id = msg.id,
                "received PUBLISH_DONE for unknown subscription — ignoring"
            );
        }

        Ok(())
    }

    /// Handle REQUEST_OK from the publisher (draft-16 §9.7).
    ///
    /// REQUEST_OK is the shared positive response for REQUEST_UPDATE, TRACK_STATUS,
    /// SUBSCRIBE_NAMESPACE, and PUBLISH_NAMESPACE. SUBSCRIBE uses its own dedicated
    /// SUBSCRIBE_OK message (§9.10) and does not come through this handler.
    fn recv_request_ok(&mut self, msg: &message::RequestOk) -> Result<(), SessionError> {
        if self.drop_track_status(msg.id)? {
            let request_kind = "track_status";
            self.log_request_ok_parsed(request_kind, msg);
            tracing::debug!(
                target: "moq_transport::control",
                session_id = %self.session_id,
                request_id = msg.id,
                request_kind,
                "received REQUEST_OK"
            );
            return Ok(());
        }

        let request_kind = "unknown";
        self.log_request_ok_parsed(request_kind, msg);
        tracing::debug!(
            target: "moq_transport::control",
            session_id = %self.session_id,
            request_id = msg.id,
            request_kind,
            "received REQUEST_OK"
        );
        Ok(())
    }

    /// Handle REQUEST_ERROR from the publisher (draft-16 §9.8).
    ///
    /// Routes to an active SUBSCRIBE by request id, otherwise logs and ignores.
    pub(super) fn recv_request_error(
        &mut self,
        msg: &message::RequestError,
    ) -> Result<(), SessionError> {
        // Route to a matching SUBSCRIBE if present.
        if let Some(subscribe) = self.remove_subscribe(msg.id) {
            self.log_request_error_parsed("subscribe", msg);
            subscribe.error(ServeError::Closed(msg.error_code))?;
            tracing::debug!(
                target: "moq_transport::control",
                session_id = %self.session_id,
                request_id = msg.id,
                request_kind = "subscribe",
                error_code = msg.error_code,
                retry_interval = msg.retry_interval,
                reason = ?msg.reason.0,
                "received REQUEST_ERROR"
            );
            return Ok(());
        } else if self.drop_track_status(msg.id)? {
            self.log_request_error_parsed("track_status", msg);
            tracing::debug!(
                target: "moq_transport::control",
                session_id = %self.session_id,
                request_id = msg.id,
                request_kind = "track_status",
                error_code = msg.error_code,
                retry_interval = msg.retry_interval,
                reason = ?msg.reason.0,
                "received REQUEST_ERROR"
            );
            return Ok(());
        }

        self.log_request_error_parsed("unknown", msg);
        tracing::debug!(
            target: "moq_transport::control",
            session_id = %self.session_id,
            request_id = msg.id,
            request_kind = "unknown",
            error_code = msg.error_code,
            retry_interval = msg.retry_interval,
            reason = ?msg.reason.0,
            "received REQUEST_ERROR"
        );
        Ok(())
    }

    pub(super) fn recv_request_timeout(
        &mut self,
        id: u64,
        request: PendingRequest,
    ) -> Result<(), SessionError> {
        match request {
            PendingRequest::Subscribe => {
                if let Some(subscribe) = self.remove_subscribe(id) {
                    subscribe.error(ServeError::internal_ctx("SUBSCRIBE response timed out"))?;
                }
            }
            PendingRequest::PublishNamespace | PendingRequest::Publish => {
                return Err(SessionError::Internal)
            }
        }

        Ok(())
    }

    fn drop_track_status(&mut self, id: u64) -> Result<bool, SessionError> {
        Ok(self
            .track_statuses
            .lock()
            .map_err(|_| SessionError::Internal)?
            .remove(&id))
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

    /// Resolve a Track Alias to its owning subscription (draft-16 §10.1).
    ///
    /// PUBLISH aliases are registered eagerly in `recv_publish` (before
    /// PUBLISH_OK), and SUBSCRIBE aliases in `recv_subscribe_ok`. A stream or
    /// datagram can still race ahead of SUBSCRIBE_OK, so when `timeout_ms` is
    /// set we wait up to that long for the alias to appear.
    ///
    /// Returns `None` if the alias is not found (immediately when `timeout_ms`
    /// is `None`, or after the timeout otherwise).
    async fn get_track_origin_by_alias(
        &self,
        track_alias: u64,
        timeout_ms: Option<u64>,
    ) -> Result<Option<TrackOrigin>, SessionError> {
        // Fast path: already present.
        {
            let aliases = self
                .track_alias_map
                .lock()
                .map_err(|_| SessionError::Internal)?;
            if let Some(origin) = aliases.get(track_alias) {
                return Ok(Some(origin));
            }
        }

        let timeout_ms = match timeout_ms {
            Some(ms) => ms,
            None => return Ok(None),
        };

        let timeout_duration = Duration::from_millis(timeout_ms);
        tokio::time::timeout(timeout_duration, async {
            loop {
                // Register notification before checking to avoid a TOCTOU gap.
                let notified = self.track_alias_notify.notified();

                {
                    let aliases = self
                        .track_alias_map
                        .lock()
                        .map_err(|_| SessionError::Internal)?;
                    if let Some(origin) = aliases.get(track_alias) {
                        return Ok(Some(origin));
                    }
                }

                notified.await;
            }
        })
        .await
        .unwrap_or(Ok(None))
    }

    /// Handle reception of a new stream from the QUIC session.
    pub(super) async fn recv_stream(
        mut self,
        stream: web_transport::RecvStream,
    ) -> Result<(), SessionError> {
        tracing::trace!("[SUBSCRIBER] recv_stream: new stream received, decoding header");
        let mut reader = Reader::new(self.session_id.clone(), stream);

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

        let subgroup_header = stream_header
            .subgroup_header
            .ok_or(SessionError::Internal)?;

        // Log subgroup header parsed/received
        if let Some(ref mlog) = self.mlog {
            if let Ok(mut mlog_guard) = mlog.lock() {
                let time = mlog_guard.elapsed_ms();
                let stream_id = 0; // TODO: Placeholder, need actual QUIC stream ID
                let event = mlog::subgroup_header_parsed(time, stream_id, &subgroup_header);
                let _ = mlog_guard.add_event(event);
            }
        }

        let track_alias = subgroup_header.track_alias;
        tracing::trace!(
            "[SUBSCRIBER] recv_stream: stream for subscription track_alias={}",
            track_alias
        );

        let mlog = self.mlog.clone();
        let res = self
            .recv_stream_inner(reader, stream_header.header_type, subgroup_header, mlog)
            .await;
        if let Err(SessionError::Serve(err)) = &res {
            tracing::warn!(
                session_id = %self.session_id,
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
        stream_header_type: data::StreamHeaderType,
        subgroup_header: data::SubgroupHeader,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        let track_alias = subgroup_header.track_alias;
        tracing::trace!(
            "[SUBSCRIBER] recv_stream_inner: processing stream for track_alias={}",
            track_alias
        );

        let origin = self
            .get_track_origin_by_alias(track_alias, Some(DEFAULT_ALIAS_WAIT_TIME_MS))
            .await?
            .ok_or_else(|| {
                SessionError::Serve(ServeError::not_found_ctx(format!(
                    "track_alias={} not found",
                    track_alias
                )))
            })?;

        self.recv_subgroup(stream_header_type, subgroup_header, origin, reader, mlog)
            .await?;

        tracing::trace!(
            "[SUBSCRIBER] recv_stream_inner: completed processing stream for track_alias={}",
            track_alias
        );
        Ok(())
    }

    /// Decode subgroup objects from a stream and write them via `get_writer`.
    ///
    /// This is the single implementation of the subgroup receive loop shared
    /// by both SUBSCRIBE-initiated and PUBLISH-initiated subscriptions.
    ///
    /// `get_writer` is called once — on the first object in the subgroup — to
    /// obtain a `SubgroupWriter`.  It receives the (possibly updated) subgroup
    /// header and must open the writer against the correct map entry (either
    /// `subscribes` for outbound SUBSCRIBE, or `publishes_received` for inbound
    /// PUBLISH).  Keeping this as a closure avoids duplicating ~100 lines of
    /// object decoding, ID tracking, validation, logging, and payload reading.
    async fn recv_subgroup_objects(
        session_id: SessionId,
        stream_header_type: data::StreamHeaderType,
        mut subgroup_header: data::SubgroupHeader,
        mut reader: Reader,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
        mut get_writer: impl FnMut(data::SubgroupHeader) -> Result<serve::SubgroupWriter, SessionError>,
    ) -> Result<(), SessionError> {
        tracing::trace!(
            "[SUBSCRIBER] recv_subgroup_objects: starting - group_id={}, subgroup_id={:?}, priority={}",
            subgroup_header.group_id,
            subgroup_header.subgroup_id,
            subgroup_header.publisher_priority
        );

        let mut object_count = 0;
        let mut previous_object_id: Option<u64> = None;
        let mut subgroup_writer: Option<serve::SubgroupWriter> = None;

        while !reader.done().await? {
            // Decode the object header.  Extension-header variant carries extra
            // relay-visible fields; plain variant does not.
            let (mut remaining_bytes, object_id_delta, status, decoded_object) =
                if stream_header_type.has_extension_headers() {
                    let object = reader.decode::<data::SubgroupObjectExt>().await?;
                    tracing::trace!(
                        "[SUBSCRIBER] recv_subgroup_objects: object #{} with ext headers \
                         object_id_delta={} payload={} status={:?} ext={:?}",
                        object_count + 1,
                        object.object_id_delta,
                        object.payload_length,
                        object.status,
                        object.extension_headers
                    );
                    // Check for known draft-14 extension types
                    if object.extension_headers.has(0xB) {
                        tracing::trace!(
                            "[SUBSCRIBER] recv_subgroup_objects: object #{} has IMMUTABLE EXTENSIONS (0xB)",
                            object_count + 1
                        );
                    }
                    if object.extension_headers.has(0x3C) {
                        tracing::trace!(
                            "[SUBSCRIBER] recv_subgroup_objects: object #{} has PRIOR GROUP ID GAP (0x3C)",
                            object_count + 1
                        );
                    }
                    let obj_copy = object.clone();
                    (
                        object.payload_length,
                        object.object_id_delta,
                        object.status,
                        Some(obj_copy),
                    )
                } else {
                    let object = reader.decode::<data::SubgroupObject>().await?;
                    tracing::trace!(
                        "[SUBSCRIBER] recv_subgroup_objects: object #{} \
                         object_id_delta={} payload={} status={:?}",
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
                };

            // Compute the absolute object ID from the delta.
            let current_object_id = match previous_object_id {
                Some(prev) => prev
                    .checked_add(object_id_delta)
                    .and_then(|v| v.checked_add(1))
                    .ok_or_else(|| {
                        SessionError::ProtocolViolation("subgroup object id overflow".to_string())
                    })?,
                None => object_id_delta,
            };
            previous_object_id = Some(current_object_id);

            let extension_headers = decoded_object.as_ref().map(|o| o.extension_headers.clone());

            // Non-normal status with extension headers is a protocol violation.
            if status.is_some_and(|s| s != data::ObjectStatus::NormalObject)
                && extension_headers.as_ref().is_some_and(|h| !h.is_empty())
            {
                return Err(SessionError::ProtocolViolation(
                    "non-normal object status with extension headers".to_string(),
                ));
            }

            // Open the subgroup writer on the first object.
            if subgroup_writer.is_none() {
                if stream_header_type.uses_first_object_id_as_subgroup_id() {
                    subgroup_header.subgroup_id = Some(current_object_id);
                }
                subgroup_writer = Some(get_writer(subgroup_header.clone())?);
            }

            // Log the object.
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

            // Write the object payload.
            // TODO SLG - object_id_delta and object status are still being ignored
            let subgroup_writer = subgroup_writer.as_mut().ok_or(SessionError::Internal)?;
            let mut object_writer = subgroup_writer.create(remaining_bytes, extension_headers)?;

            while remaining_bytes > 0 {
                let chunk = reader.read_chunk(remaining_bytes).await?.ok_or_else(|| {
                    tracing::error!(
                        session_id = %session_id,
                        "[SUBSCRIBER] recv_subgroup_objects: stream ended with {} bytes remaining",
                        remaining_bytes
                    );
                    SessionError::WrongSize
                })?;
                remaining_bytes -= chunk.len();
                object_writer.write(chunk)?;
            }

            object_count += 1;
        }

        tracing::trace!(
            "[SUBSCRIBER] recv_subgroup_objects: completed (group_id={}, subgroup_id={}, {} objects)",
            subgroup_header.group_id,
            subgroup_header.subgroup_id.unwrap_or(0),
            object_count
        );
        Ok(())
    }

    /// Handle subgroup stream data, routing the writer through the owning
    /// subscription (SUBSCRIBE or PUBLISH) identified by `origin`.
    ///
    /// The shared object loop (`recv_subgroup_objects`) opens the writer lazily
    /// on the first object via the closure, which preserves the case where the
    /// subgroup id is derived from the first object id.
    async fn recv_subgroup(
        &mut self,
        stream_header_type: data::StreamHeaderType,
        subgroup_header: data::SubgroupHeader,
        origin: TrackOrigin,
        reader: Reader,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        let subscribes = self.subscribes.clone();
        let publishes_received = self.publishes_received.clone();
        Self::recv_subgroup_objects(
            self.session_id.clone(),
            stream_header_type,
            subgroup_header,
            reader,
            mlog,
            move |header| match origin {
                TrackOrigin::Subscribe(subscribe_id) => {
                    let mut map = subscribes.lock().map_err(|_| SessionError::Internal)?;
                    let recv = map.get_mut(&subscribe_id).ok_or_else(|| {
                        SessionError::Serve(ServeError::not_found_ctx(format!(
                            "subscribe_id={} not found for track_alias={}",
                            subscribe_id, header.track_alias
                        )))
                    })?;
                    Ok(recv.subgroup(header)?)
                }
                TrackOrigin::Publish(publish_id) => {
                    let mut map = publishes_received
                        .lock()
                        .map_err(|_| SessionError::Internal)?;
                    let recv = map.get_mut(&publish_id).ok_or_else(|| {
                        SessionError::Serve(ServeError::not_found_ctx(format!(
                            "publish_id={} not found for track_alias={}",
                            publish_id, header.track_alias
                        )))
                    })?;
                    Ok(recv.subgroup(header)?)
                }
            },
        )
        .await
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

        let track_alias = datagram.track_alias;
        let origin = self
            .get_track_origin_by_alias(track_alias, Some(DEFAULT_ALIAS_WAIT_TIME_MS))
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
                        "[SUBSCRIBER] recv_datagram (SUBSCRIBE): track_alias={}, group_id={}, object_id={}",
                        track_alias,
                        datagram.group_id,
                        datagram.object_id.unwrap_or(0)
                    );
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
                        "[SUBSCRIBER] recv_datagram (PUBLISH): track_alias={}, group_id={}, object_id={}",
                        track_alias,
                        datagram.group_id,
                        datagram.object_id.unwrap_or(0)
                    );
                    recv.datagram(datagram)?;
                }
            }
            None => {
                tracing::warn!(
                    session_id = %self.session_id,
                    "[SUBSCRIBER] recv_datagram: discarded due to unknown track_alias={}, group_id={}, object_id={}",
                    track_alias,
                    datagram.group_id,
                    datagram.object_id.unwrap_or(0)
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::task::Poll;

    use super::*;
    use crate::{message, serve::Track};

    fn subscriber() -> Subscriber {
        let request_id = RequestId::new(0, 100, 100, 0);
        Subscriber::new(
            Queue::default(),
            Queue::default(),
            None,
            request_id,
            PendingRequests::default(),
            SessionId::generate(),
        )
    }

    fn publish_msg(id: u64, alias: u64, track: &str) -> message::Publish {
        message::Publish {
            id,
            track_namespace: TrackNamespace::from_utf8_path("test"),
            track_name: TrackName::from(track),
            track_alias: alias,
            params: Default::default(),
            track_extensions: Default::default(),
        }
    }

    #[tokio::test]
    async fn duplicate_publish_for_same_track_sends_request_error() {
        let mut subscriber = subscriber();

        subscriber
            .recv_publish(&publish_msg(1, 10, "0.mp4"))
            .unwrap();
        subscriber
            .recv_publish(&publish_msg(3, 10, "0.mp4"))
            .unwrap();

        let msg = subscriber.outgoing.pop().await.unwrap();
        let message::Message::RequestError(err) = msg else {
            panic!("expected REQUEST_ERROR for duplicate PUBLISH");
        };
        assert_eq!(err.id, 3);
        assert_eq!(
            err.error_code,
            RequestErrorCode::DuplicateSubscription as u64
        );
    }

    #[test]
    fn duplicate_publish_alias_for_different_track_closes_session() {
        let mut subscriber = subscriber();

        subscriber
            .recv_publish(&publish_msg(1, 10, "0.mp4"))
            .unwrap();
        let err = subscriber.recv_publish(&publish_msg(3, 10, "1.mp4"));

        assert!(matches!(err, Err(SessionError::Duplicate)));

        let failed_name = FullTrackName {
            namespace: TrackNamespace::from_utf8_path("test"),
            name: TrackName::from("1.mp4"),
        };
        assert!(!subscriber
            .subscriber_names
            .lock()
            .unwrap()
            .contains_name(&failed_name));
    }

    #[test]
    fn request_ok_routes_to_pending_track_status() {
        let mut subscriber = subscriber();
        let namespace = TrackNamespace::from_utf8_path("test");

        subscriber.track_status(&namespace, "0.mp4");
        assert!(subscriber.track_statuses.lock().unwrap().contains(&0));

        subscriber
            .recv_request_ok(&message::RequestOk {
                id: 0,
                params: Default::default(),
            })
            .unwrap();

        assert!(subscriber.track_statuses.lock().unwrap().is_empty());
    }

    #[test]
    fn request_error_routes_to_pending_track_status() {
        let mut subscriber = subscriber();
        let namespace = TrackNamespace::from_utf8_path("test");

        subscriber.track_status(&namespace, "0.mp4");
        assert!(subscriber.track_statuses.lock().unwrap().contains(&0));

        subscriber
            .recv_request_error(&message::RequestError {
                id: 0,
                error_code: message::RequestErrorCode::DoesNotExist as u64,
                retry_interval: 0,
                reason: crate::coding::ReasonPhrase("not found".to_string()),
            })
            .unwrap();

        assert!(subscriber.track_statuses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn subscribe_open_cleans_up_when_cancelled_before_ok() {
        let mut subscriber = subscriber();
        let observer = subscriber.clone();
        let (writer, _reader) =
            Track::new(TrackNamespace::from_utf8_path("test"), "0.mp4").produce();

        {
            let subscribe = subscriber.subscribe_open(writer);
            futures::pin_mut!(subscribe);

            assert!(matches!(futures::poll!(&mut subscribe), Poll::Pending));
            assert_eq!(observer.subscribes.lock().unwrap().len(), 1);
        }

        assert!(observer.subscribes.lock().unwrap().is_empty());
        assert!(observer.track_alias_map.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dropping_open_subscribe_removes_recv_state() {
        let mut subscriber = subscriber();
        let observer = subscriber.clone();
        let (writer, _reader) =
            Track::new(TrackNamespace::from_utf8_path("test"), "0.mp4").produce();

        let subscribe = subscriber.subscribe_open(writer);
        futures::pin_mut!(subscribe);

        assert!(matches!(futures::poll!(&mut subscribe), Poll::Pending));
        assert_eq!(observer.subscribes.lock().unwrap().len(), 1);

        let mut receiver = observer.clone();
        receiver
            .recv_subscribe_ok(&message::SubscribeOk {
                id: 0,
                track_alias: 10,
                params: Default::default(),
                track_extensions: Default::default(),
            })
            .unwrap();

        let subscribe = match futures::poll!(&mut subscribe) {
            Poll::Ready(Ok(subscribe)) => subscribe,
            Poll::Ready(Err(err)) => panic!("subscribe failed: {err}"),
            Poll::Pending => panic!("subscribe remained pending after SubscribeOk"),
        };

        assert_eq!(observer.subscribes.lock().unwrap().len(), 1);
        assert!(matches!(
            observer.track_alias_map.lock().unwrap().get(10),
            Some(TrackOrigin::Subscribe(0))
        ));

        drop(subscribe);

        assert!(observer.subscribes.lock().unwrap().is_empty());
        assert!(observer.track_alias_map.lock().unwrap().is_empty());
    }
}
