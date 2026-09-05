// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    collections::{hash_map, HashMap},
    sync::{Arc, Mutex},
};

use futures::{stream::FuturesUnordered, StreamExt};

use crate::{
    coding::{KeyValuePairs, TrackNamespace, TrackNamespacePrefix},
    message::{self, Message},
    mlog,
    serve::{FullTrackName, ServeError, TrackReader, TracksReader},
};

use crate::watch::Queue;

use super::{
    split_published_state, ObjectForwarderRecv, PendingRequest, PendingRequests, PublishNamespace,
    PublishNamespaceRecv, Published, PublishedInfo, PublishedRecv, RequestId, RequestIdAllocation,
    Session, SessionConfig, SessionError, SessionId, Subscribed, SubscribedNamespace,
    SubscribedNamespaceInfo, SubscribedNamespaceRecv, TrackStatusRequested,
};
use crate::message::RequestErrorCode;

#[derive(Default)]
struct NameRegistry {
    by_name: HashMap<FullTrackName, u64>,
    by_id: HashMap<u64, FullTrackName>,
}

impl NameRegistry {
    fn contains_name(&self, name: &FullTrackName) -> bool {
        self.by_name.contains_key(name)
    }

    fn reserve(&mut self, name: FullTrackName, id: u64) -> bool {
        if self.by_name.contains_key(&name) || self.by_id.contains_key(&id) {
            return false;
        }

        self.by_id.insert(id, name.clone());
        self.by_name.insert(name, id);
        true
    }

    fn insert(&mut self, name: FullTrackName, id: u64) {
        if let Some(old_name) = self.by_id.insert(id, name.clone()) {
            self.by_name.remove(&old_name);
        }
        if let Some(old_id) = self.by_name.insert(name, id) {
            self.by_id.remove(&old_id);
        }
    }

    fn remove_id(&mut self, id: u64) -> Option<FullTrackName> {
        let name = self.by_id.remove(&id)?;
        self.by_name.remove(&name);
        Some(name)
    }
}

struct PublishedEntry {
    recv: PublishedRecv,
    forwarder: Option<ObjectForwarderRecv>,
}

impl PublishedEntry {
    fn new(recv: PublishedRecv) -> Self {
        Self {
            recv,
            forwarder: None,
        }
    }

    fn recv_unsubscribe(&mut self) -> Result<(), ServeError> {
        self.recv.recv_unsubscribe()?;
        if let Some(forwarder) = &mut self.forwarder {
            forwarder.recv_unsubscribe()?;
        }
        Ok(())
    }
}

// TODO remove Clone.
#[derive(Clone)]
pub struct Publisher {
    webtransport: web_transport::Session,

    /// Active outbound PUBLISH_NAMESPACE requests, keyed by namespace.
    publish_namespaces: Arc<Mutex<HashMap<TrackNamespace, PublishNamespaceRecv>>>,

    /// Active outbound PUBLISH requests, keyed by request id.
    publisheds: Arc<Mutex<HashMap<u64, PublishedEntry>>>,

    /// Active outbound PUBLISHes keyed by Full Track Name for §5.1 same-role
    /// duplicate-subscription checks.
    published_names: Arc<Mutex<NameRegistry>>,

    /// Active inbound SUBSCRIBE requests, keyed by request id.
    subscribeds: Arc<Mutex<HashMap<u64, ObjectForwarderRecv>>>,

    /// Active inbound SUBSCRIBEs keyed by Full Track Name.
    subscribed_names: Arc<Mutex<NameRegistry>>,

    /// Subscriptions for namespaces that have no matching PUBLISH_NAMESPACE.
    unknown_subscribed: Queue<Subscribed>,

    /// TRACK_STATUS requests for namespaces that have no matching PUBLISH_NAMESPACE.
    unknown_track_status_requested: Queue<TrackStatusRequested>,

    /// Active inbound SUBSCRIBE_NAMESPACE prefixes, keyed by Request ID.
    subscribed_namespace_prefixes: Arc<Mutex<HashMap<u64, TrackNamespacePrefix>>>,

    /// SUBSCRIBE_NAMESPACE requests surfaced to the application.
    unknown_subscribed_namespace: Queue<SubscribedNamespace>,

    /// Queue for outbound control messages; processed by the session run_send task.
    outgoing: Queue<Message>,

    /// Shared with Subscriber so all requests within a session use unique IDs.
    /// When we need a new Request Id for sending a request, we can get it from here.
    /// The manager is shared with the Subscriber, so the session uses unique request ids
    /// for all requests generated.  If we initiated the QUIC connection then request
    /// IDs start at 0 and increment by 2 (even numbers).  If we accepted an inbound
    /// QUIC connection then request IDs start at 1 and increment by 2 (odd numbers).
    request_id: RequestId,

    /// Tracks outbound publisher-role requests waiting for OK/error responses.
    pending_requests: PendingRequests,

    /// Optional mlog writer for logging transport events
    mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,

    /// Correlation id of the owning session, tagged onto this publisher's log records.
    session_id: SessionId,
}

impl Publisher {
    pub(crate) fn new(
        outgoing: Queue<Message>,
        webtransport: web_transport::Session,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
        request_id: RequestId,
        pending_requests: PendingRequests,
        session_id: SessionId,
    ) -> Self {
        Self {
            webtransport,
            publish_namespaces: Default::default(),
            publisheds: Default::default(),
            published_names: Default::default(),
            subscribeds: Default::default(),
            subscribed_names: Default::default(),
            unknown_subscribed: Default::default(),
            unknown_track_status_requested: Default::default(),
            subscribed_namespace_prefixes: Default::default(),
            unknown_subscribed_namespace: Default::default(),
            outgoing,
            request_id,
            pending_requests,
            mlog,
            session_id,
        }
    }

    /// Correlation id of the session this publisher belongs to.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Accept a publisher session using a generated local correlation ID.
    ///
    /// Use [`Self::accept_with_session_id`] when a peer-observed QUIC connection ID is available.
    pub async fn accept(
        session: web_transport::Session,
        transport: super::Transport,
    ) -> Result<(Session, Publisher), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `accept` accept it and remove `accept_with_session_id`.
        Self::accept_with_session_id(session, SessionId::generate(), transport).await
    }

    /// Accept a publisher session with explicit configuration and a generated correlation ID.
    ///
    /// Use [`Self::accept_with_config_and_session_id`] when a peer-observed QUIC connection ID is
    /// available.
    pub async fn accept_with_config(
        session: web_transport::Session,
        transport: super::Transport,
        config: SessionConfig,
    ) -> Result<(Session, Publisher), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `accept_with_config` accept it and remove `accept_with_config_and_session_id`.
        Self::accept_with_config_and_session_id(session, SessionId::generate(), transport, config)
            .await
    }

    /// Accept a publisher session using a peer-observed QUIC connection ID.
    pub async fn accept_with_session_id(
        session: web_transport::Session,
        session_id: SessionId,
        transport: super::Transport,
    ) -> Result<(Session, Publisher), SessionError> {
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

    /// Accept a configured publisher session using a peer-observed QUIC connection ID.
    pub async fn accept_with_config_and_session_id(
        session: web_transport::Session,
        session_id: SessionId,
        transport: super::Transport,
        config: SessionConfig,
    ) -> Result<(Session, Publisher), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `accept_with_config` accept it and remove `accept_with_config_and_session_id`.
        let (session, publisher, _) = Session::accept_with_config_and_session_id(
            session, session_id, None, transport, config,
        )
        .await?;
        let publisher = publisher.ok_or(SessionError::Internal)?;
        Ok((session, publisher))
    }

    /// Create a publisher session using a generated local correlation ID.
    ///
    /// Use [`Self::connect_with_session_id`] when a peer-observed QUIC connection ID is available.
    pub async fn connect(
        session: web_transport::Session,
        transport: super::Transport,
    ) -> Result<(Session, Publisher), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `connect` accept it and remove `connect_with_session_id`.
        Self::connect_with_session_id(session, SessionId::generate(), transport).await
    }

    /// Create a configured publisher session using a generated local correlation ID.
    ///
    /// Use [`Self::connect_with_config_and_session_id`] when a peer-observed QUIC connection ID is
    /// available.
    pub async fn connect_with_config(
        session: web_transport::Session,
        transport: super::Transport,
        config: SessionConfig,
    ) -> Result<(Session, Publisher), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `connect_with_config` accept it and remove `connect_with_config_and_session_id`.
        Self::connect_with_config_and_session_id(session, SessionId::generate(), transport, config)
            .await
    }

    /// Create a publisher session using a peer-observed QUIC connection ID.
    pub async fn connect_with_session_id(
        session: web_transport::Session,
        session_id: SessionId,
        transport: super::Transport,
    ) -> Result<(Session, Publisher), SessionError> {
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

    /// Create a configured publisher session using a peer-observed QUIC connection ID.
    pub async fn connect_with_config_and_session_id(
        session: web_transport::Session,
        session_id: SessionId,
        transport: super::Transport,
        config: SessionConfig,
    ) -> Result<(Session, Publisher), SessionError> {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `connect_with_config` accept it and remove `connect_with_config_and_session_id`.
        let (session, publisher, _) = Session::connect_with_config_and_session_id(
            session, session_id, None, transport, config,
        )
        .await?;
        Ok((session, publisher))
    }

    /// Send a PUBLISH_NAMESPACE for a namespace and serve tracks using the provided
    /// [serve::TracksReader].  Blocks until the namespace is unannounced or an error occurs.
    pub async fn publish_namespace(&mut self, tracks: TracksReader) -> Result<(), SessionError> {
        let publish_ns = match self
            .publish_namespaces
            .lock()
            .map_err(|_| SessionError::Internal)?
            .entry(tracks.namespace.clone())
        {
            // Duplicate PUBLISH_NAMESPACE for the same namespace is a protocol error.
            hash_map::Entry::Occupied(_) => return Err(ServeError::Duplicate.into()),

            hash_map::Entry::Vacant(entry) => {
                // Allocate a request ID, enforcing the peer-advertised maximum.
                let request_id = match self.request_id.allocate()? {
                    RequestIdAllocation::Allocated(id) => id,
                    blocked @ RequestIdAllocation::Blocked { .. } => {
                        if let Some(msg) = blocked.requests_blocked() {
                            let _ = self.outgoing.push(msg.into());
                        }
                        return Err(SessionError::TooManyRequests);
                    }
                };
                self.pending_requests
                    .insert(request_id, PendingRequest::PublishNamespace)?;
                let (mut send, recv) =
                    PublishNamespace::new(self.clone(), request_id, tracks.namespace.clone());
                entry.insert(recv);
                send.send_request();
                send
            }
        };

        let mut subscribe_tasks = FuturesUnordered::new();
        let mut status_tasks = FuturesUnordered::new();
        let mut subscribe_done = false;
        let mut status_done = false;

        loop {
            tokio::select! {
                res = publish_ns.subscribed(), if !subscribe_done => {
                    match res? {
                        Some(subscribed) => {
                            let tracks = tracks.clone();
                            let session_id = self.session_id.clone();
                            subscribe_tasks.push(async move {
                                let info = subscribed.info.clone();
                                if let Err(err) = Self::serve_subscribe(subscribed, tracks).await {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        subscribe_info = ?info,
                                        error = %err,
                                        "failed serving subscribe"
                                    );
                                }
                            });
                        }
                        None => subscribe_done = true,
                    }
                },
                res = publish_ns.track_status_requested(), if !status_done => {
                    match res? {
                        Some(status) => {
                            let tracks = tracks.clone();
                            let session_id = self.session_id.clone();
                            status_tasks.push(async move {
                                let request_msg = status.request_msg.clone();
                                if let Err(err) = Self::serve_track_status(status, tracks).await {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        request = ?request_msg,
                                        error = %err,
                                        "failed serving track status request"
                                    );
                                }
                            });
                        }
                        None => status_done = true,
                    }
                },
                Some(res) = subscribe_tasks.next() => res,
                Some(res) = status_tasks.next() => res,
                else => return Ok(()),
            }
        }
    }

    /// Send PUBLISH for a specific track and return a handle that can serve it.
    ///
    /// The publisher chooses the Track Alias.  For the first cut, we use
    /// `track_alias = request_id` because request ids are already unique in the
    /// session and draft-16 §10.1 only requires uniqueness, not a separate
    /// allocator.
    pub async fn publish(
        &mut self,
        track: TrackReader,
        mut params: KeyValuePairs,
    ) -> Result<Published, SessionError> {
        let full_name = FullTrackName {
            namespace: track.namespace.clone(),
            name: track.name.clone(),
        };

        if let Some(largest) = track.largest_location() {
            params
                .set_largest_object(largest)
                .map_err(|_| SessionError::Internal)?;
        }

        let forward = params
            .forward()
            .map_err(SessionError::Decode)?
            .unwrap_or(true);
        let largest_location = params.largest_object().map_err(SessionError::Decode)?;

        // §5.1: this endpoint can have at most one publisher-role subscription
        // for a track. Inbound SUBSCRIBE and outbound PUBLISH are both
        // publisher-role subscriptions from this endpoint's perspective. Keep
        // the duplicate check and published-name reservation atomic.
        let request_id = {
            let subscribed_names = self
                .subscribed_names
                .lock()
                .map_err(|_| SessionError::Internal)?;
            let mut published_names = self
                .published_names
                .lock()
                .map_err(|_| SessionError::Internal)?;
            if subscribed_names.contains_name(&full_name)
                || published_names.contains_name(&full_name)
            {
                return Err(SessionError::Serve(ServeError::Duplicate));
            }

            let request_id = match self.request_id.allocate()? {
                RequestIdAllocation::Allocated(id) => id,
                blocked @ RequestIdAllocation::Blocked { .. } => {
                    if let Some(msg) = blocked.requests_blocked() {
                        let _ = self.outgoing.push(msg.into());
                    }
                    return Err(SessionError::TooManyRequests);
                }
            };

            if !published_names.reserve(full_name.clone(), request_id) {
                return Err(SessionError::Duplicate);
            }
            request_id
        };

        let track_alias = request_id;

        let info = PublishedInfo {
            id: request_id,
            track_namespace: track.namespace.clone(),
            track_name: track.name.clone(),
            track_alias,
            forward,
            largest_location,
        };

        let (published_state, recv_state) = split_published_state(forward);

        // Register state before queuing PUBLISH so fast PUBLISH_OK / REQUEST_ERROR
        // can be routed.
        match self.publisheds.lock() {
            Ok(mut publisheds) => {
                publisheds.insert(
                    request_id,
                    PublishedEntry::new(PublishedRecv::new(recv_state)),
                );
            }
            Err(_) => {
                if let Ok(mut published_names) = self.published_names.lock() {
                    published_names.remove_id(request_id);
                }
                return Err(SessionError::Internal);
            }
        }
        if let Err(err) = self
            .pending_requests
            .insert(request_id, PendingRequest::Publish)
        {
            let _ = self.remove_published(request_id)?;
            return Err(err);
        }

        let msg = message::Publish {
            id: request_id,
            track_namespace: info.track_namespace.clone(),
            track_name: info.track_name.clone(),
            track_alias,
            params,
            track_extensions: Default::default(),
        };
        let published = Published::new(self.clone(), info, published_state, track);
        self.send_message(msg);

        Ok(published)
    }

    pub async fn serve_subscribe(
        subscribed: Subscribed,
        mut tracks: TracksReader,
    ) -> Result<(), SessionError> {
        if let Some(track) = tracks.subscribe(
            subscribed.info.track_namespace.clone(),
            &subscribed.info.track_name,
        ) {
            subscribed.serve(track).await?;
        } else {
            let namespace = subscribed.info.track_namespace.clone();
            let name = subscribed.info.track_name.clone();
            subscribed.close(ServeError::not_found_ctx(format!(
                "track '{}/{}' not found in tracks",
                namespace, name
            )))?;
        }

        Ok(())
    }

    pub async fn serve_track_status(
        track_status_request: TrackStatusRequested,
        mut tracks: TracksReader,
    ) -> Result<(), SessionError> {
        let track = tracks
            .subscribe(
                track_status_request.request_msg.track_namespace.clone(),
                &track_status_request.request_msg.track_name,
            )
            .ok_or_else(|| {
                ServeError::not_found_ctx(format!(
                    "track '{}/{}' not found for track_status",
                    track_status_request.request_msg.track_namespace,
                    track_status_request.request_msg.track_name
                ))
            })?;

        track_status_request.respond_ok(&track)?;

        Ok(())
    }

    /// Returns the next subscription that did not match any active PUBLISH_NAMESPACE.
    pub async fn subscribed(&mut self) -> Option<Subscribed> {
        self.unknown_subscribed.pop().await
    }

    /// Returns the next TRACK_STATUS request that did not match any active PUBLISH_NAMESPACE.
    pub async fn track_status_requested(&mut self) -> Option<TrackStatusRequested> {
        self.unknown_track_status_requested.pop().await
    }

    pub async fn subscribed_namespace(&mut self) -> Option<SubscribedNamespace> {
        self.unknown_subscribed_namespace.pop().await
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

    pub(crate) fn recv_message(&mut self, msg: message::Subscriber) -> Result<(), SessionError> {
        match msg {
            message::Subscriber::Subscribe(msg) => self.recv_subscribe(msg)?,
            // REQUEST_UPDATE is intentionally rejected for now. Dynamic Forward
            // updates require making the data-plane delivery filter mutable;
            // until then this is explicit and simple.
            message::Subscriber::RequestUpdate(msg) => self.recv_request_update(msg)?,
            // Draft-16: REQUEST_OK is used for PUBLISH_NAMESPACE acceptance and
            // REQUEST_UPDATE responses. PUBLISH acceptance uses PUBLISH_OK.
            message::Subscriber::RequestOk(msg) => self.recv_request_ok(msg)?,
            // Draft-16: REQUEST_ERROR rejects PUBLISH_NAMESPACE or PUBLISH.
            message::Subscriber::RequestError(msg) => self.recv_request_error(msg)?,
            message::Subscriber::Unsubscribe(msg) => self.recv_unsubscribe(msg)?,
            // FETCH not yet implemented — send REQUEST_ERROR NOT_SUPPORTED (§4).
            message::Subscriber::Fetch(msg) => {
                self.send_not_supported(msg.id, "fetch");
            }
            // FETCH_CANCEL references an existing request; log and ignore.
            message::Subscriber::FetchCancel(msg) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    session_id = %self.session_id,
                    request_id = msg.id,
                    "received FETCH_CANCEL for unsupported FETCH — ignoring"
                );
            }
            message::Subscriber::TrackStatus(msg) => self.recv_track_status(msg)?,
            // Draft-16 §3.3/§9.25: SUBSCRIBE_NAMESPACE begins a dedicated bidi stream.
            message::Subscriber::SubscribeNamespace(msg) => {
                return Err(SessionError::ProtocolViolation(format!(
                    "SUBSCRIBE_NAMESPACE {} received on control stream",
                    msg.id
                )));
            }
            message::Subscriber::PublishNamespaceCancel(msg) => {
                self.recv_publish_namespace_cancel(msg)?;
            }
            message::Subscriber::PublishOk(msg) => self.recv_publish_ok(msg)?,
        }

        Ok(())
    }

    /// Send REQUEST_ERROR NOT_SUPPORTED for an incoming request we do not implement.
    ///
    /// Draft-16 §4: limited endpoints SHOULD respond with NOT_SUPPORTED rather
    /// than ignoring unsupported request types.
    fn send_not_supported(&mut self, request_id: u64, request_kind: &str) {
        tracing::debug!(
            target: "moq_transport::control",
            session_id = %self.session_id,
            request_id,
            "sending REQUEST_ERROR NOT_SUPPORTED for unimplemented request"
        );
        self.send_request_error(
            request_kind,
            message::RequestError {
                id: request_id,
                error_code: RequestErrorCode::NotSupported as u64,
                retry_interval: 0,
                reason: crate::coding::ReasonPhrase("not supported".to_string()),
            },
        );
    }

    /// Handle REQUEST_OK from subscriber (draft-16 §9.7).
    pub(super) fn recv_request_ok(&mut self, msg: message::RequestOk) -> Result<(), SessionError> {
        self.log_request_ok_parsed("request_ok", &msg);
        // The publish_namespaces map is keyed by namespace; we must search by request_id.
        // TODO(itzmanish): maintain a second index keyed by request_id to make this O(1).
        let mut namespaces = self
            .publish_namespaces
            .lock()
            .map_err(|_| SessionError::Internal)?;
        if let Some(entry) = namespaces.iter_mut().find(|(_k, v)| v.request_id == msg.id) {
            entry.1.recv_ok()?;
        }

        Ok(())
    }

    /// Handle PUBLISH_OK from subscriber — acceptance of our PUBLISH (draft-16 §9.14).
    pub(super) fn recv_publish_ok(&mut self, msg: message::PublishOk) -> Result<(), SessionError> {
        if let Some(published) = self
            .publisheds
            .lock()
            .map_err(|_| SessionError::Internal)?
            .get_mut(&msg.id)
        {
            published.recv.recv_ok(&msg)?;
        } else {
            tracing::debug!(
                target: "moq_transport::control",
                session_id = %self.session_id,
                request_id = msg.id,
                "received PUBLISH_OK for unknown PUBLISH — ignoring"
            );
        }

        Ok(())
    }

    /// Handle REQUEST_ERROR from subscriber (draft-16 §9.8).
    pub(super) fn recv_request_error(
        &mut self,
        msg: message::RequestError,
    ) -> Result<(), SessionError> {
        self.log_request_error_parsed("request_error", &msg);
        if let Some(recv) = self.drop_publish_namespace(msg.id) {
            recv.recv_error(ServeError::Closed(msg.error_code))?;
            return Ok(());
        }

        let published = self.remove_published(msg.id)?;
        if let Some(mut published) = published {
            published
                .recv
                .recv_error(ServeError::Closed(msg.error_code))?;
        }

        Ok(())
    }

    pub(super) fn recv_request_timeout(
        &mut self,
        id: u64,
        request: PendingRequest,
    ) -> Result<(), SessionError> {
        let err = ServeError::internal_ctx(format!("{:?} response timed out", request));
        match request {
            PendingRequest::PublishNamespace => {
                if let Some(recv) = self.drop_publish_namespace(id) {
                    recv.recv_error(err)?;
                }
            }
            PendingRequest::Publish => {
                if let Some(mut published) = self.remove_published(id)? {
                    published.recv.recv_error(err)?;
                }
            }
            PendingRequest::Subscribe => return Err(SessionError::Internal),
        }

        Ok(())
    }

    /// Handle REQUEST_UPDATE from subscriber (draft-16 §9.11).
    ///
    /// First-cut behavior: reject all updates with NOT_SUPPORTED. This avoids
    /// pretending dynamic Forward updates are wired into the data-plane serve
    /// loop.
    fn recv_request_update(&mut self, msg: message::RequestUpdate) -> Result<(), SessionError> {
        self.send_request_error(
            "request_update",
            message::RequestError {
                id: msg.id,
                error_code: RequestErrorCode::NotSupported as u64,
                retry_interval: 0,
                reason: crate::coding::ReasonPhrase("not supported".to_string()),
            },
        );
        Ok(())
    }

    fn recv_publish_namespace_cancel(
        &mut self,
        msg: message::PublishNamespaceCancel,
    ) -> Result<(), SessionError> {
        // Draft-16 §9.24: PUBLISH_NAMESPACE_CANCEL now carries Request ID.
        if let Some(recv) = self.drop_publish_namespace(msg.id) {
            recv.recv_error(ServeError::Cancel)?;
        }
        Ok(())
    }

    fn recv_subscribe(&mut self, msg: message::Subscribe) -> Result<(), SessionError> {
        let namespace = msg.track_namespace.clone();
        let full_name = FullTrackName {
            namespace: msg.track_namespace.clone(),
            name: msg.track_name.clone(),
        };

        let subscribed = {
            let mut subscribeds = self
                .subscribeds
                .lock()
                .map_err(|_| SessionError::Internal)?;

            if subscribeds.contains_key(&msg.id) {
                let id = msg.id;
                drop(subscribeds);
                // Draft-16 §5.1: duplicate SUBSCRIBE for the same request ID
                // MUST be rejected with DUPLICATE_SUBSCRIPTION, not a session close.
                self.send_request_error(
                    "subscribe",
                    message::RequestError {
                        id,
                        error_code: RequestErrorCode::DuplicateSubscription as u64,
                        retry_interval: 0,
                        reason: crate::coding::ReasonPhrase("duplicate subscription".to_string()),
                    },
                );
                return Ok(());
            }

            let mut subscribed_names = self
                .subscribed_names
                .lock()
                .map_err(|_| SessionError::Internal)?;
            let already_published = self
                .published_names
                .lock()
                .map_err(|_| SessionError::Internal)?
                .contains_name(&full_name);

            if subscribed_names.contains_name(&full_name) || already_published {
                let id = msg.id;
                drop(subscribed_names);
                drop(subscribeds);
                self.send_request_error(
                    "subscribe",
                    message::RequestError {
                        id,
                        error_code: RequestErrorCode::DuplicateSubscription as u64,
                        retry_interval: 0,
                        reason: crate::coding::ReasonPhrase("duplicate subscription".to_string()),
                    },
                );
                return Ok(());
            }

            let (send, recv) = Subscribed::new(self.clone(), msg, self.mlog.clone())?;
            subscribed_names.insert(full_name, send.info.id);
            subscribeds.insert(send.info.id, recv);

            send
        };

        // Route to an active PUBLISH_NAMESPACE if present.
        if let Some(ns) = self
            .publish_namespaces
            .lock()
            .map_err(|_| SessionError::Internal)?
            .get_mut(&namespace)
        {
            return ns.recv_subscribe(subscribed).map_err(Into::into);
        }

        // Otherwise, surface it to the application via the unknown queue.
        if let Err(err) = self.unknown_subscribed.push(subscribed) {
            err.close(ServeError::not_found_ctx(format!(
                "unknown_subscribed queue full for namespace {:?}",
                namespace
            )))?;
        }

        Ok(())
    }

    fn recv_track_status(&mut self, msg: message::TrackStatus) -> Result<(), SessionError> {
        let namespace = msg.track_namespace.clone();

        let track_status_requested = TrackStatusRequested::new(self.clone(), msg);

        if let Some(ns) = self
            .publish_namespaces
            .lock()
            .map_err(|_| SessionError::Internal)?
            .get_mut(&namespace)
        {
            return ns
                .recv_track_status_requested(track_status_requested)
                .map_err(Into::into);
        }

        if let Err(mut err) = self
            .unknown_track_status_requested
            .push(track_status_requested)
        {
            err.respond_error(RequestErrorCode::InternalError as u64, "internal error")?;
        }

        Ok(())
    }

    pub(super) fn recv_subscribe_namespace(
        &mut self,
        msg: message::SubscribeNamespace,
    ) -> Result<SubscribedNamespaceRecv, SessionError> {
        let forward = msg.params.forward()?.unwrap_or(true);

        {
            let mut prefixes = self
                .subscribed_namespace_prefixes
                .lock()
                .map_err(|_| SessionError::Internal)?;
            if Self::has_subscribed_namespace_overlap(&prefixes, &msg.track_namespace_prefix) {
                return Ok(SubscribedNamespaceRecv::rejected(
                    msg.id,
                    RequestErrorCode::PrefixOverlap as u64,
                    "prefix overlap",
                ));
            }
            prefixes.insert(msg.id, msg.track_namespace_prefix.clone());
        }

        let info = SubscribedNamespaceInfo {
            request_id: msg.id,
            namespace_prefix: msg.track_namespace_prefix,
            subscribe_options: msg.subscribe_options,
            forward,
        };
        let (mut send, recv) =
            SubscribedNamespace::new(info, self.subscribed_namespace_prefixes.clone());

        if let Err(send_back) = self.unknown_subscribed_namespace.push(send) {
            send = send_back;
            send.reject(RequestErrorCode::InternalError as u64, "internal error")?;
        }

        Ok(recv)
    }

    fn has_subscribed_namespace_overlap(
        prefixes: &HashMap<u64, TrackNamespacePrefix>,
        prefix: &TrackNamespacePrefix,
    ) -> bool {
        prefixes.values().any(|existing| existing.overlaps(prefix))
    }

    fn recv_unsubscribe(&mut self, msg: message::Unsubscribe) -> Result<(), SessionError> {
        if let Some(mut subscribed) = self.remove_subscribe(msg.id)? {
            subscribed.recv_unsubscribe()?;
            return Ok(());
        }

        if let Some(mut published) = self.remove_published(msg.id)? {
            published.recv_unsubscribe()?;
        }

        Ok(())
    }

    /// Pre-send hook: clean up internal state when terminal publisher messages are enqueued.
    fn act_on_message_to_send<T: Into<message::Publisher>>(
        &mut self,
        msg: T,
    ) -> message::Publisher {
        let msg = msg.into();
        match &msg {
            message::Publisher::PublishDone(m) => {
                // PUBLISH_DONE terminates either SUBSCRIBE-initiated serving or
                // outbound PUBLISH serving. Clean up both maps; only one will
                // contain the id.
                self.drop_subscribe(m.id);
                self.drop_published(m.id);
            }
            // Draft-16: PUBLISH_NAMESPACE_DONE carries Request ID, not namespace.
            // Dropping the recv state signals that the namespace is done.
            message::Publisher::PublishNamespaceDone(m) => {
                let _ = self.drop_publish_namespace(m.id);
            }
            _ => {}
        }
        msg
    }

    /// Enqueue a control message for sending (fire-and-forget).
    pub(super) fn send_message<T: Into<message::Publisher> + Into<Message>>(&mut self, msg: T) {
        let msg = self.act_on_message_to_send(msg);
        self.outgoing.push(msg.into()).ok();
    }

    /// Enqueue a control message without panicking on poisoned queue state.
    pub(super) fn try_send_message<T: Into<message::Publisher> + Into<Message>>(
        &mut self,
        msg: T,
    ) -> Result<(), ()> {
        let msg = self.act_on_message_to_send(msg);
        self.outgoing.try_push(msg.into()).map_err(|_| ())
    }

    /// Enqueue a control message and wait until it has been dequeued for sending.
    pub(super) async fn send_message_and_wait<T: Into<message::Publisher> + Into<Message>>(
        &mut self,
        msg: T,
    ) {
        let msg = self.act_on_message_to_send(msg);
        self.outgoing
            .push_and_wait_until_popped(msg.into())
            .await
            .ok();
    }

    pub(super) fn drop_subscribe(&mut self, id: u64) {
        if let Err(err) = self.remove_subscribe(id) {
            tracing::error!(session_id = %self.session_id, request_id = id, error = %err, "failed to drop subscribe state");
        }
    }

    pub(super) fn register_published_subscription(
        &mut self,
        id: u64,
        recv: ObjectForwarderRecv,
    ) -> Result<(), SessionError> {
        let mut publisheds = self.publisheds.lock().map_err(|_| SessionError::Internal)?;
        let published = publisheds.get_mut(&id).ok_or(SessionError::Internal)?;
        if published.forwarder.is_some() {
            return Err(SessionError::Duplicate);
        }
        published.forwarder = Some(recv);
        Ok(())
    }

    fn remove_subscribe(&mut self, id: u64) -> Result<Option<ObjectForwarderRecv>, SessionError> {
        let mut subscribeds = self
            .subscribeds
            .lock()
            .map_err(|_| SessionError::Internal)?;
        let mut subscribed_names = self
            .subscribed_names
            .lock()
            .map_err(|_| SessionError::Internal)?;

        let subscribed = subscribeds.remove(&id);
        subscribed_names.remove_id(id);

        Ok(subscribed)
    }

    #[cfg(test)]
    fn drop_subscribed_name(
        subscribed_names: &Arc<Mutex<NameRegistry>>,
        id: u64,
    ) -> Result<(), SessionError> {
        subscribed_names
            .lock()
            .map_err(|_| SessionError::Internal)?
            .remove_id(id);

        Ok(())
    }

    fn drop_publish_namespace(&mut self, id: u64) -> Option<PublishNamespaceRecv> {
        let _ = self.pending_requests.remove(id);
        if let Ok(mut ns) = self.publish_namespaces.lock() {
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

    fn drop_published(&mut self, id: u64) {
        if let Err(err) = self.remove_published(id) {
            tracing::error!(session_id = %self.session_id, request_id = id, error = %err, "failed to drop published state");
        }
    }

    fn remove_published(&mut self, id: u64) -> Result<Option<PublishedEntry>, SessionError> {
        let _ = self.pending_requests.remove(id);
        let mut published_names = self
            .published_names
            .lock()
            .map_err(|_| SessionError::Internal)?;
        let mut publisheds = self.publisheds.lock().map_err(|_| SessionError::Internal)?;

        let published = publisheds.remove(&id);
        if published.is_some() {
            published_names.remove_id(id);
        }

        Ok(published)
    }

    pub(super) async fn open_uni(&mut self) -> Result<web_transport::SendStream, SessionError> {
        Ok(self.webtransport.open_uni().await?)
    }

    pub(super) async fn send_datagram(&mut self, data: bytes::Bytes) -> Result<(), SessionError> {
        Ok(self.webtransport.send_datagram(data).await?)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use crate::{
        coding::{TrackName, TrackNamespace, TrackNamespacePrefix},
        serve::FullTrackName,
    };

    use super::{NameRegistry, Publisher};

    fn full_track_name(namespace: &str, name: &str) -> FullTrackName {
        FullTrackName {
            namespace: TrackNamespace::from_utf8_path(namespace),
            name: TrackName::from(name),
        }
    }

    #[test]
    fn drop_subscribed_name_removes_only_matching_request_id() {
        let subscribed_names = Arc::new(Mutex::new(NameRegistry::default()));
        let unsubscribed_track = full_track_name("bb1", "video.m4s");
        let active_track = full_track_name("bb1", "audio.m4s");

        {
            let mut names = subscribed_names.lock().unwrap();
            names.insert(unsubscribed_track.clone(), 6);
            names.insert(active_track.clone(), 8);
        }

        Publisher::drop_subscribed_name(&subscribed_names, 6).unwrap();

        let names = subscribed_names.lock().unwrap();
        assert!(!names.contains_name(&unsubscribed_track));
        assert_eq!(names.by_name.get(&active_track), Some(&8));
    }

    #[test]
    fn name_registry_reserve_rejects_duplicate_without_overwrite() {
        let mut names = NameRegistry::default();
        let track = full_track_name("bb1", "video.m4s");

        assert!(names.reserve(track.clone(), 6));
        assert!(!names.reserve(track.clone(), 8));

        assert_eq!(names.by_name.get(&track), Some(&6));
        assert_eq!(names.by_id.get(&6), Some(&track));
        assert!(!names.by_id.contains_key(&8));
    }

    #[test]
    fn subscribed_namespace_overlap_detects_common_prefix() {
        let mut prefixes = HashMap::new();
        prefixes.insert(
            0,
            TrackNamespacePrefix::from_utf8_path("example.com/meeting=123"),
        );

        assert!(Publisher::has_subscribed_namespace_overlap(
            &prefixes,
            &TrackNamespacePrefix::from_utf8_path("example.com/meeting=123/participant=100")
        ));
        assert!(Publisher::has_subscribed_namespace_overlap(
            &prefixes,
            &TrackNamespacePrefix::from_utf8_path("example.com")
        ));
        assert!(!Publisher::has_subscribed_namespace_overlap(
            &prefixes,
            &TrackNamespacePrefix::from_utf8_path("example.com/meeting=456")
        ));
    }
}
