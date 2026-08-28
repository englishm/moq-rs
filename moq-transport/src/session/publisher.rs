// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    collections::HashMap,
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
    split_published_state, BidiResponse, BidiResponseMap, DeliveryError, NameRegistry,
    ObjectForwarderRecv, PublishNamespace, PublishNamespaceRecv, Published, PublishedInfo,
    PublishedRecv, RequestId, Session, SessionError, Subscribed, SubscribedNamespace,
    SubscribedNamespaceInfo, SubscribedNamespaceRecv, TrackStatusRequested,
};
use crate::message::RequestErrorCode;

/// Publisher-side state for one outbound PUBLISH.
///
/// The forwarder half only exists once serving starts, so UNSUBSCRIBE has to
/// cancel both the request state and (if present) the data-plane loop.
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

struct AbortPublishedOnDrop {
    publisher: Publisher,
    request_id: Option<u64>,
}

impl AbortPublishedOnDrop {
    fn complete(&mut self) {
        if let Some(request_id) = self.request_id.take() {
            self.publisher.drop_published(request_id);
        }
    }

    fn cancel_from_peer(&mut self) {
        if let Some(request_id) = self.request_id.take() {
            self.publisher.cancel_published(request_id);
        }
    }
}

impl Drop for AbortPublishedOnDrop {
    fn drop(&mut self) {
        if let Some(request_id) = self.request_id.take() {
            self.publisher.abort_published(request_id);
        }
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

    /// Active outbound PUBLISHes keyed by Full Track Name, for the §5.1
    /// same-role duplicate-subscription check.
    published_names: Arc<Mutex<NameRegistry>>,

    /// When a Subscribe is received and we have a matching publish_namespace entry, the
    /// subscription is routed to that PublishNamespaceRecv.  Otherwise it goes here.
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

    /// Optional mlog writer for logging transport events
    mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,

    /// Channel for sending bidi reader futures to `Session::run`, which polls
    /// them cooperatively under structured concurrency (no task is spawned).
    bidi_task_tx: super::BidiTaskSender,

    /// Inbound request streams owned by `Session::run`. Normal serving waits
    /// for the owning writer to encode its terminal message and send FIN.
    pub(super) bidi_response_map: BidiResponseMap,
}

struct PublishNamespaceCleanup {
    publisher: Publisher,
    request_id: u64,
}

enum PublishNamespaceEvent {
    Cancel(Result<u32, tokio::sync::oneshot::error::RecvError>),
    Response(Result<Message, SessionError>),
    Stopped(Result<Option<u8>, SessionError>),
}

impl Drop for PublishNamespaceCleanup {
    fn drop(&mut self) {
        self.publisher.abort_publish_namespace(self.request_id);
    }
}

impl Publisher {
    pub(crate) fn new(
        outgoing: Queue<Message>,
        webtransport: web_transport::Session,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
        request_id: RequestId,
        bidi_task_tx: super::BidiTaskSender,
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
            mlog,
            bidi_task_tx,
            bidi_response_map: Default::default(),
        }
    }

    pub async fn accept(
        session: web_transport::Session,
        transport: super::Transport,
    ) -> Result<(Session, Publisher), SessionError> {
        let (session, publisher, _) = Session::accept(session, None, transport).await?;
        Ok((session, publisher.ok_or(SessionError::Internal)?))
    }

    pub async fn connect(
        session: web_transport::Session,
        transport: super::Transport,
    ) -> Result<(Session, Publisher), SessionError> {
        let (session, publisher, _) = Session::connect(session, None, transport).await?;
        Ok((session, publisher))
    }

    /// Send a PUBLISH_NAMESPACE for a namespace and serve tracks using the provided
    /// [serve::TracksReader].  Blocks until the namespace is unannounced or an error occurs.
    ///
    /// Draft-18: sends PUBLISH_NAMESPACE on a new bidi request stream and reads
    /// responses from the same stream. Requires `Session::run` to be driven
    /// concurrently, since `run` polls the bidi response reader.
    pub async fn publish_namespace(&mut self, tracks: TracksReader) -> Result<(), SessionError> {
        // Phase 1: allocate under lock, release before any await.
        let (publish_ns, wire_msg, request_id, cancel) = {
            let mut namespaces = self
                .publish_namespaces
                .lock()
                .map_err(|_| SessionError::Internal)?;

            if namespaces.contains_key(&tracks.namespace) {
                return Err(ServeError::Duplicate.into());
            }

            let request_id = self.request_id.allocate()?;
            let (send, recv, cancel) =
                PublishNamespace::new(self.clone(), request_id, tracks.namespace.clone());
            namespaces.insert(tracks.namespace.clone(), recv);
            let wire_msg = send.wire_message();
            (send, wire_msg, request_id, cancel)
        };
        // Lock released here.

        self.open_publish_namespace_stream(request_id, wire_msg, cancel)
            .await?;

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
                            subscribe_tasks.push(async move {
                                let info = subscribed.info.clone();
                                if let Err(err) = Self::serve_subscribe(subscribed, tracks).await {
                                    tracing::warn!(
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
                            status_tasks.push(async move {
                                let request_msg = status.request_msg.clone();
                                if let Err(err) = Self::serve_track_status(status, tracks).await {
                                    tracing::warn!(
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

    /// Open a PUBLISH_NAMESPACE request stream and cancel both halves when the
    /// application drops the publish.
    async fn open_publish_namespace_stream(
        &mut self,
        request_id: u64,
        msg: message::PublishNamespace,
        mut cancel: tokio::sync::oneshot::Receiver<u32>,
    ) -> Result<(), SessionError> {
        let frame = super::encode_request_frame(&Message::PublishNamespace(msg))?;
        let (send_stream, recv_stream) = self.webtransport.open_bi().await?;
        let mut writer = super::Writer::new(send_stream);
        writer.write(&frame).await?;

        let mut this = self.clone();
        let cleanup = PublishNamespaceCleanup {
            publisher: this.clone(),
            request_id,
        };

        if self
            .bidi_task_tx
            .send(Box::pin(async move {
                let _cleanup = cleanup;
                let mut reader = super::Reader::new(recv_stream);
                let mut reading = true;
                let mut responded = false;
                let mut request_stopped = false;

                loop {
                    let event = tokio::select! {
                        reset = &mut cancel => PublishNamespaceEvent::Cancel(reset),
                        response = Session::decode_bidi_response(&mut reader, request_id), if reading => {
                            PublishNamespaceEvent::Response(response)
                        }
                        stopped = writer.closed(), if !request_stopped => {
                            PublishNamespaceEvent::Stopped(stopped)
                        }
                    };

                    match event {
                        PublishNamespaceEvent::Cancel(reset) => {
                            if let Ok(code) = reset {
                                writer.reset(code);
                                reader.stop(code);
                            }
                            return Ok(());
                        }
                        PublishNamespaceEvent::Stopped(stopped) => {
                            let _ = stopped?;
                            if responded {
                                reader.stop(super::CANCELLED_STREAM_CODE);
                                return Ok(());
                            }
                            request_stopped = true;
                        }
                        PublishNamespaceEvent::Response(Ok(Message::RequestOk(msg))) => {
                            if responded {
                                return Err(SessionError::ProtocolViolation(
                                    "multiple responses to PUBLISH_NAMESPACE".to_string(),
                                ));
                            }
                            responded = true;
                            this.recv_publish_namespace_ok(&msg)?;
                            if request_stopped {
                                reader.stop(super::CANCELLED_STREAM_CODE);
                                return Ok(());
                            }
                        }
                        PublishNamespaceEvent::Response(Ok(Message::RequestError(msg))) => {
                            if responded {
                                return Err(SessionError::ProtocolViolation(
                                    "multiple responses to PUBLISH_NAMESPACE".to_string(),
                                ));
                            }
                            this.recv_request_error(msg)?;
                            let _ = writer.finish();
                            return Ok(());
                        }
                        PublishNamespaceEvent::Response(Ok(msg)) => {
                            return Err(SessionError::ProtocolViolation(format!(
                                "unexpected {} on PUBLISH_NAMESPACE request stream",
                                msg.name()
                            )));
                        }
                        PublishNamespaceEvent::Response(Err(err)) if err.is_stream_error() => {
                            writer.reset(super::CANCELLED_STREAM_CODE);
                            return Ok(());
                        }
                        PublishNamespaceEvent::Response(Err(err)) if err.is_stream_ended() => {
                            if !responded {
                                return Ok(());
                            }
                            tracing::debug!(
                                request_id,
                                "PUBLISH_NAMESPACE response stream finished after acceptance"
                            );
                            reading = false;
                        }
                        PublishNamespaceEvent::Response(Err(err)) => {
                            return Err(err);
                        }
                    }
                }
            }))
            .is_err()
        {
            return Err(SessionError::Internal);
        }

        Ok(())
    }

    /// Offer a single track to the peer with PUBLISH (draft-18 §10.13).
    ///
    /// The request gets its own bidi stream: PUBLISH goes out on it, PUBLISH_OK
    /// / REQUEST_ERROR / UNSUBSCRIBE come back on it, and PUBLISH_DONE is
    /// written on it when serving completes. `Session::run` must
    /// be driven concurrently, since it polls the stream's reader.
    ///
    /// `Published::ok()` is unbounded, matching `Subscriber::subscribe_open`.
    /// A peer that accepts the stream and never answers is caught only by the
    /// QUIC idle timeout, so a caller that needs a tighter bound must impose
    /// its own.
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
        // per track. Inbound SUBSCRIBE and outbound PUBLISH both make us the
        // publisher, so the duplicate check spans both maps and stays atomic
        // with the name reservation.
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

            let request_id = self.request_id.allocate()?;
            published_names.insert(full_name, request_id);
            request_id
        };

        // The request ID doubles as the Track Alias, matching SUBSCRIBE serving.
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

        // Register response state before the PUBLISH goes out so a fast
        // PUBLISH_OK / REQUEST_ERROR has somewhere to land.
        match self.publisheds.lock() {
            Ok(mut publisheds) => {
                publisheds.insert(
                    request_id,
                    PublishedEntry::new(PublishedRecv::new(recv_state)),
                );
            }
            Err(_) => {
                self.drop_published_name(request_id);
                return Err(SessionError::Internal);
            }
        }

        let msg = message::Publish {
            id: request_id,
            track_namespace: info.track_namespace.clone(),
            track_name: info.track_name.clone(),
            track_alias,
            params,
            track_extensions: Default::default(),
        };
        let abort = AbortPublishedOnDrop {
            publisher: self.clone(),
            request_id: Some(request_id),
        };
        let stream = self.open_publish_stream(request_id, msg, abort).await?;

        Ok(Published::new(
            self.clone(),
            stream,
            info,
            published_state,
            track,
        ))
    }

    /// Open the PUBLISH request stream and hand its reader/writer pump to
    /// `Session::run`.
    ///
    /// Returns the sink used to write follow-up messages (PUBLISH_DONE) on the
    /// same stream. The pump is polled by `run` under structured concurrency,
    /// so it cannot outlive the session.
    async fn open_publish_stream(
        &mut self,
        request_id: u64,
        msg: message::Publish,
        abort: AbortPublishedOnDrop,
    ) -> Result<super::RequestStreamSink, SessionError> {
        // Validate local parameters before consuming a peer request-stream slot.
        let frame = super::encode_request_frame(&Message::Publish(msg))?;
        let (send_stream, recv_stream) = self.webtransport.open_bi().await?;
        let mut writer = super::ResetOnDropWriter::new(super::Writer::new(send_stream));
        writer.write(&frame).await?;

        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::unbounded_channel::<BidiResponse>();
        let mut this = self.clone();

        if self
            .bidi_task_tx
            .send(Box::pin(async move {
                let mut abort = abort;
                let mut reader = super::Reader::new(recv_stream);
                // The peer may finish its half as soon as it has replied
                // PUBLISH_OK. That does not end the request, so reading stops
                // while the write half stays open for PUBLISH_DONE.
                let mut reading = true;
                let mut writer_closed = false;

                let result = loop {
                    tokio::select! {
                        // Messages we write on our own request stream
                        // (PUBLISH_DONE). Draft-18 omits the Request ID here.
                        outgoing = stream_rx.recv() => {
                            let Some(mut outgoing) = outgoing else { break Ok(()) };
                            let terminal = matches!(outgoing.message, Message::PublishDone(_));
                            if let Err(err) = Session::encode_bidi_response(&mut writer, &outgoing.message).await {
                                outgoing.complete(Err(DeliveryError::from_session_error(&err)));
                                tracing::debug!(request_id, error = %err, "failed writing on PUBLISH request stream");
                                writer.reset(0);
                                writer_closed = true;
                                break Ok(());
                            }
                            if terminal {
                                let finish = writer.finish();
                                writer_closed = true;
                                if finish.is_ok() {
                                    abort.complete();
                                }
                                outgoing.complete(
                                    finish
                                        .as_ref()
                                        .map(|_| ())
                                        .map_err(DeliveryError::from_session_error),
                                );
                                if let Err(err) = finish {
                                    tracing::debug!(request_id, error = %err, "failed to FIN PUBLISH request stream");
                                }
                                break Ok(());
                            }
                            outgoing.complete(Ok(()));
                        }
                        // Not cancellation-safe: a frame half-read when the
                        // write branch wins is lost. Safe only because that
                        // branch always breaks out of the loop.
                        response = Session::decode_bidi_response(&mut reader, request_id), if reading => {
                            match response {
                                Ok(msg) => match TryInto::<message::Subscriber>::try_into(msg) {
                                    Ok(sub_msg) => {
                                        if let Err(err) = this.recv_message(sub_msg) {
                                            break Err(err);
                                        }
                                    }
                                    Err(msg) => tracing::warn!(
                                        request_id,
                                        msg_type = msg.name(),
                                        "unexpected message on PUBLISH request stream"
                                    ),
                                },
                                Err(err) if err.is_stream_error() => {
                                    tracing::debug!(request_id, error = %err, "PUBLISH response stream reset by peer");
                                    abort.cancel_from_peer();
                                    break Ok(());
                                }
                                Err(err) if err.is_stream_ended() => {
                                    tracing::debug!(request_id, error = %err, "PUBLISH response reader ended");
                                    reading = false;
                                }
                                // An undecodable frame is not a resynchronisation
                                // point: draft-18 requires closing the session on an
                                // unknown message type, so this must not be skipped.
                                Err(err) => break Err(err),
                            }
                        }
                        stopped = writer.closed() => {
                            match stopped {
                                Ok(_) => {
                                    abort.cancel_from_peer();
                                    writer.reset(0);
                                    writer_closed = true;
                                    break Ok(());
                                }
                                Err(err) => break Err(err),
                            }
                        }
                    }
                };

                if !writer_closed {
                    let _ = writer.finish();
                }
                result
            }))
            .is_err()
        {
            return Err(SessionError::Internal);
        }

        Ok(stream_tx)
    }

    /// Attach the data-plane half of an outbound PUBLISH so a later
    /// UNSUBSCRIBE can stop it.
    pub(super) fn register_published_subscription(
        &mut self,
        id: u64,
        forwarder: ObjectForwarderRecv,
    ) -> Result<(), SessionError> {
        let mut publisheds = self.publisheds.lock().map_err(|_| SessionError::Internal)?;
        let entry = publisheds
            .get_mut(&id)
            .ok_or(SessionError::Serve(ServeError::Cancel))?;
        entry.forwarder = Some(forwarder);
        Ok(())
    }

    pub(super) fn drop_published(&self, id: u64) {
        let _ = self.remove_published(id);
    }

    fn send_bidi_response(
        &mut self,
        message: Message,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<(), DeliveryError>>, SessionError> {
        let id = message.response_target_id().ok_or(SessionError::Internal)?;
        Session::log_control_message(&message, "sent");
        if let Message::SubscribeOk(msg) = &message {
            self.add_mlog_event(|time| mlog::events::subscribe_ok_created(time, msg.id, msg));
        }

        let stream = self
            .bidi_response_map
            .lock()
            .map_err(|_| SessionError::Internal)?
            .get(&id)
            .cloned()
            .ok_or_else(|| DeliveryError::Cancelled.into_session_error())?;
        let (response, receipt) = BidiResponse::with_completion(message);
        stream
            .send(response)
            .map_err(|_| DeliveryError::Cancelled.into_session_error())?;
        Ok(receipt)
    }

    async fn send_bidi_response_and_wait(&mut self, message: Message) -> Result<(), SessionError> {
        let receipt = self.send_bidi_response(message)?;
        receipt
            .await
            .map_err(|_| DeliveryError::Cancelled.into_session_error())?
            .map_err(DeliveryError::into_session_error)
    }

    pub(super) fn send_subscribe_ok(
        &mut self,
        msg: message::SubscribeOk,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<(), DeliveryError>>, SessionError> {
        self.send_bidi_response(msg.into())
    }

    pub(super) async fn send_publish_done_and_wait(
        &mut self,
        msg: message::PublishDone,
    ) -> Result<(), SessionError> {
        self.send_bidi_response_and_wait(msg.into()).await
    }

    /// Fail an outbound PUBLISH whose request stream ended without completing
    /// terminal delivery. Successful PUBLISH_DONE delivery removes the entry
    /// before disarming the request-stream guard, making this a no-op.
    fn abort_published(&self, id: u64) {
        let Some(mut published) = self.remove_published(id) else {
            return;
        };

        tracing::debug!(
            request_id = id,
            "PUBLISH request stream closed before completion"
        );
        if let Err(err) = published.recv.recv_error(ServeError::Cancel) {
            tracing::debug!(request_id = id, error = %err, "failed to fail aborted PUBLISH");
        }
    }

    fn cancel_published(&self, id: u64) {
        let Some(mut published) = self.remove_published(id) else {
            return;
        };
        if let Err(err) = published.recv_unsubscribe() {
            tracing::debug!(request_id = id, error = %err, "failed to cancel reset PUBLISH");
        }
    }

    fn remove_published(&self, id: u64) -> Option<PublishedEntry> {
        self.drop_published_name(id);
        self.publisheds.lock().ok()?.remove(&id)
    }

    fn drop_published_name(&self, id: u64) {
        if let Ok(mut names) = self.published_names.lock() {
            names.remove_by_request_id(id);
        }
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

    /// Returns the next inbound SUBSCRIBE_NAMESPACE request.
    pub async fn subscribed_namespace(&mut self) -> Option<SubscribedNamespace> {
        self.unknown_subscribed_namespace.pop().await
    }

    /// Accept an inbound SUBSCRIBE_NAMESPACE and surface it to the application.
    ///
    /// Returns the transport-side half, which the caller drives with the
    /// request stream's writer and reader. Overlapping prefixes are rejected
    /// without reaching the application.
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
            // REQUEST_UPDATE: not yet implemented — send REQUEST_ERROR NOT_SUPPORTED (§4).
            message::Subscriber::RequestUpdate(msg) => {
                self.send_not_supported(msg.id, "request_update");
            }
            // REQUEST_OK from a subscriber accepts either our PUBLISH_NAMESPACE
            // or our PUBLISH; the request ID says which.
            message::Subscriber::RequestOk(msg) => self.recv_request_ok(msg)?,
            // REQUEST_ERROR from a subscriber rejects either our PUBLISH_NAMESPACE
            // or our PUBLISH; the request ID says which.
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
                    request_id = msg.id,
                    "received FETCH_CANCEL for unsupported FETCH — ignoring"
                );
            }
            message::Subscriber::TrackStatus(msg) => self.recv_track_status(msg)?,
            // SUBSCRIBE_NAMESPACE not yet implemented — send REQUEST_ERROR NOT_SUPPORTED (§4).
            message::Subscriber::SubscribeNamespace(msg) => {
                self.send_not_supported(msg.id, "subscribe_namespace");
            }
            message::Subscriber::PublishNamespaceCancel(_) => {
                return Err(SessionError::ProtocolViolation(
                    "PUBLISH_NAMESPACE_CANCEL is not part of draft-18".to_string(),
                ));
            }
            // Draft-18 removed the dedicated PUBLISH_OK type in favour of
            // REQUEST_OK, so we never send it. Accepting it on receive keeps
            // draft-16 peers working and costs nothing, since the payload is the
            // same shape. Retiring the type belongs with the other draft-16
            // leftovers still decoded here.
            message::Subscriber::PublishOk(msg) => {
                tracing::debug!(
                    target: "moq_transport::control",
                    request_id = msg.id,
                    "PUBLISH accepted with the draft-16 PUBLISH_OK type"
                );
                let msg = message::RequestOk {
                    id: msg.id,
                    params: msg.params,
                };
                self.recv_publish_ok(&msg)?;
            }
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

    /// Handle REQUEST_OK from a subscriber, accepting one of our requests
    /// (draft-18 §10.5).
    ///
    /// One message type answers PUBLISH_NAMESPACE and PUBLISH alike, so the
    /// request ID decides which, exactly as it does for REQUEST_ERROR. Guessing
    /// from the type instead would resolve a PUBLISH as a namespace
    /// registration and leave the publish waiting forever.
    fn recv_request_ok(&mut self, msg: message::RequestOk) -> Result<(), SessionError> {
        if self.recv_publish_namespace_ok(&msg)? {
            return Ok(());
        }

        if self.recv_publish_ok(&msg)? {
            return Ok(());
        }

        self.log_request_ok_parsed("unknown", &msg);
        tracing::debug!(
            target: "moq_transport::control",
            request_id = msg.id,
            "received REQUEST_OK for an unknown request, ignoring"
        );
        Ok(())
    }

    /// Acceptance of an outbound PUBLISH_NAMESPACE. Returns whether the request
    /// ID matched one.
    fn recv_publish_namespace_ok(
        &mut self,
        msg: &message::RequestOk,
    ) -> Result<bool, SessionError> {
        let matched = {
            // The publish_namespaces map is keyed by namespace; we must search by request_id.
            // TODO(itzmanish): maintain a second index keyed by request_id to make this O(1).
            let mut namespaces = self
                .publish_namespaces
                .lock()
                .map_err(|_| SessionError::Internal)?;
            match namespaces.iter_mut().find(|(_k, v)| v.request_id == msg.id) {
                Some(entry) => {
                    entry.1.recv_ok()?;
                    true
                }
                None => false,
            }
        };

        if matched {
            self.log_request_ok_parsed("publish_namespace", msg);
        }
        Ok(matched)
    }

    /// Handle REQUEST_ERROR from a subscriber — rejection of an outbound
    /// PUBLISH_NAMESPACE or PUBLISH (draft-16 §9.8).
    fn recv_request_error(&mut self, msg: message::RequestError) -> Result<(), SessionError> {
        if let Some(recv) = self.drop_publish_namespace(msg.id) {
            self.log_request_error_parsed("publish_namespace", &msg);
            recv.recv_error(ServeError::Closed(msg.error_code))?;
            return Ok(());
        }

        if let Some(mut published) = self.remove_published(msg.id) {
            self.log_request_error_parsed("publish", &msg);
            published
                .recv
                .recv_error(ServeError::Closed(msg.error_code))?;
            return Ok(());
        }

        self.log_request_error_parsed("unknown", &msg);
        Ok(())
    }

    /// Acceptance of an outbound PUBLISH. Returns whether the request ID matched
    /// one.
    fn recv_publish_ok(&mut self, msg: &message::RequestOk) -> Result<bool, SessionError> {
        let matched = {
            let mut publisheds = self.publisheds.lock().map_err(|_| SessionError::Internal)?;
            match publisheds.get_mut(&msg.id) {
                Some(entry) => {
                    entry.recv.recv_ok(msg)?;
                    true
                }
                None => false,
            }
        };

        if matched {
            self.log_request_ok_parsed("publish", msg);
        }
        Ok(matched)
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
            if subscribed_names.contains_name(&full_name) {
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

    /// Handle UNSUBSCRIBE, which terminates either an inbound SUBSCRIBE we are
    /// serving or an outbound PUBLISH the peer no longer wants. The request ID
    /// alone does not say which, so both maps are checked.
    fn recv_unsubscribe(&mut self, msg: message::Unsubscribe) -> Result<(), SessionError> {
        let known_subscribe = {
            let mut subscribeds = self
                .subscribeds
                .lock()
                .map_err(|_| SessionError::Internal)?;
            match subscribeds.get_mut(&msg.id) {
                Some(subscribed) => {
                    subscribed.recv_unsubscribe()?;
                    true
                }
                None => false,
            }
        };

        if known_subscribe {
            self.remove_subscribe(msg.id)?;
            return Ok(());
        }

        if let Some(mut published) = self.remove_published(msg.id) {
            published.recv_unsubscribe()?;
            return Ok(());
        }

        // An UNSUBSCRIBE for an ID we no longer hold is an ordinary race: the
        // peer's UNSUBSCRIBE can cross with our own PUBLISH_DONE for the same
        // subscription. Closing the session over it would turn a benign
        // interleaving into a connection failure.
        tracing::debug!(
            target: "moq_transport::control",
            request_id = msg.id,
            "UNSUBSCRIBE for unknown subscription — ignoring"
        );
        Ok(())
    }

    /// Pre-send hook: clean up internal state when terminal publisher messages are enqueued.
    fn act_on_message_to_send<T: Into<message::Publisher>>(
        &mut self,
        msg: T,
    ) -> message::Publisher {
        let msg = msg.into();
        if let message::Publisher::PublishNamespaceDone(m) = &msg {
            let _ = self.drop_publish_namespace(m.id);
        } else if let message::Publisher::PublishDone(m) = &msg {
            self.drop_subscribe(m.id);
        }
        msg
    }

    /// Enqueue a control message for sending (fire-and-forget).
    pub(super) fn send_message<T: Into<message::Publisher> + Into<Message>>(&mut self, msg: T) {
        let msg = self.act_on_message_to_send(msg);
        self.outgoing.push(msg.into()).ok();
    }

    pub(super) fn drop_subscribe(&mut self, id: u64) {
        let _ = self.remove_subscribe(id);
    }

    pub(super) fn cancel_subscribe(&self, id: u64) {
        if let Ok(mut subscribeds) = self.subscribeds.lock() {
            if let Some(mut subscribed) = subscribeds.remove(&id) {
                let _ = subscribed.recv_unsubscribe();
            }
        }
        let _ = Self::drop_subscribed_name(&self.subscribed_names, id);
    }

    fn remove_subscribe(&mut self, id: u64) -> Result<(), SessionError> {
        self.subscribeds
            .lock()
            .map_err(|_| SessionError::Internal)?
            .remove(&id);
        Self::drop_subscribed_name(&self.subscribed_names, id)
    }

    fn drop_subscribed_name(
        subscribed_names: &Arc<Mutex<NameRegistry>>,
        id: u64,
    ) -> Result<(), SessionError> {
        subscribed_names
            .lock()
            .map_err(|_| SessionError::Internal)?
            .remove_by_request_id(id);

        Ok(())
    }

    pub(super) fn drop_publish_namespace(&mut self, id: u64) -> Option<PublishNamespaceRecv> {
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

    fn abort_publish_namespace(&mut self, id: u64) {
        let Some(recv) = self.drop_publish_namespace(id) else {
            return;
        };
        if let Err(err) = recv.recv_error(ServeError::Cancel) {
            tracing::debug!(
                request_id = id,
                error = %err,
                "failed to fail aborted PUBLISH_NAMESPACE"
            );
        }
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
    use std::sync::{Arc, Mutex};

    use crate::{
        coding::{KeyValuePairs, ReasonPhrase, TrackName, TrackNamespace},
        data,
        message::{self, Message},
        serve::{self, FullTrackName, ServeError},
        session::{test_support::loopback_session_pair, Reader, RequestId, Session, Writer},
        watch::Queue,
    };
    use bytes::Bytes;

    use super::{
        split_published_state, AbortPublishedOnDrop, NameRegistry, PublishedEntry, PublishedRecv,
        PublishNamespace, Publisher,
    };

    fn full_track_name(namespace: &str, name: &str) -> FullTrackName {
        FullTrackName {
            namespace: TrackNamespace::from_utf8_path(namespace),
            name: TrackName::from(name),
        }
    }

    fn subgroup_track() -> (serve::SubgroupsWriter, serve::TrackReader) {
        let (writer, reader) =
            serve::Track::new(TrackNamespace::from_utf8_path("test"), "track").produce();
        (writer.subgroups().unwrap(), reader)
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
        assert_eq!(names.get_by_name(&active_track), Some(8));
    }

    #[test]
    fn publish_frame_rejects_subscribe_only_params_before_stream_open() {
        let mut params = KeyValuePairs::default();
        params.set_rendezvous_timeout(1_000);
        let msg = Message::Publish(message::Publish {
            id: 0,
            track_namespace: TrackNamespace::from_utf8_path("test/ns"),
            track_name: "video".into(),
            track_alias: 0,
            params,
            track_extensions: Default::default(),
        });

        assert!(matches!(
            super::super::encode_request_frame(&msg),
            Err(crate::session::SessionError::Encode(
                crate::coding::EncodeError::InvalidValue
            ))
        ));
    }

    #[tokio::test]
    async fn publish_reservation_guard_cleans_up_before_driver_installation() {
        let (publisher_session, _peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
        );
        let request_id = 0;
        let track = full_track_name("test", "track");
        publisher
            .published_names
            .lock()
            .unwrap()
            .insert(track.clone(), request_id);
        let (_state, recv_state) = split_published_state(true);
        publisher.publisheds.lock().unwrap().insert(
            request_id,
            PublishedEntry::new(PublishedRecv::new(recv_state)),
        );

        drop(AbortPublishedOnDrop {
            publisher: publisher.clone(),
            request_id: Some(request_id),
        });

        assert!(!publisher
            .published_names
            .lock()
            .unwrap()
            .contains_name(&track));
        assert!(!publisher
            .publisheds
            .lock()
            .unwrap()
            .contains_key(&request_id));
    }

    #[tokio::test]
    async fn cancellation_before_forwarder_registration_is_not_internal() {
        let (publisher_session, _peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
        );
        let (_forwarder, recv) = super::super::ObjectForwarder::new(publisher.clone(), 0, None);

        assert!(matches!(
            publisher.register_published_subscription(0, recv),
            Err(crate::session::SessionError::Serve(
                serve::ServeError::Cancel
            ))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_publish_waits_for_encoded_done_and_fin_with_exact_stream_count() {
        for expected_stream_count in [0, 2] {
            let (publisher_session, peer_session) = loopback_session_pair().await;
            let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
            let (bidi_task_tx, mut bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut publisher = Publisher::new(
                outgoing,
                publisher_session,
                None,
                RequestId::new(0, 1),
                bidi_task_tx,
            );
            let request_peer = peer_session.clone();
            let request = tokio::spawn(async move {
                let (send_stream, recv_stream) = request_peer.accept_bi().await.unwrap();
                let mut reader = Reader::new(recv_stream);
                let Message::Publish(publish) = reader.decode().await.unwrap() else {
                    panic!("expected PUBLISH request");
                };
                let mut writer = Writer::new(send_stream);
                Session::encode_bidi_response(
                    &mut writer,
                    &Message::RequestOk(message::RequestOk {
                        id: publish.id,
                        params: KeyValuePairs::default(),
                    }),
                )
                .await
                .unwrap();
                writer.finish().unwrap();
                (publish.id, reader)
            });

            let (mut subgroups, track) = subgroup_track();
            let published = publisher
                .publish(track, KeyValuePairs::default())
                .await
                .unwrap();
            let (request_id, mut request_reader) = request.await.unwrap();
            let request_driver = tokio::spawn(bidi_task_rx.recv().await.unwrap());
            let serve = tokio::spawn(published.serve());
            for group_id in 0..expected_stream_count {
                let mut subgroup = subgroups
                    .create(serve::Subgroup {
                        group_id,
                        subgroup_id: 0,
                        priority: 128,
                    })
                    .unwrap();
                subgroup.write(Bytes::from_static(b"x")).unwrap();
                drop(subgroup);
                let stream = peer_session.accept_uni().await.unwrap();
                let mut reader = Reader::new(stream);
                let _: data::StreamHeader = reader.decode().await.unwrap();
                let object: data::SubgroupObjectExt = reader.decode().await.unwrap();
                assert_eq!(
                    reader
                        .read_chunk(object.payload_length)
                        .await
                        .unwrap()
                        .unwrap(),
                    b"x"[..]
                );
                assert!(reader.done().await.unwrap());
            }
            drop(subgroups);

            let Message::PublishDone(done) =
                Session::decode_bidi_response(&mut request_reader, request_id)
                    .await
                    .unwrap()
            else {
                panic!("expected PUBLISH_DONE response");
            };
            assert_eq!(done.stream_count, expected_stream_count);
            assert!(request_reader.done().await.unwrap());
            serve.await.unwrap().unwrap();
            request_driver.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn dropping_direct_publish_driver_unblocks_serve() {
        let (publisher_session, _peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, mut bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
        );
        let (subgroups, track) = subgroup_track();
        let published = publisher
            .publish(track, KeyValuePairs::default())
            .await
            .unwrap();
        let driver = bidi_task_rx.recv().await.unwrap();
        drop(driver);
        drop(subgroups);

        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), published.serve())
                .await
                .unwrap(),
            Err(crate::session::SessionError::Serve(
                serve::ServeError::Cancel
            ))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_publish_source_failure_sends_done_before_returning_error() {
        let (publisher_session, peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, mut bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
        );
        let request_peer = peer_session.clone();
        let request = tokio::spawn(async move {
            let (send_stream, recv_stream) = request_peer.accept_bi().await.unwrap();
            let mut reader = Reader::new(recv_stream);
            let Message::Publish(publish) = reader.decode().await.unwrap() else {
                panic!("expected PUBLISH request");
            };
            let mut writer = Writer::new(send_stream);
            Session::encode_bidi_response(
                &mut writer,
                &Message::RequestOk(message::RequestOk {
                    id: publish.id,
                    params: KeyValuePairs::default(),
                }),
            )
            .await
            .unwrap();
            writer.finish().unwrap();
            (publish.id, reader)
        });
        let (subgroups, track) = subgroup_track();
        let published = publisher
            .publish(track, KeyValuePairs::default())
            .await
            .unwrap();
        let (request_id, mut request_reader) = request.await.unwrap();
        let request_driver = tokio::spawn(bidi_task_rx.recv().await.unwrap());
        let serve = tokio::spawn(published.serve());
        subgroups.close(serve::ServeError::Closed(0x42)).unwrap();

        let Message::PublishDone(done) =
            Session::decode_bidi_response(&mut request_reader, request_id)
                .await
                .unwrap()
        else {
            panic!("expected PUBLISH_DONE for source failure");
        };
        assert_eq!(done.status_code, 0x42);
        assert_eq!(done.stream_count, 0);
        assert!(request_reader.done().await.unwrap());
        assert!(matches!(
            serve.await.unwrap(),
            Err(crate::session::SessionError::Serve(
                serve::ServeError::Closed(0x42)
            ))
        ));
        request_driver.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_publish_peer_cancellation_suppresses_publish_done() {
        let (publisher_session, peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, mut bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
        );
        let request_peer = peer_session.clone();
        let (cancel_request, wait_to_cancel) = tokio::sync::oneshot::channel();
        let request = tokio::spawn(async move {
            let (send_stream, recv_stream) = request_peer.accept_bi().await.unwrap();
            let mut reader = Reader::new(recv_stream);
            let Message::Publish(publish) = reader.decode().await.unwrap() else {
                panic!("expected PUBLISH request");
            };
            let mut writer = Writer::new(send_stream);
            Session::encode_bidi_response(
                &mut writer,
                &Message::RequestOk(message::RequestOk {
                    id: publish.id,
                    params: KeyValuePairs::default(),
                }),
            )
            .await
            .unwrap();
            wait_to_cancel.await.unwrap();
            writer.reset(0);
            reader
        });

        let (mut subgroups, reader) = subgroup_track();
        let published = publisher
            .publish(reader, KeyValuePairs::default())
            .await
            .unwrap();
        let request_driver = tokio::spawn(bidi_task_rx.recv().await.unwrap());
        let serve = tokio::spawn(published.serve());
        let mut subgroup = subgroups
            .create(serve::Subgroup {
                group_id: 0,
                subgroup_id: 0,
                priority: 128,
            })
            .unwrap();
        subgroup.write(Bytes::from_static(b"x")).unwrap();
        let stream = peer_session.accept_uni().await.unwrap();
        let mut data_reader = Reader::new(stream);
        let _: data::StreamHeader = data_reader.decode().await.unwrap();
        let object: data::SubgroupObjectExt = data_reader.decode().await.unwrap();
        assert_eq!(
            data_reader
                .read_chunk(object.payload_length)
                .await
                .unwrap()
                .unwrap(),
            b"x"[..]
        );
        cancel_request.send(()).unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), serve)
            .await
            .unwrap()
            .unwrap();
        result.unwrap();
        assert!(matches!(
            data_reader.done().await,
            Err(crate::session::SessionError::WebTransport(
                web_transport::Error::Read(web_transport::quinn::ReadError::Reset(0))
            ))
        ));
        let mut request_reader = request.await.unwrap();
        assert!(request_reader.done().await.unwrap());
        request_driver.await.unwrap().unwrap();
        assert!(!publisher
            .published_names
            .lock()
            .unwrap()
            .contains_name(&full_track_name("test", "track")));
        drop(subgroup);
        drop(subgroups);
    }

    #[tokio::test]
    async fn legacy_namespace_cancel_is_a_protocol_violation() {
        let (client, _server) = loopback_session_pair().await;
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut publisher = Publisher::new(
            Queue::default(),
            client,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
        );

        let err = publisher
            .recv_message(message::Subscriber::PublishNamespaceCancel(
                message::PublishNamespaceCancel {
                    id: 0,
                    error_code: 1,
                    reason_phrase: ReasonPhrase("cancelled".to_string()),
                },
            ))
            .unwrap_err();

        assert!(matches!(
            err,
            crate::session::SessionError::ProtocolViolation(reason)
                if reason == "PUBLISH_NAMESPACE_CANCEL is not part of draft-18"
        ));
    }

    #[tokio::test]
    async fn accepted_namespace_survives_response_fin_until_local_cancel() {
        let (client, server) = loopback_session_pair().await;
        let (bidi_task_tx, mut bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut publisher = Publisher::new(
            Queue::default(),
            client,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
        );
        let namespace = TrackNamespace::from_utf8_path("test");
        let (publish, recv, cancel) =
            PublishNamespace::new(publisher.clone(), 0, namespace.clone());
        publisher
            .publish_namespaces
            .lock()
            .unwrap()
            .insert(namespace, recv);
        publisher
            .open_publish_namespace_stream(0, publish.wire_message(), cancel)
            .await
            .unwrap();

        let pump = tokio::spawn(bidi_task_rx.recv().await.unwrap());
        let (peer_send, peer_recv) = server.accept_bi().await.unwrap();
        let mut peer_writer = Writer::new(peer_send);
        let mut peer_reader = Reader::new(peer_recv);
        assert!(matches!(
            peer_reader.decode::<Message>().await.unwrap(),
            Message::PublishNamespace(_)
        ));
        Session::encode_bidi_response(
            &mut peer_writer,
            &Message::RequestOk(message::RequestOk {
                id: 0,
                params: Default::default(),
            }),
        )
        .await
        .unwrap();
        peer_writer.finish().unwrap();

        publish.ok().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), publish.closed())
                .await
                .is_err()
        );

        drop(publish);

        pump.await.unwrap().unwrap();
        let reset = peer_reader.done().await.unwrap_err();
        assert!(reset.is_stream_error());
    }

    #[tokio::test]
    async fn response_reset_closes_accepted_namespace() {
        let (client, server) = loopback_session_pair().await;
        let (bidi_task_tx, mut bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut publisher = Publisher::new(
            Queue::default(),
            client,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
        );
        let namespace = TrackNamespace::from_utf8_path("test");
        let (publish, recv, cancel) =
            PublishNamespace::new(publisher.clone(), 0, namespace.clone());
        publisher
            .publish_namespaces
            .lock()
            .unwrap()
            .insert(namespace, recv);
        publisher
            .open_publish_namespace_stream(0, publish.wire_message(), cancel)
            .await
            .unwrap();

        let pump = tokio::spawn(bidi_task_rx.recv().await.unwrap());
        let (peer_send, peer_recv) = server.accept_bi().await.unwrap();
        let mut peer_writer = Writer::new(peer_send);
        let mut peer_reader = Reader::new(peer_recv);
        assert!(matches!(
            peer_reader.decode::<Message>().await.unwrap(),
            Message::PublishNamespace(_)
        ));
        Session::encode_bidi_response(
            &mut peer_writer,
            &Message::RequestOk(message::RequestOk {
                id: 0,
                params: Default::default(),
            }),
        )
        .await
        .unwrap();
        publish.ok().await.unwrap();

        peer_writer.reset(super::super::CANCELLED_STREAM_CODE);

        assert!(matches!(publish.closed().await, Err(ServeError::Cancel)));
        pump.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn request_stop_does_not_discard_namespace_rejection() {
        let (client, server) = loopback_session_pair().await;
        let (bidi_task_tx, mut bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut publisher = Publisher::new(
            Queue::default(),
            client,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
        );
        let namespace = TrackNamespace::from_utf8_path("test");
        let (publish, recv, cancel) =
            PublishNamespace::new(publisher.clone(), 0, namespace.clone());
        publisher
            .publish_namespaces
            .lock()
            .unwrap()
            .insert(namespace, recv);
        publisher
            .open_publish_namespace_stream(0, publish.wire_message(), cancel)
            .await
            .unwrap();

        let pump = tokio::spawn(bidi_task_rx.recv().await.unwrap());
        let (peer_send, peer_recv) = server.accept_bi().await.unwrap();
        let mut peer_writer = Writer::new(peer_send);
        let mut peer_reader = Reader::new(peer_recv);
        assert!(matches!(
            peer_reader.decode::<Message>().await.unwrap(),
            Message::PublishNamespace(_)
        ));

        peer_reader.stop(super::super::CANCELLED_STREAM_CODE);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        Session::encode_bidi_response(
            &mut peer_writer,
            &Message::RequestError(message::RequestError {
                id: 0,
                error_code: message::RequestErrorCode::Uninterested as u64,
                retry_interval: 0,
                reason: crate::coding::ReasonPhrase("not interested".to_string()),
            }),
        )
        .await
        .unwrap();
        peer_writer.finish().unwrap();

        assert!(matches!(
            publish.ok().await,
            Err(ServeError::Closed(code))
                if code == message::RequestErrorCode::Uninterested as u64
        ));
        pump.await.unwrap().unwrap();
    }
}
