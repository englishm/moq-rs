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

use super::Subscriber;

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

        self.session.send_message(message::RequestOk {
            id: self.request_id,
            params,
        });
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

        self.session.send_request_error(
            "publish",
            message::RequestError {
                id: self.request_id,
                error_code,
                retry_interval: 0,
                reason: ReasonPhrase("uninterested".to_string()),
            },
        );

        // Clean up subscriber-side state for this PUBLISH.
        self.session.remove_publish_received(self.request_id);
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

        let mut subgroups = match writer {
            TrackWriterMode::Track(track) => track.subgroups()?,
            TrackWriterMode::Subgroups(subgroups) => subgroups,
            _ => return Err(ServeError::Mode),
        };

        let subgroup_writer = subgroups.create(serve::Subgroup {
            group_id: header.group_id,
            subgroup_id: header.subgroup_id.unwrap_or(0),
            priority: header.publisher_priority,
        })?;

        self.writer = Some(subgroups.into());
        Ok(subgroup_writer)
    }

    /// Write a datagram Object into the track.
    ///
    /// Mirrors `SubscribeRecv::datagram`.
    pub fn datagram(&mut self, datagram: data::Datagram) -> Result<(), ServeError> {
        let writer = self.writer.take().ok_or(ServeError::Done)?;

        match writer {
            TrackWriterMode::Track(track) => {
                let mut datagrams = track.datagrams()?;
                datagrams.write(serve::Datagram {
                    group_id: datagram.group_id,
                    object_id: datagram.object_id.unwrap_or(0),
                    priority: datagram.publisher_priority,
                    payload: datagram.payload.unwrap_or_default(),
                    extension_headers: datagram.extension_headers.unwrap_or_default(),
                })?;
                self.writer = Some(TrackWriterMode::Datagrams(datagrams));
                Ok(())
            }
            TrackWriterMode::Datagrams(mut datagrams) => {
                datagrams.write(serve::Datagram {
                    group_id: datagram.group_id,
                    object_id: datagram.object_id.unwrap_or(0),
                    priority: datagram.publisher_priority,
                    payload: datagram.payload.unwrap_or_default(),
                    extension_headers: datagram.extension_headers.unwrap_or_default(),
                })?;
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
    /// Closes the writer so the `TrackReader` sees end-of-track.
    pub fn recv_done(&mut self, status_code: u64) {
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
        let (pr, recv, subscriber, _outgoing) = make_pair_with_outgoing(request_id).await;
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
        Queue<message::Message>,
    ) {
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let outgoing = Queue::default();
        let subscriber = crate::session::Subscriber::new(
            outgoing.clone(),
            loopback_session().await,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
        );
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
            writer,
            reader,
        );
        (pr, recv, subscriber, outgoing)
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
    async fn recv_done_closes_writer_and_sets_state() {
        let (_pr, mut recv, _sub) = make_pair(1).await;
        assert!(recv.writer.is_some());

        recv.recv_done(message::PublishDoneCode::TrackEnded as u64);

        assert!(
            recv.writer.is_none(),
            "writer must be dropped after PUBLISH_DONE"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_returns_ok_for_track_ended() {
        let (pr, mut recv, _sub) = make_pair(3).await;

        recv.recv_done(message::PublishDoneCode::TrackEnded as u64);

        assert_eq!(pr.closed().await, Ok(()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_returns_error_for_non_track_ended() {
        let (pr, mut recv, _sub) = make_pair(4).await;

        recv.recv_done(message::PublishDoneCode::Expired as u64);

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
        let (mut pr, _recv, _sub, mut outgoing) = make_pair_with_outgoing(6).await;

        pr.accept(false).expect("accept a fresh PUBLISH");

        let sent = outgoing
            .pop()
            .await
            .expect("acceptance is sent to the peer");
        let message::Message::RequestOk(msg) = sent else {
            panic!(
                "PUBLISH must be accepted with REQUEST_OK, got {}",
                sent.name()
            );
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
