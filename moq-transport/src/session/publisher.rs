// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    collections::{hash_map, HashMap},
    sync::{atomic, Arc, Mutex},
};

use futures::{stream::FuturesUnordered, StreamExt};

use crate::{
    coding::TrackNamespace,
    message::{self, Message},
    mlog,
    serve::{ServeError, TracksReader},
};

use crate::watch::Queue;

use super::{
    Announce, AnnounceRecv, Session, SessionError, Subscribed, SubscribedRecv, TrackStatusRequested,
};

// TODO remove Clone.
#[derive(Clone)]
pub struct Publisher {
    webtransport: web_transport::Session,

    /// When the announce method is used, a new entry is added to this HashMap to track outbound announcement
    announces: Arc<Mutex<HashMap<TrackNamespace, AnnounceRecv>>>,

    /// When a Subscribe is received and we have a previous announce for the namespace, then a new entry is
    /// added to this HashMap to track the inbound subscription
    subscribeds: Arc<Mutex<HashMap<u64, SubscribedRecv>>>,

    /// When a Subscribe is received and we DO NOT have a previous announce for the namespace, then a new entry is
    /// added to this Queue to track the inbound subscription
    unknown_subscribed: Queue<Subscribed>,

    /// When a TrackStatus is received and we DO NOT have a previous announce for the namespace, then a new entry is
    /// added to this Queue to track the inbound track status request
    unknown_track_status_requested: Queue<TrackStatusRequested>,

    /// The queue we will write any outbound control messages we want to sent, the session run_send task
    /// will process the queue and send the message on the control stream.
    outgoing: Queue<Message>,

    /// When we need a new Request Id for sending a request, we can get it from here.  Note:  The instance
    /// of AtomicU64 is shared with the Subscriber, so the session uses unique request ids for all requests
    /// generated.  Note:  If we initiated the QUIC connection then request id's start at 0 and increment by 2
    /// for each request (even numbers).  If we accepted an inbound QUIC connection then request id's start at 1 and
    /// increment by 2 for each request (odd numbers).
    next_requestid: Arc<atomic::AtomicU64>,

    /// Optional mlog writer for logging transport events
    mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
}

impl Publisher {
    pub(crate) fn new(
        outgoing: Queue<Message>,
        webtransport: web_transport::Session,
        next_requestid: Arc<atomic::AtomicU64>,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Self {
        Self {
            webtransport,
            announces: Default::default(),
            subscribeds: Default::default(),
            unknown_subscribed: Default::default(),
            unknown_track_status_requested: Default::default(),
            outgoing,
            next_requestid,
            mlog,
        }
    }

    pub async fn accept(
        session: web_transport::Session,
        transport: super::Transport,
    ) -> Result<(Session, Publisher), SessionError> {
        let (session, publisher, _) = Session::accept(session, None, transport).await?;
        Ok((session, publisher.unwrap()))
    }

    pub async fn connect(
        session: web_transport::Session,
        transport: super::Transport,
    ) -> Result<(Session, Publisher), SessionError> {
        let (session, publisher, _) = Session::connect(session, None, transport).await?;
        Ok((session, publisher))
    }

    /// Announce a namespace and serve tracks using the provided [serve::TracksReader].
    /// The caller uses [serve::TracksWriter] for static tracks and [serve::TracksRequest] for dynamic tracks.
    pub async fn announce(&mut self, tracks: TracksReader) -> Result<(), SessionError> {
        // Check if annouce for this namespace already exists or not, and if not, then create a new Announce
        let announce = match self
            .announces
            .lock()
            .unwrap()
            .entry(tracks.namespace.clone())
        {
            // Namespace already exists in HashMap (has already been announced) - return Duplicate error
            hash_map::Entry::Occupied(_) => return Err(ServeError::Duplicate.into()),

            // This is a new announce, send announce message to peer.
            hash_map::Entry::Vacant(entry) => {
                // Get the current next request id to use and increment the value for by 2 for the next request
                let request_id = self.next_requestid.fetch_add(2, atomic::Ordering::Relaxed);

                let (send, recv) =
                    Announce::new(self.clone(), request_id, tracks.namespace.clone());
                entry.insert(recv);
                send
            }
        };

        let mut subscribe_tasks = FuturesUnordered::new();
        let mut status_tasks = FuturesUnordered::new();
        let mut subscribe_done = false;
        let mut status_done = false;

        // The code enters an infinite loop and waits for one of several events:
        // - A new subscription arrives.
        // - A new track status request arrives.
        // - One of the spawned subscription-handling tasks completes.
        // - One of the spawned status-handling tasks completes.
        // Exit the loop when all input streams are done (None), and all tasks have completed
        loop {
            tokio::select! {
                // Get next subscription to this announce
                res = announce.subscribed(), if !subscribe_done => {
                    match res? {
                        Some(subscribed) => {
                            let tracks = tracks.clone();

                            subscribe_tasks.push(async move {
                                let info = subscribed.info.clone();
                                if let Err(err) = Self::serve_subscribe(subscribed, tracks).await {
                                    tracing::warn!("failed serving subscribe: {:?}, error: {}", info, err)
                                }
                            });
                        },
                        None => subscribe_done = true,
                    }

                },
                res = announce.track_status_requested(), if !status_done => {
                    match res? {
                        Some(status) => {
                            let tracks = tracks.clone();

                            status_tasks.push(async move {
                                let request_msg = status.request_msg.clone();
                                if let Err(err) = Self::serve_track_status(status, tracks).await {
                                    tracing::warn!("failed serving track status request: {:?}, error: {}", request_msg, err)
                                }
                            });
                        },
                        None => status_done = true,
                    }
                },
                Some(res) = subscribe_tasks.next() => res,
                Some(res) = status_tasks.next() => res,
                else => return Ok(())
            }
        }
    }

    pub async fn serve_subscribe(
        subscribed: Subscribed,
        mut tracks: TracksReader,
    ) -> Result<(), SessionError> {
        if let Some((track, interest)) = tracks.subscribe(
            subscribed.info.track_namespace.clone(),
            &subscribed.info.track_name,
        ) {
            // Hold the interest guard for the whole of `serve`, so the cache entry
            // backing this reader is not evicted (and its upstream subscription
            // released) while we are still forwarding from it.
            let _interest = interest;
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
        // The guard is held only for the duration of the response; a status
        // request is not a subscription, so it should not keep an idle upstream
        // subscription alive beyond the grace period.
        let (track, _interest) = tracks
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

    // Returns subscriptions that do not map to an active announce.
    pub async fn subscribed(&mut self) -> Option<Subscribed> {
        self.unknown_subscribed.pop().await
    }

    // Returns track_status requests that do not map to an active announce.
    pub async fn track_status_requested(&mut self) -> Option<TrackStatusRequested> {
        self.unknown_track_status_requested.pop().await
    }

    pub(crate) fn recv_message(&mut self, msg: message::Subscriber) -> Result<(), SessionError> {
        let res = match msg {
            message::Subscriber::Subscribe(msg) => self.recv_subscribe(msg),
            message::Subscriber::SubscribeUpdate(msg) => self.recv_subscribe_update(msg),
            message::Subscriber::Unsubscribe(msg) => self.recv_unsubscribe(msg),
            message::Subscriber::Fetch(_msg) => Err(SessionError::unimplemented("FETCH")),
            message::Subscriber::FetchCancel(_msg) => {
                Err(SessionError::unimplemented("FETCH_CANCEL"))
            }
            message::Subscriber::TrackStatus(msg) => self.recv_track_status(msg),
            message::Subscriber::SubscribeNamespace(_msg) => {
                Err(SessionError::unimplemented("SUBSCRIBE_NAMESPACE"))
            }
            message::Subscriber::UnsubscribeNamespace(_msg) => {
                Err(SessionError::unimplemented("UNSUBSCRIBE_NAMESPACE"))
            }
            message::Subscriber::PublishNamespaceCancel(msg) => {
                self.recv_publish_namespace_cancel(msg)
            }
            message::Subscriber::PublishNamespaceOk(msg) => self.recv_publish_namespace_ok(msg),
            message::Subscriber::PublishNamespaceError(msg) => {
                self.recv_publish_namespace_error(msg)
            }
            message::Subscriber::PublishOk(_msg) => Err(SessionError::unimplemented("PUBLISH_OK")),
            message::Subscriber::PublishError(_msg) => {
                Err(SessionError::unimplemented("PUBLISH_ERROR"))
            }
        };

        if let Err(err) = res {
            tracing::warn!("failed to process message: {}", err);
        }

        Ok(())
    }

    fn recv_publish_namespace_ok(
        &mut self,
        msg: message::PublishNamespaceOk,
    ) -> Result<(), SessionError> {
        // We need to find the announce request using the request id, however the self.announces data structure
        // is a HashMap indexed by Namespace (which is needed for handling PUBLISH_NAMESPACE_CANCEL).  TODO - make more efficient.
        // For now iterate through all self.annouces until we find the matching id.
        let mut announces = self.announces.lock().unwrap();
        let announce = announces.iter_mut().find(|(_k, v)| v.request_id == msg.id);

        if let Some(announce) = announce {
            announce.1.recv_ok()?;
        }

        Ok(())
    }

    fn recv_publish_namespace_error(
        &mut self,
        msg: message::PublishNamespaceError,
    ) -> Result<(), SessionError> {
        // We need to find the announce request using the request id, however the self.announces data structure
        // is a HashMap indexed by Namespace (which is needed for handling PUBLISH_NAMESPACE_CANCEL).  TODO - make more efficient.
        // For now iterate through all self.annouces until we find the matching id.
        let mut announces = self.announces.lock().unwrap();

        // Find the key first (immutable borrow only)
        let key_opt = announces
            .iter()
            .find(|(_k, v)| v.request_id == msg.id)
            .map(|(k, _)| k.clone());

        // Remove from HashMap and take ownership
        if let Some(key) = key_opt {
            if let Some((_ns, v)) = announces.remove_entry(&key) {
                // Step 3: call recv_error, consuming v
                v.recv_error(ServeError::Closed(msg.error_code))?;
            }
        }

        Ok(())
    }

    fn recv_publish_namespace_cancel(
        &mut self,
        msg: message::PublishNamespaceCancel,
    ) -> Result<(), SessionError> {
        // TODO: If a publisher receives new subscriptions for that namespace after receiving an ANNOUNCE_CANCEL,
        // it SHOULD close the session as a 'Protocol Violation'.
        if let Some(announce) = self.announces.lock().unwrap().remove(&msg.track_namespace) {
            announce.recv_error(ServeError::Cancel)?;
        }

        Ok(())
    }

    fn recv_subscribe(&mut self, msg: message::Subscribe) -> Result<(), SessionError> {
        let namespace = msg.track_namespace.clone();

        let subscribed = {
            let mut subscribeds = self.subscribeds.lock().unwrap();

            // See if entry exists for this request id already, if so error out
            let entry = match subscribeds.entry(msg.id) {
                hash_map::Entry::Occupied(_) => return Err(SessionError::Duplicate),
                hash_map::Entry::Vacant(entry) => entry,
            };

            // Create new Subscribed entry and add to HashMap
            let (send, recv) = Subscribed::new(self.clone(), msg, self.mlog.clone());
            entry.insert(recv);

            send
        };

        // If we have an announce, route the subscribe to it.
        if let Some(announce) = self.announces.lock().unwrap().get_mut(&namespace) {
            return announce.recv_subscribe(subscribed).map_err(Into::into);
        }

        // Otherwise, put it in the unknown queue.
        // TODO Have some way to detect if the application is not reading from the unknown queue,
        // then send SubscribeError.
        if let Err(err) = self.unknown_subscribed.push(subscribed) {
            // Default to closing with a not found error I guess.
            err.close(ServeError::not_found_ctx(format!(
                "unknown_subscribed queue full for namespace {:?}",
                namespace
            )))?;
        }

        Ok(())
    }

    fn recv_subscribe_update(
        &mut self,
        _msg: message::SubscribeUpdate,
    ) -> Result<(), SessionError> {
        // TODO: Implement updating subscriptions.
        Err(SessionError::unimplemented("SUBSCRIBE_UPDATE"))
    }

    fn recv_track_status(&mut self, msg: message::TrackStatus) -> Result<(), SessionError> {
        let namespace = msg.track_namespace.clone();

        // Create TrackStatusRequested to track this request
        let track_status_requested = TrackStatusRequested::new(self.clone(), msg);

        // If we have an announce, route the track_status to it.
        if let Some(announce) = self.announces.lock().unwrap().get_mut(&namespace) {
            return announce
                .recv_track_status_requested(track_status_requested)
                .map_err(Into::into);
        }

        // Otherwise, put it in the unknown_track_status queue.
        // TODO Have some way to detect if the application is not reading from the unknown_track_status queue,
        // then send TrackStatusError.
        if let Err(mut err) = self
            .unknown_track_status_requested
            .push(track_status_requested)
        {
            // push only fails if the queue is dropped, send  TrackStatusError, Internal error
            err.respond_error(0, "Internal error")?;
        }

        Ok(())
    }

    fn recv_unsubscribe(&mut self, msg: message::Unsubscribe) -> Result<(), SessionError> {
        if let Some(subscribed) = self.subscribeds.lock().unwrap().get_mut(&msg.id) {
            subscribed.recv_unsubscribe()?;
        }

        Ok(())
    }

    /// Process a message before sending it, performing any necessary internal actions.
    fn act_on_message_to_send<T: Into<message::Publisher>>(
        &mut self,
        msg: T,
    ) -> message::Publisher {
        let msg = msg.into();
        match &msg {
            message::Publisher::PublishDone(m) => self.drop_subscribe(m.id),
            message::Publisher::SubscribeError(m) => self.drop_subscribe(m.id),
            message::Publisher::PublishNamespaceDone(m) => {
                self.drop_publish_namespace(&m.track_namespace);
            }
            _ => {}
        }
        msg
    }

    /// Send a message without waiting for it to be sent.
    pub(super) fn send_message<T: Into<message::Publisher> + Into<Message>>(&mut self, msg: T) {
        let msg = self.act_on_message_to_send(msg);
        self.outgoing.push(msg.into()).ok();
    }

    /// Send a message and wait until it is sent (or at least popped off the outgoing control message queue)
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

    fn drop_subscribe(&mut self, id: u64) {
        self.subscribeds.lock().unwrap().remove(&id);
    }

    fn drop_publish_namespace(&mut self, namespace: &TrackNamespace) {
        self.announces.lock().unwrap().remove(namespace);
    }

    pub(super) async fn open_uni(&mut self) -> Result<web_transport::SendStream, SessionError> {
        Ok(self.webtransport.open_uni().await?)
    }

    pub(super) async fn send_datagram(&mut self, data: bytes::Bytes) -> Result<(), SessionError> {
        Ok(self.webtransport.send_datagram(data).await?)
    }
}
