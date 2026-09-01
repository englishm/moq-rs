// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Inbound PUBLISH handling: this endpoint, acting as subscriber, receives a
//! PUBLISH from a publisher and can accept it with REQUEST_OK, reject it with
//! REQUEST_ERROR, and then receive Objects on the resulting subscription
//! (draft-18 §10.10 / §10.5 / §10.6).
//!
//! # Ownership model
//!
//! The transport layer retains the `TrackWriter` (inside `PublishReceivedRecv`)
//! so the stream/datagram receive paths can write inbound Objects without
//! application involvement. The application receives the `TrackReader` from
//! `PublishReceived::ok`. This mirrors the outbound-SUBSCRIBE receive path
//! where `SubscribeRecv` owns the writer.
//!
//! This is the inbound counterpart of `Published` (outbound PUBLISH). Both use
//! the same `TrackOrigin` alias map in `Subscriber` for stream routing.

use crate::{
    coding::{KeyValuePairs, Location, ReasonPhrase, TrackName, TrackNamespace},
    data,
    message::{self, RequestErrorCode},
    serve::{self, ServeError, TrackReader, TrackWriterMode},
    watch::State,
};

use super::{DoneOutcome, StreamDrain, Subscriber};

// ── Shared state ──────────────────────────────────────────────────────────────

/// State shared between `PublishReceived` (application handle) and
/// `PublishReceivedRecv` (transport handle).
pub(crate) struct PublishReceivedState {
    /// True once PUBLISH_DONE has been received from the publisher.
    done: bool,
    /// Terminal result; set when `done` becomes true.
    closed: Result<(), ServeError>,
}

impl Default for PublishReceivedState {
    fn default() -> Self {
        Self {
            done: false,
            closed: Ok(()),
        }
    }
}

// ── Application-facing handle ─────────────────────────────────────────────────

/// An inbound PUBLISH received by this endpoint acting as subscriber
/// (draft-18 §10.10).
///
/// Call [`ok`](Self::ok) to accept the subscription and obtain the
/// [`TrackReader`]. Dropping without calling `ok` sends `REQUEST_ERROR
/// UNINTERESTED` back to the publisher.
pub struct PublishReceived {
    session: Subscriber,
    state: State<PublishReceivedState>,
    reader: Option<TrackReader>,

    /// Request ID of the inbound PUBLISH.
    request_id: u64,
    /// Track Alias chosen by the publisher (§10.1).
    track_alias: u64,
    /// Full track identifier.
    namespace: TrackNamespace,
    name: TrackName,
    /// Initial Forward value parsed from the PUBLISH params (§10.10).
    initial_forward: bool,
    /// LARGEST_OBJECT from the PUBLISH params, if present (§5.1).
    largest_location: Option<Location>,

    /// True once `ok()` has been called successfully.
    ok: bool,
    /// Optional override for the rejection error sent on drop.
    error: Option<ServeError>,
}

impl PublishReceived {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        session: Subscriber,
        request_id: u64,
        track_alias: u64,
        namespace: TrackNamespace,
        name: TrackName,
        initial_forward: bool,
        largest_location: Option<Location>,
        reader: TrackReader,
        state: State<PublishReceivedState>,
    ) -> Self {
        Self {
            session,
            state,
            reader: Some(reader),
            request_id,
            track_alias,
            namespace,
            name,
            initial_forward,
            largest_location,
            ok: false,
            error: None,
        }
    }

    /// Move out the `TrackReader` before accepting.
    ///
    /// This lets relays register local/coordinator state before accepting the
    /// PUBLISH. It does not answer the publisher; callers must still call
    /// [`accept`](Self::accept) or drop this handle to reject.
    pub fn take_reader(&mut self) -> Result<TrackReader, ServeError> {
        self.reader.take().ok_or(ServeError::Done)
    }

    /// Accept the PUBLISH by answering REQUEST_OK (draft-18 §10.5).
    ///
    /// `forward` sets the initial Forward State:
    ///   - `true` the publisher may start transmitting Objects immediately.
    ///   - `false` the publisher pauses until Forward is later set to 1.
    ///
    /// Note: post-establishment `REQUEST_UPDATE` is not supported yet, so
    /// passing `false` keeps the publisher paused for the lifetime of the
    /// subscription in this implementation.
    ///
    /// Track Properties stay empty. §10.5 requires that of every REQUEST_OK
    /// except TRACK_STATUS_OK, and sending them here would oblige the peer to
    /// close the session with PROTOCOL_VIOLATION.
    pub fn accept(&mut self, forward: bool) -> Result<(), ServeError> {
        if self.ok {
            return Err(ServeError::Duplicate);
        }

        let mut params = KeyValuePairs::default();
        params.set_forward(forward);

        self.session.send_publish_response(
            message::RequestOk {
                id: self.request_id,
                params,
            }
            .into(),
        )?;
        self.ok = true;

        Ok(())
    }

    /// Accept the PUBLISH and return the `TrackReader`.
    pub fn ok(&mut self, forward: bool) -> Result<TrackReader, ServeError> {
        let reader = self.take_reader()?;
        self.accept(forward)?;
        Ok(reader)
    }

    /// Mark this track for rejection with a specific error on drop.
    pub fn close(mut self, err: ServeError) {
        self.error = Some(err);
    }

    /// Wait until the publisher sends PUBLISH_DONE or the session closes.
    ///
    /// Returns `Ok(())` on clean termination (TRACK_ENDED), or the error code
    /// from PUBLISH_DONE on all other outcomes.
    pub async fn closed(&self) -> Result<(), ServeError> {
        loop {
            {
                let state = self.state.lock();
                match state.closed.clone() {
                    Ok(()) => {}
                    Err(ServeError::Done) => return Ok(()),
                    Err(err) => return Err(err),
                }
                match state.modified() {
                    Some(notify) => notify,
                    None => return Ok(()),
                }
            }
            .await;
        }
    }

    pub fn namespace(&self) -> &TrackNamespace {
        &self.namespace
    }

    pub fn name(&self) -> &TrackName {
        &self.name
    }

    pub fn track_alias(&self) -> u64 {
        self.track_alias
    }

    pub fn initial_forward(&self) -> bool {
        self.initial_forward
    }

    pub fn largest_location(&self) -> Option<Location> {
        self.largest_location
    }
}

impl Drop for PublishReceived {
    fn drop(&mut self) {
        if self.ok {
            // Already accepted; nothing to send — PUBLISH_DONE arrives from the
            // publisher to terminate.
            return;
        }

        // Never accepted: send REQUEST_ERROR to reject the subscription (§10.6).
        let err = self.error.clone().unwrap_or(ServeError::Cancel);

        let error_code = match &err {
            ServeError::Cancel | ServeError::Done => RequestErrorCode::Uninterested as u64,
            ServeError::Duplicate => RequestErrorCode::DuplicateSubscription as u64,
            ServeError::NotFound | ServeError::NotFoundWithId(_, _) => {
                RequestErrorCode::DoesNotExist as u64
            }
            ServeError::NotImplemented(_) | ServeError::NotImplementedWithId(_, _) => {
                RequestErrorCode::NotSupported as u64
            }
            ServeError::Internal(_) | ServeError::InternalWithId(_, _) => {
                RequestErrorCode::InternalError as u64
            }
            ServeError::Closed(code) => *code,
            _ => RequestErrorCode::InternalError as u64,
        };

        if let Err(err) = self.session.send_publish_response(
            message::RequestError {
                id: self.request_id,
                error_code,
                retry_interval: 0,
                reason: ReasonPhrase("uninterested".to_string()),
            }
            .into(),
        ) {
            tracing::debug!(
                request_id = self.request_id,
                error = %err,
                "failed to send inbound PUBLISH rejection"
            );
        }
    }
}

// ── Transport-facing recv handle ──────────────────────────────────────────────

/// Transport-side bookkeeping for a single inbound PUBLISH.
///
/// Stored in `Subscriber::publishes_received`. Stream and datagram receive
/// paths write Objects directly into the `TrackWriterMode` here.
pub(crate) struct PublishReceivedRecv {
    /// Shared state so both the transport and app can observe PUBLISH_DONE.
    state: State<PublishReceivedState>,

    /// Write half for inbound Objects. The transport owns this so it can push
    /// Objects without going through the application.
    writer: Option<TrackWriterMode>,

    pub(super) default_publisher_priority: u8,

    /// PUBLISH_DONE Stream Count accounting (§10.11).
    drain: StreamDrain<u64>,
}

impl PublishReceivedRecv {
    /// Create a `PublishReceived` / `PublishReceivedRecv` pair from a PUBLISH
    /// message. Both handles share the same state.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn produce(
        session: Subscriber,
        request_id: u64,
        track_alias: u64,
        namespace: TrackNamespace,
        name: TrackName,
        initial_forward: bool,
        largest_location: Option<Location>,
        default_publisher_priority: u8,
        writer: serve::TrackWriter,
        reader: TrackReader,
    ) -> (PublishReceived, PublishReceivedRecv) {
        let (app_state, transport_state) = State::<PublishReceivedState>::default().split();

        let app = PublishReceived::new(
            session,
            request_id,
            track_alias,
            namespace,
            name,
            initial_forward,
            largest_location,
            reader,
            app_state,
        );
        let recv = Self {
            state: transport_state,
            writer: Some(writer.into()),
            default_publisher_priority,
            drain: StreamDrain::default(),
        };

        (app, recv)
    }

    /// Open a subgroup writer for the given subgroup header.
    ///
    /// Mirrors `SubscribeRecv::subgroup` so the same subgroup receive loop can
    /// serve both SUBSCRIBE-initiated and PUBLISH-initiated subscriptions.
    pub fn subgroup(
        &mut self,
        header: data::SubgroupHeader,
    ) -> Result<serve::SubgroupWriter, ServeError> {
        let writer = self.writer.take().ok_or(ServeError::Done)?;

        // Every failure below restores the writer: one rejected subgroup must
        // not strand the track in a state where nothing can be written, which
        // would also lose the PUBLISH_DONE status code later.
        let mut subgroups = match writer {
            TrackWriterMode::Track(track) => track.subgroups()?,
            TrackWriterMode::Subgroups(subgroups) => subgroups,
            other => {
                self.writer = Some(other);
                return Err(ServeError::Mode);
            }
        };

        let priority = if header.header_type.uses_default_priority() {
            self.default_publisher_priority
        } else {
            header.publisher_priority
        };
        let subgroup_writer = match subgroups.create_with_metadata(
            serve::Subgroup {
                group_id: header.group_id,
                subgroup_id: header.subgroup_id.unwrap_or(0),
                priority,
            },
            serve::SubgroupStreamMetadata {
                has_properties: header.header_type.has_properties(),
                end_of_group: header.header_type.end_of_group(),
                first_object: header.header_type.begins_with_first_object(),
            },
        ) {
            Ok(writer) => writer,
            Err(err) => {
                // Put the writer back: one rejected subgroup must not strand
                // the whole track in a state where nothing can be written.
                self.writer = Some(subgroups.into());
                return Err(err);
            }
        };

        self.writer = Some(subgroups.into());
        Ok(subgroup_writer)
    }

    /// Record that a data stream for this subscription was received.
    ///
    /// Called once per stream when its SUBGROUP_HEADER is accepted, which is
    /// what §10.11 counts: Stream Count includes "streams that contained no
    /// Objects (e.g., an empty Subgroup)", so this cannot wait for a first
    /// Object to arrive.
    ///
    /// The stream stays counted as open until
    /// [`note_stream_finished`](Self::note_stream_finished), so a subscription
    /// is never torn down underneath a stream that is still delivering
    /// Objects.
    pub fn note_stream_received(&mut self) {
        self.drain.note_stream_received();
    }

    /// Record that a data stream finished being read.
    ///
    /// Returns `true` if this was the last thing a deferred teardown was
    /// waiting for, in which case the caller must release the subscription's
    /// session-level state.
    #[must_use]
    pub fn note_stream_finished(&mut self) -> bool {
        match self.drain.note_stream_finished() {
            Some(status_code) => {
                self.finish(status_code);
                true
            }
            None => false,
        }
    }

    /// Write a datagram Object into the track.
    ///
    /// Mirrors `SubscribeRecv::datagram`.
    pub fn datagram(&mut self, datagram: data::Datagram) -> Result<(), ServeError> {
        let datagram = serve::Datagram::from_data(datagram, self.default_publisher_priority);
        let writer = self.writer.take().ok_or(ServeError::Done)?;

        match writer {
            TrackWriterMode::Track(track) => {
                let mut datagrams = track.datagrams()?;
                datagrams.write(datagram)?;
                self.writer = Some(TrackWriterMode::Datagrams(datagrams));
                Ok(())
            }
            TrackWriterMode::Datagrams(mut datagrams) => {
                datagrams.write(datagram)?;
                self.writer = Some(TrackWriterMode::Datagrams(datagrams));
                Ok(())
            }
            other => {
                self.writer = Some(other);
                Err(ServeError::Mode)
            }
        }
    }

    /// Called when PUBLISH_DONE arrives (§10.11).
    ///
    /// `stream_count` is the number of data streams the publisher opened for
    /// this subscription. The subscription is not torn down until that many
    /// streams have been received, because a stream that is still in flight
    /// carries Objects the publisher legitimately sent. Closing the writer
    /// early makes the `TrackReader` report end-of-track and those Objects are
    /// lost.
    ///
    /// See [`DoneOutcome`] for what the caller must do with each result.
    pub fn recv_done(&mut self, status_code: u64, stream_count: u64) -> DoneOutcome {
        // A repeat PUBLISH_DONE must not re-arm a subscription that already
        // finished, nor replace the count an earlier one is draining against.
        let (outcome, terminal) = self.drain.arm(status_code, stream_count);
        if let Some(status_code) = terminal {
            self.finish(status_code);
        }
        outcome
    }

    /// True while waiting for streams announced by PUBLISH_DONE.
    pub fn is_draining(&self) -> bool {
        self.drain.is_draining()
    }

    /// End the subscription because its request stream died.
    ///
    /// No further Stream Count can be announced, but a data stream already
    /// being read still has to finish, so this can defer exactly like
    /// PUBLISH_DONE does. See [`DoneOutcome`] for what the caller must do.
    pub fn abort(&mut self, status_code: u64) -> DoneOutcome {
        if self.drain.is_finished() {
            return DoneOutcome::Finished;
        }
        if self.drain.is_draining() {
            // A drain already owns teardown and carries the publisher's own
            // status code, which is better than this placeholder.
            return DoneOutcome::AlreadyDraining;
        }

        // Stream Count 0: nothing further can be announced on a dead request
        // stream, but streams already being read must still finish.
        let (outcome, terminal) = self.drain.arm(status_code, 0);
        if let Some(status_code) = terminal {
            self.finish(status_code);
        }
        outcome
    }

    /// Force teardown of a subscription still waiting for announced streams.
    ///
    /// §10.11 requires a timeout because the publisher may have set an
    /// incorrect Stream Count, reset a stream before its SUBGROUP_HEADER, or
    /// declared that it could not count its streams at all.
    ///
    /// Returns `true` if a pending teardown was completed by this call.
    pub fn drain_timeout(&mut self) -> bool {
        match self.drain.timeout() {
            Some(status_code) => {
                self.finish(status_code);
                true
            }
            None => false,
        }
    }

    fn finish(&mut self, status_code: u64) {
        self.drain.mark_finished();
        if let Some(mut state) = self.state.lock_mut() {
            state.done = true;
            state.closed = if status_code == message::PublishDoneCode::TrackEnded as u64 {
                Err(ServeError::Done)
            } else {
                Err(ServeError::Closed(status_code))
            };
        }
        // Drop the writer to signal end-of-track to any downstream readers.
        self.writer = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        coding::TrackNamespace,
        serve::Track,
        session::{test_support::loopback_session, RequestId},
        watch::Queue,
    };

    async fn make_pair(
        request_id: u64,
    ) -> (
        PublishReceived,
        PublishReceivedRecv,
        crate::session::Subscriber,
    ) {
        let (pr, recv, subscriber, _responses) = make_pair_with_outgoing(request_id).await;
        (pr, recv, subscriber)
    }

    /// Same, but hands back the session's outgoing queue so a test can see what
    /// went to the peer rather than only what changed in memory.
    async fn make_pair_with_outgoing(
        request_id: u64,
    ) -> (
        PublishReceived,
        PublishReceivedRecv,
        crate::session::Subscriber,
        tokio::sync::mpsc::UnboundedReceiver<crate::session::BidiResponse>,
    ) {
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let outgoing = Queue::default();
        let response_map: crate::session::BidiResponseMap = Default::default();
        let subscriber = crate::session::Subscriber::new(
            outgoing.clone(),
            loopback_session().await,
            crate::session::Transport::WebTransport,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
            response_map.clone(),
        );
        let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel();
        response_map.lock().unwrap().insert(request_id, response_tx);
        let (writer, reader) =
            Track::new(TrackNamespace::from_utf8_path("test"), "0.mp4").produce();
        let (pr, recv) = PublishReceivedRecv::produce(
            subscriber.clone(),
            request_id,
            42,
            TrackNamespace::from_utf8_path("test"),
            "0.mp4".into(),
            true,
            None,
            128,
            writer,
            reader,
        );
        (pr, recv, subscriber, response_rx)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn take_reader_returns_once() {
        let (mut pr, _recv, _sub) = make_pair(0).await;
        assert!(pr.take_reader().is_ok());
        assert!(
            pr.take_reader().is_err(),
            "reader must only be given out once"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn datagram_resolves_publish_default_priority_and_preserves_status() {
        let (mut publish, mut recv, _subscriber) = make_pair(13).await;
        recv.default_publisher_priority = 200;
        let reader = publish.take_reader().unwrap();

        recv.datagram(data::Datagram {
            datagram_type: data::DatagramType::StatusDefaultPriority,
            track_alias: 42,
            group_id: 8,
            object_id: None,
            publisher_priority: None,
            extension_headers: None,
            status: Some(data::ObjectStatus::EndOfTrack),
            payload: None,
        })
        .unwrap();

        let crate::serve::TrackReaderMode::Datagrams(mut datagrams) = reader.mode().await.unwrap()
        else {
            panic!("expected datagram mode");
        };
        let datagram = datagrams.read().await.unwrap().unwrap();
        assert_eq!(datagram.group_id, 8);
        assert_eq!(datagram.object_id, 0);
        assert_eq!(datagram.priority, 200);
        assert_eq!(datagram.status, data::ObjectStatus::EndOfTrack);
        assert!(!datagram.end_of_group);
        assert!(datagram.payload.is_empty());
    }

    fn subgroup_header(group_id: u64) -> data::SubgroupHeader {
        data::SubgroupHeader {
            header_type: data::StreamHeaderType::SubgroupIdExt,
            track_alias: 42,
            group_id,
            subgroup_id: Some(0),
            publisher_priority: 128,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recv_done_closes_writer_and_sets_state() {
        let (_pr, mut recv, _sub) = make_pair(1).await;
        assert!(recv.writer.is_some());

        // Stream Count 0: the publisher opened no data streams, so there is
        // nothing to wait for and teardown is immediate.
        assert_eq!(
            recv.recv_done(message::PublishDoneCode::TrackEnded as u64, 0),
            DoneOutcome::Finished
        );

        assert!(
            recv.writer.is_none(),
            "writer must be dropped after PUBLISH_DONE"
        );
    }

    /// Draft-18 §10.11: PUBLISH_DONE carries a Stream Count so the subscriber
    /// knows how many data streams may still arrive. Tearing the writer down
    /// as soon as PUBLISH_DONE is parsed discards streams that were legitimately
    /// sent, which silently drops Objects the publisher already delivered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_after_publish_done_is_accepted_until_stream_count_is_met() {
        let (_pr, mut recv, _sub) = make_pair(7).await;

        // PUBLISH_DONE announces one stream that has not been received yet.
        assert_eq!(
            recv.recv_done(message::PublishDoneCode::TrackEnded as u64, 1),
            DoneOutcome::DrainArmed
        );

        assert!(
            recv.writer.is_some(),
            "the writer must stay alive while an announced stream is outstanding"
        );
        recv.note_stream_received();
        recv.subgroup(subgroup_header(0))
            .expect("a stream announced by Stream Count must still be written");
        assert!(
            recv.writer.is_some(),
            "the writer stays alive while the stream is still being read"
        );

        assert!(
            recv.note_stream_finished(),
            "the last announced stream closing completes the teardown"
        );
        assert!(
            recv.writer.is_none(),
            "the writer is dropped once every announced stream has closed"
        );
    }

    /// §10.11 counts streams "including streams that contained no Objects
    /// (e.g., an empty Subgroup)", so a stream must count when its
    /// SUBGROUP_HEADER is accepted rather than when a first Object arrives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_empty_stream_still_counts_towards_stream_count() {
        let (_pr, mut recv, _sub) = make_pair(12).await;

        assert_eq!(
            recv.recv_done(message::PublishDoneCode::TrackEnded as u64, 1),
            DoneOutcome::DrainArmed
        );
        // No subgroup writer is ever opened because no Object arrives.
        recv.note_stream_received();
        assert!(recv.note_stream_finished());
        assert!(recv.writer.is_none());
    }

    /// The deferred teardown must still complete when the announced streams
    /// arrive, otherwise the reader would never observe end-of-track.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_resolves_once_announced_streams_arrive() {
        let (pr, mut recv, _sub) = make_pair(8).await;

        assert_eq!(
            recv.recv_done(message::PublishDoneCode::TrackEnded as u64, 2),
            DoneOutcome::DrainArmed
        );
        recv.note_stream_received();
        assert!(!recv.note_stream_finished(), "one of two streams");
        recv.note_stream_received();
        assert!(recv.note_stream_finished(), "two of two streams");

        assert_eq!(pr.closed().await, Ok(()));
    }

    /// Streams received before PUBLISH_DONE count towards Stream Count, so a
    /// subscription whose streams all arrived early tears down immediately.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streams_received_before_publish_done_satisfy_the_count() {
        let (_pr, mut recv, _sub) = make_pair(9).await;

        recv.note_stream_received();
        assert!(
            !recv.note_stream_finished(),
            "nothing to finish before PUBLISH_DONE"
        );
        assert_eq!(
            recv.recv_done(message::PublishDoneCode::TrackEnded as u64, 1),
            DoneOutcome::Finished
        );

        assert!(
            recv.writer.is_none(),
            "no stream is outstanding, so teardown is immediate"
        );
    }

    /// §10.11: a publisher unable to count its streams sets Stream Count to
    /// 2^62 - 1. The subscriber cannot wait for that many streams, so the drain
    /// timer is the only thing that ends the subscription.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_stream_count_is_resolved_by_the_drain_timer() {
        let (pr, mut recv, _sub) = make_pair(10).await;

        let unknown = (1u64 << 62) - 1;
        assert_eq!(
            recv.recv_done(message::PublishDoneCode::TrackEnded as u64, unknown),
            DoneOutcome::DrainArmed
        );
        assert!(
            recv.writer.is_some(),
            "an unknown Stream Count leaves the subscription draining"
        );

        assert!(recv.drain_timeout(), "the timer forces teardown");
        assert!(recv.writer.is_none(), "the writer is dropped on expiry");
        assert_eq!(pr.closed().await, Ok(()));
    }

    /// A duplicate PUBLISH_DONE must not re-arm a drain (each armed drain costs
    /// a timer) nor resurrect a finished subscription.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_publish_done_does_not_rearm_the_drain() {
        let (_pr, mut recv, _sub) = make_pair(11).await;

        assert_eq!(
            recv.recv_done(message::PublishDoneCode::TrackEnded as u64, 1),
            DoneOutcome::DrainArmed
        );
        assert_eq!(
            recv.recv_done(message::PublishDoneCode::TrackEnded as u64, 9),
            DoneOutcome::AlreadyDraining,
            "a repeat PUBLISH_DONE must not arm a second timer or change the count"
        );

        recv.note_stream_received();
        assert!(
            recv.note_stream_finished(),
            "the original count still applies"
        );
        assert_eq!(
            recv.recv_done(message::PublishDoneCode::TrackEnded as u64, 1),
            DoneOutcome::Finished,
            "a PUBLISH_DONE after teardown reports the subscription as finished"
        );
        assert!(!recv.drain_timeout(), "no drain is pending once finished");
    }

    /// The request stream dying mid-drain must not cancel the drain, nor lose
    /// the status code the publisher already sent in PUBLISH_DONE.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_leaves_an_armed_drain_alone() {
        let (pr, mut recv, _sub) = make_pair(13).await;

        assert_eq!(
            recv.recv_done(message::PublishDoneCode::Expired as u64, 4),
            DoneOutcome::DrainArmed
        );
        assert_eq!(
            recv.abort(message::PublishDoneCode::InternalError as u64),
            DoneOutcome::AlreadyDraining
        );

        // The drain still owns teardown, and still carries the publisher's code.
        assert!(recv.drain_timeout());
        assert!(
            matches!(pr.closed().await, Err(ServeError::Closed(code)) if code == message::PublishDoneCode::Expired as u64),
            "the publisher's status code survives the abort"
        );
    }

    /// With nothing in flight, a dead request stream ends the subscription
    /// immediately and applies the caller's code.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_ends_an_idle_subscription_immediately() {
        let (pr, mut recv, _sub) = make_pair(14).await;

        assert_eq!(
            recv.abort(message::PublishDoneCode::InternalError as u64),
            DoneOutcome::Finished
        );
        assert_eq!(
            recv.abort(message::PublishDoneCode::InternalError as u64),
            DoneOutcome::Finished,
            "a finished subscription is not finished twice"
        );

        assert!(matches!(
            pr.closed().await,
            Err(ServeError::Closed(code)) if code == message::PublishDoneCode::InternalError as u64
        ));
    }

    /// §10.11: the request stream can die before PUBLISH_DONE ever arrives. A
    /// data stream already being read must still be delivered — dropping the
    /// writer here loses Objects the publisher legitimately sent, which is the
    /// same failure this module exists to prevent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_waits_for_a_stream_still_being_read() {
        let (pr, mut recv, _sub) = make_pair(15).await;

        recv.note_stream_received();
        assert_eq!(
            recv.abort(message::PublishDoneCode::InternalError as u64),
            DoneOutcome::DrainArmed,
            "teardown defers while a stream is mid-transfer"
        );
        assert!(
            recv.writer.is_some(),
            "the writer must survive so the in-flight Object can be written"
        );
        recv.subgroup(subgroup_header(0))
            .expect("the in-flight stream can still be written");

        assert!(recv.note_stream_finished(), "closing the stream ends it");
        assert!(recv.writer.is_none());
        assert!(matches!(
            pr.closed().await,
            Err(ServeError::Closed(code)) if code == message::PublishDoneCode::InternalError as u64
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_returns_ok_for_track_ended() {
        let (pr, mut recv, _sub) = make_pair(3).await;

        assert_eq!(
            recv.recv_done(message::PublishDoneCode::TrackEnded as u64, 0),
            DoneOutcome::Finished
        );

        assert_eq!(pr.closed().await, Ok(()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_returns_error_for_non_track_ended() {
        let (pr, mut recv, _sub) = make_pair(4).await;

        assert_eq!(
            recv.recv_done(message::PublishDoneCode::Expired as u64, 0),
            DoneOutcome::Finished
        );

        assert!(matches!(
            pr.closed().await,
            Err(ServeError::Closed(code)) if code == message::PublishDoneCode::Expired as u64
        ));
    }

    /// Draft-18 removed the dedicated PUBLISH_OK type and made REQUEST_OK the
    /// response to PUBLISH (§10.5). Answering with the old type left direct
    /// PUBLISH unable to interoperate: a conforming peer rejects it, and its own
    /// REQUEST_OK was routed to the PUBLISH_NAMESPACE handler, so the publish
    /// never resolved.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accept_answers_with_request_ok() {
        let (mut pr, _recv, _sub, mut responses) = make_pair_with_outgoing(6).await;

        pr.accept(false).expect("accept a fresh PUBLISH");

        let sent = responses
            .recv()
            .await
            .expect("acceptance is sent to the peer");
        let msg = match sent.message {
            message::Message::RequestOk(msg) => msg,
            other => panic!(
                "PUBLISH must be accepted with REQUEST_OK, got {}",
                other.name()
            ),
        };
        assert_eq!(msg.id, 6, "the response carries the request it answers");
        assert_eq!(
            msg.params.forward().unwrap(),
            Some(false),
            "the Forward state asked for is carried on the acceptance"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publish_received_drop_without_ok_does_not_panic() {
        let (pr, _recv, _sub) = make_pair(5).await;
        // Drop without calling ok() — should send REQUEST_ERROR, not panic.
        drop(pr);
    }
}
