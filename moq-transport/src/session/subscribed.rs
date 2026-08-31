// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ops;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::stream::FuturesUnordered;
use futures::StreamExt;

use crate::coding::{Encode, EncodeError, KeyValuePairs, Location, ReasonPhrase};
use crate::message::RequestErrorCode;
use crate::mlog;
use crate::serve::{ServeError, TrackReaderMode};
use crate::watch::State;
use crate::{data, message, serve};

use super::{
    DeliveryError, DeliveryFilter, Publisher, ResetOnDropWriter, SessionError, SubscribeInfo,
    Writer,
};

// This file defines Publisher handling of inbound Subscriptions

#[derive(Debug)]
struct ObjectForwarderState {
    largest_location: Option<Location>,
    retry_interval: u64,
    /// Set to true when UNSUBSCRIBE is received.  When true, Drop skips sending
    /// PUBLISH_DONE or REQUEST_ERROR because the subscriber already terminated.
    unsubscribed: bool,
    closed: Result<(), ServeError>,
}

impl ObjectForwarderState {
    fn update_largest_location(&mut self, group_id: u64, object_id: u64) -> Result<(), ServeError> {
        let location = Location::new(group_id, object_id);
        self.largest_location = Some(
            self.largest_location
                .map_or(location, |largest| largest.max(location)),
        );

        Ok(())
    }
}

impl Default for ObjectForwarderState {
    fn default() -> Self {
        Self {
            largest_location: None,
            retry_interval: 0,
            unsubscribed: false,
            closed: Ok(()),
        }
    }
}

pub struct Subscribed {
    /// The tracknamespace and trackname for the subscription.
    pub info: SubscribeInfo,

    /// Data-plane half, shared with outbound PUBLISH serving.
    forwarder: ObjectForwarder,

    /// Tracks whether SubscribeOk was enqueued, which selects PUBLISH_DONE instead
    /// of REQUEST_ERROR for both normal and abnormal termination.
    ok: bool,

    /// Set before awaiting the terminal receipt so cancellation cannot make
    /// Drop enqueue a duplicate terminal message.
    terminal_started: bool,
}

#[derive(Clone, Default)]
pub(super) struct StreamCount(Arc<AtomicU64>);

impl StreamCount {
    fn opened(&self) {
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                count.checked_add(1)
            });
    }

    pub(super) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Failure while serving a track under a pre-acceptance deadline.
#[derive(Debug, thiserror::Error)]
pub enum ServeWithDeadlineError {
    /// The deadline passed before SUBSCRIBE_OK was committed.
    #[error("subscribe deadline expired before acceptance")]
    DeadlineExpired,

    /// Serving failed before or after acceptance for another reason.
    #[error(transparent)]
    Session(#[from] SessionError),
}

/// Serves a track's Objects on a session, keyed by Track Alias.
///
/// SUBSCRIBE-initiated ([`Subscribed`]) and PUBLISH-initiated
/// ([`super::Published`]) subscriptions differ only in how they are set up and
/// torn down; the object-sending loop is identical, so it lives here.
pub(super) struct ObjectForwarder {
    /// The session's Publisher manager, used to send control messages,
    /// create new QUIC streams, and send datagrams.
    publisher: Publisher,

    state: State<ObjectForwarderState>,

    /// Track Alias carried in every subgroup header and datagram we emit.
    track_alias: u64,

    /// Optional mlog writer for logging transport events
    mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,

    stream_count: StreamCount,
}

impl ObjectForwarder {
    pub(super) fn new(
        publisher: Publisher,
        track_alias: u64,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> (Self, ObjectForwarderRecv) {
        let (send, recv) = State::default().split();
        let send = Self {
            publisher,
            state: send,
            track_alias,
            mlog,
            stream_count: StreamCount::default(),
        };
        let recv = ObjectForwarderRecv { state: recv };
        (send, recv)
    }

    pub(super) fn set_largest_location(
        &self,
        largest_location: Option<Location>,
    ) -> Result<(), ServeError> {
        self.state
            .lock_mut()
            .ok_or(ServeError::Cancel)?
            .largest_location = largest_location;
        Ok(())
    }

    /// Commit the decision to accept a SUBSCRIBE while holding the same state
    /// lock used by withdrawal.
    fn prepare_subscribe_ok(
        &self,
        largest_location: Option<Location>,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            return Err(ServeError::Timeout);
        }

        state.into_mut().ok_or(ServeError::Cancel)?.largest_location = largest_location;
        Ok(())
    }

    pub(super) fn stream_count_handle(&self) -> StreamCount {
        self.stream_count.clone()
    }

    /// Snapshot the fields a terminal message needs, without holding the lock
    /// across the send that follows.
    fn terminal_state(&self) -> (ServeError, u64, u64, bool) {
        let state = self.state.lock();
        let err = state
            .closed
            .as_ref()
            .err()
            .cloned()
            .unwrap_or(ServeError::Done);
        (
            err,
            self.stream_count.get(),
            state.retry_interval,
            state.unsubscribed,
        )
    }

    fn close(&self, err: ServeError, retry_interval: u64) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        let mut state = state.into_mut().ok_or(ServeError::Done)?;
        state.closed = Err(err);
        state.retry_interval = retry_interval;

        Ok(())
    }

    pub(super) async fn closed(&self) -> Result<(), ServeError> {
        loop {
            {
                let state = self.state.lock();
                state.closed.clone()?;

                match state.modified() {
                    Some(notify) => notify,
                    None => return Ok(()),
                }
            }
            .await;
        }
    }

    pub(super) async fn serve(
        &mut self,
        track: serve::TrackReader,
        delivery_filter: DeliveryFilter,
    ) -> Result<(), SessionError> {
        match track.mode().await? {
            TrackReaderMode::Stream(_stream) => Err(SessionError::Serve(
                ServeError::not_implemented_ctx("stream track reader mode"),
            )),
            TrackReaderMode::Subgroups(subgroups) => {
                self.serve_subgroups(subgroups, delivery_filter).await
            }
            TrackReaderMode::Datagrams(datagrams) => {
                self.serve_datagrams(datagrams, delivery_filter).await
            }
        }
    }
}

impl Subscribed {
    pub(super) fn new(
        publisher: Publisher,
        msg: message::Subscribe,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(Self, ObjectForwarderRecv), SessionError> {
        let info = SubscribeInfo::new_from_subscribe(&msg)?;
        // The subscription's request ID doubles as its Track Alias.
        let (forwarder, recv) = ObjectForwarder::new(publisher, info.id, mlog);
        let send = Self {
            info,
            forwarder,
            ok: false,
            terminal_started: false,
        };

        Ok((send, recv))
    }

    pub async fn serve(self, track: serve::TrackReader) -> Result<(), SessionError> {
        let largest_location = track.largest_location();
        self.serve_with_largest_location(track, largest_location)
            .await
    }

    /// Serve using the track position captured when the subscription resolved.
    ///
    /// Relays can wait for an upstream subscription after resolving a local
    /// track. Objects arriving during that wait are part of this subscription,
    /// not cached history against which its filter should be evaluated again.
    pub async fn serve_with_largest_location(
        self,
        track: serve::TrackReader,
        largest_location: Option<Location>,
    ) -> Result<(), SessionError> {
        self.serve_before(track, None, largest_location).await
    }

    /// Serve a track only if acceptance is committed before `deadline`.
    pub async fn serve_with_deadline(
        self,
        track: serve::TrackReader,
        deadline: tokio::time::Instant,
    ) -> Result<(), ServeWithDeadlineError> {
        let largest_location = track.largest_location();
        self.serve_with_deadline_and_largest_location(track, deadline, largest_location)
            .await
    }

    pub async fn serve_with_deadline_and_largest_location(
        mut self,
        track: serve::TrackReader,
        deadline: tokio::time::Instant,
        largest_location: Option<Location>,
    ) -> Result<(), ServeWithDeadlineError> {
        let result = self
            .serve_inner(track, Some(deadline), largest_location)
            .await;
        let deadline_expired =
            !self.ok && matches!(result, Err(SessionError::Serve(ServeError::Timeout)));

        if deadline_expired {
            // A simultaneous UNSUBSCRIBE may have closed the shared state first,
            // but the relay still needs the deadline result for its response and metric.
            if let Err(err) = &result {
                let _ = self.forwarder.close(err.clone().into(), 0);
            }
            return Err(ServeWithDeadlineError::DeadlineExpired);
        }

        if let Err(err) = &result {
            let _ = self.forwarder.close(err.clone().into(), 0);
        }
        let terminal = self.finish().await;
        match result {
            Ok(()) => terminal.map_err(ServeWithDeadlineError::Session),
            Err(err) => Err(ServeWithDeadlineError::Session(err)),
        }
    }

    async fn serve_before(
        mut self,
        track: serve::TrackReader,
        deadline: Option<tokio::time::Instant>,
        largest_location: Option<Location>,
    ) -> Result<(), SessionError> {
        let res = self.serve_inner(track, deadline, largest_location).await;
        if let Err(err) = &res {
            let _ = self.forwarder.close(err.clone().into(), 0);
        }
        let terminal = self.finish().await;
        res.and(terminal)
    }

    async fn serve_inner(
        &mut self,
        track: serve::TrackReader,
        deadline: Option<tokio::time::Instant>,
        largest_location: Option<Location>,
    ) -> Result<(), SessionError> {
        self.forwarder
            .prepare_subscribe_ok(largest_location, deadline)?;

        // Wait until SUBSCRIBE_OK is encoded before serving. Otherwise a data
        // stream or an immediate PUBLISH_DONE could overtake acceptance.
        let mut params = KeyValuePairs::default();
        if let Some(largest) = largest_location {
            params
                .set_largest_object(largest)
                .map_err(|_| SessionError::Internal)?;
        }

        let acceptance = self
            .forwarder
            .publisher
            .send_subscribe_ok(message::SubscribeOk {
                id: self.info.id,
                track_alias: self.info.id, // use subscription id as track alias
                params,
                track_extensions: Default::default(),
            })?;

        self.ok = true;
        acceptance
            .await
            .map_err(|_| DeliveryError::Cancelled.into_session_error())?
            .map_err(DeliveryError::into_session_error)?;

        let delivery_filter = self.info.delivery_filter(largest_location);

        self.forwarder.serve(track, delivery_filter).await
    }

    pub fn close(self, err: ServeError) -> Result<(), ServeError> {
        self.close_with_retry(err, 0)
    }

    /// Reject a pending subscription and tell the subscriber when it may retry.
    ///
    /// `retry_interval` uses the REQUEST_ERROR wire encoding: minimum delay in
    /// milliseconds plus one, or zero when the request should not be retried.
    pub fn close_with_retry(self, err: ServeError, retry_interval: u64) -> Result<(), ServeError> {
        self.forwarder.close(err, retry_interval)
    }

    pub async fn closed(&self) -> Result<(), ServeError> {
        self.forwarder.closed().await
    }

    async fn finish(&mut self) -> Result<(), SessionError> {
        let (err, stream_count, _, unsubscribed) = self.forwarder.terminal_state();
        if !self.ok || unsubscribed {
            return Ok(());
        }

        self.terminal_started = true;
        let result = self
            .forwarder
            .publisher
            .send_publish_done_and_wait(message::PublishDone {
                id: self.info.id,
                status_code: Self::publish_done_code(&err),
                stream_count,
                reason: ReasonPhrase(err.to_string()),
            })
            .await;
        self.forwarder.publisher.drop_subscribe(self.info.id);
        result
    }
}

impl ops::Deref for Subscribed {
    type Target = SubscribeInfo;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

impl Drop for Subscribed {
    fn drop(&mut self) {
        let (err, stream_count, retry_interval, unsubscribed) = self.forwarder.terminal_state();

        // Normal completion owns an async receipt. Drop only handles abnormal
        // cancellation before that terminal delivery starts.
        if unsubscribed {
            return;
        }
        if self.terminal_started {
            return;
        }

        if self.ok {
            self.forwarder.publisher.send_message(message::PublishDone {
                id: self.info.id,
                status_code: Self::publish_done_code(&err),
                stream_count,
                reason: ReasonPhrase(err.to_string()),
            });
        } else {
            // Draft-16 §9.8: subscription rejection uses REQUEST_ERROR, not the
            // legacy SUBSCRIBE_ERROR.
            self.forwarder.publisher.send_request_error(
                "subscribe",
                message::RequestError {
                    id: self.info.id,
                    error_code: Self::request_error_code(&err),
                    retry_interval,
                    reason: ReasonPhrase(err.to_string()),
                },
            );
            self.forwarder.publisher.drop_subscribe(self.info.id);
        };
    }
}

impl Subscribed {
    fn publish_done_code(err: &ServeError) -> u64 {
        match err {
            ServeError::Done => message::PublishDoneCode::TrackEnded as u64,
            ServeError::Closed(code) => *code,
            _ => message::PublishDoneCode::InternalError as u64,
        }
    }

    fn request_error_code(err: &ServeError) -> u64 {
        match err {
            ServeError::Closed(code) => *code,
            ServeError::NotFound | ServeError::NotFoundWithId(_, _) => {
                RequestErrorCode::DoesNotExist as u64
            }
            // draft-18 §10.2.6 distinguishes the two: DOES_NOT_EXIST when the relay
            // did not wait, TIMEOUT when it held the subscription and no publisher
            // arrived before the subscriber's RENDEZVOUS_TIMEOUT elapsed.
            ServeError::Timeout => RequestErrorCode::Timeout as u64,
            ServeError::Duplicate => RequestErrorCode::DuplicateSubscription as u64,
            ServeError::Cancel | ServeError::Done => RequestErrorCode::Uninterested as u64,
            ServeError::Mode
            | ServeError::Size
            | ServeError::NotImplemented(_)
            | ServeError::NotImplementedWithId(_, _) => RequestErrorCode::NotSupported as u64,
            ServeError::Internal(_) | ServeError::InternalWithId(_, _) => {
                RequestErrorCode::InternalError as u64
            }
        }
    }

    fn is_expected_serve_shutdown(err: &SessionError) -> bool {
        matches!(
            err,
            SessionError::Serve(ServeError::Cancel | ServeError::Done)
        )
    }

    /// Whether a failed subgroup indicates a fault on this side rather than
    /// the peer or the network ending one data stream.
    ///
    /// A peer may cancel an individual stream (§11.4.1) and the connection may
    /// fail underneath us; neither means this subscription malfunctioned.
    fn is_local_serve_failure(err: &SessionError) -> bool {
        !Self::is_expected_serve_shutdown(err) && !matches!(err, SessionError::WebTransport(_))
    }
}

impl ObjectForwarder {
    async fn serve_subgroups(
        &mut self,
        mut subgroups: serve::SubgroupsReader,
        delivery_filter: DeliveryFilter,
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();
        let mut done: Option<Result<(), ServeError>> = None;
        // A subgroup that could not be sent means the peer did not receive
        // Objects this subscription promised. Reporting `Ok` there would let
        // callers treat a silent delivery failure as a successful transfer.
        //
        // Only local faults count. A peer is entitled to cancel an individual
        // data stream (§11.4.1), and the resulting transport error must not
        // turn an otherwise healthy subscription's PUBLISH_DONE into
        // INTERNAL_ERROR.
        let mut first_failure: Option<SessionError> = None;

        loop {
            tokio::select! {
                res = subgroups.next(), if done.is_none() => match res {
                    Ok(Some(subgroup)) => {
                        let publisher = self.publisher.clone();
                        let state = self.state.clone();
                        let info = subgroup.info.clone();
                        let mlog = self.mlog.clone();
                        let track_alias = self.track_alias;
                        let stream_count = self.stream_count.clone();

                        tasks.push(async move {
                            let res = Self::serve_subgroup(track_alias, subgroup, publisher, state, stream_count, mlog, delivery_filter).await;
                            if let Err(err) = &res {
                                if Subscribed::is_expected_serve_shutdown(err) {
                                    tracing::debug!(subgroup_info = ?info, error = %err, "stopped serving subgroup");
                                } else {
                                    tracing::warn!(subgroup_info = ?info, error = %err, "failed to serve subgroup");
                                }
                            }
                            res
                        });
                    },
                    Ok(None) => done = Some(Ok(())),
                    Err(err) => done = Some(Err(err)),
                },
                res = self.closed(), if done.is_none() || !tasks.is_empty() => return res.map_err(Into::into),
                res = tasks.next(), if !tasks.is_empty() => {
                    // Remaining subgroups still get their chance to send; the
                    // first local failure is reported once they settle.
                    if let Some(Err(err)) = res {
                        if Subscribed::is_local_serve_failure(&err) && first_failure.is_none() {
                            first_failure = Some(err);
                        }
                    }
                },
                // Reached only once both the subgroup source and `closed()` are
                // disabled, which requires `done` to be set.
                else => {
                    // The track's own outcome is the authoritative one; a
                    // per-subgroup fault is only reported when the track itself
                    // ended cleanly, so a specific upstream code is never
                    // replaced by a generic one.
                    done.ok_or(SessionError::Internal)??;
                    return match first_failure {
                        Some(err) => Err(err),
                        None => Ok(()),
                    };
                }
            }
        }
    }

    async fn serve_subgroup(
        track_alias: u64,
        mut subgroup_reader: serve::SubgroupReader,
        mut publisher: Publisher,
        state: State<ObjectForwarderState>,
        stream_count: StreamCount,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
        delivery_filter: DeliveryFilter,
    ) -> Result<(), SessionError> {
        tracing::trace!(
            "[PUBLISHER] serve_subgroup: starting - group_id={}, subgroup_id={:?}, priority={}",
            subgroup_reader.group_id,
            subgroup_reader.subgroup_id,
            subgroup_reader.priority
        );

        let mut writer: Option<ResetOnDropWriter> = None;
        let mut object_count = 0;
        let mut previous_object_id = None;
        let mut filtered_prefix = false;
        let metadata = subgroup_reader.metadata();
        loop {
            let mut subgroup_object_reader = match subgroup_reader.next().await {
                Ok(Some(object)) => object,
                Ok(None) => break,
                Err(err) => {
                    if let Some(writer) = writer.as_mut() {
                        writer.reset(0);
                    }
                    return Err(err.into());
                }
            };
            if !delivery_filter.allows(subgroup_reader.group_id, subgroup_object_reader.object_id) {
                tracing::trace!(
                    "[PUBLISHER] serve_subgroup: filtered object group_id={}, object_id={}",
                    subgroup_reader.group_id,
                    subgroup_object_reader.object_id
                );
                filtered_prefix = true;
                continue;
            }

            if writer.is_none() {
                let header = data::SubgroupHeader {
                    header_type: data::StreamHeaderType::subgroup(
                        data::SubgroupIdMode::Explicit,
                        metadata.has_properties,
                        metadata.end_of_group,
                        false,
                        metadata.first_object && !filtered_prefix,
                    ),
                    track_alias,
                    group_id: subgroup_reader.group_id,
                    subgroup_id: Some(subgroup_reader.subgroup_id),
                    publisher_priority: subgroup_reader.priority,
                };
                let mut send_stream = publisher.open_uni().await?;
                tracing::trace!("[PUBLISHER] serve_subgroup: opened unidirectional stream");

                // TODO figure out u32 vs u64 priority
                send_stream.set_priority(subgroup_reader.priority as i32);
                let mut new_writer = ResetOnDropWriter::new(Writer::new(send_stream));
                stream_count.opened();

                {
                    let locked = state.lock();
                    let closed = locked.closed.clone();
                    closed?;
                }

                tracing::trace!(
                    "[PUBLISHER] serve_subgroup: sending header - track_alias={}, group_id={}, subgroup_id={:?}, priority={}, header_type={:?}",
                    header.track_alias,
                    header.group_id,
                    header.subgroup_id,
                    header.publisher_priority,
                    header.header_type
                );

                new_writer.encode(&header).await?;

                // Log subgroup header created/sent
                if let Some(ref mlog) = mlog {
                    if let Ok(mut mlog_guard) = mlog.lock() {
                        let time = mlog_guard.elapsed_ms();
                        let stream_id = 0; // TODO: Placeholder, need actual QUIC stream ID
                        let event = mlog::subgroup_header_created(time, stream_id, &header);
                        let _ = mlog_guard.add_event(event);
                    }
                }

                writer = Some(new_writer);
            }

            let writer = writer.as_mut().ok_or(SessionError::Internal)?;
            let object_id_delta = data::encode_object_id_delta(
                &mut previous_object_id,
                subgroup_object_reader.object_id,
            )
            .map_err(|err| SessionError::Serve(ServeError::internal_ctx(err.to_string())))?;
            let status = if subgroup_object_reader.size == 0 {
                Some(subgroup_object_reader.status)
            } else {
                None
            };

            tracing::trace!(
                "[PUBLISHER] serve_subgroup: sending object #{} - object_id={}, object_id_delta={}, payload_length={}, status={:?}, extension_headers={:?}",
                object_count + 1,
                subgroup_object_reader.object_id,
                object_id_delta,
                subgroup_object_reader.size,
                status,
                subgroup_object_reader.extension_headers
            );

            if metadata.has_properties {
                let subgroup_object = data::SubgroupObjectExt {
                    object_id_delta,
                    extension_headers: subgroup_object_reader.extension_headers.clone(),
                    payload_length: subgroup_object_reader.size,
                    status,
                };
                writer.encode(&subgroup_object).await?;

                if let Some(ref mlog) = mlog {
                    if let Ok(mut mlog_guard) = mlog.lock() {
                        let time = mlog_guard.elapsed_ms();
                        let stream_id = 0; // TODO: Placeholder, need actual QUIC stream ID
                        let event = mlog::subgroup_object_ext_created(
                            time,
                            stream_id,
                            subgroup_reader.group_id,
                            subgroup_reader.subgroup_id,
                            subgroup_object_reader.object_id,
                            &subgroup_object,
                        );
                        let _ = mlog_guard.add_event(event);
                    }
                }
            } else {
                if !subgroup_object_reader.extension_headers.is_empty() {
                    writer.reset(0);
                    return Err(ServeError::internal_ctx(
                        "subgroup Object has properties without the PROPERTIES header bit",
                    )
                    .into());
                }
                let subgroup_object = data::SubgroupObject {
                    object_id_delta,
                    payload_length: subgroup_object_reader.size,
                    status,
                };
                writer.encode(&subgroup_object).await?;

                if let Some(ref mlog) = mlog {
                    if let Ok(mut mlog_guard) = mlog.lock() {
                        let time = mlog_guard.elapsed_ms();
                        let stream_id = 0; // TODO: Placeholder, need actual QUIC stream ID
                        let event = mlog::subgroup_object_created(
                            time,
                            stream_id,
                            subgroup_reader.group_id,
                            subgroup_reader.subgroup_id,
                            subgroup_object_reader.object_id,
                            &subgroup_object,
                        );
                        let _ = mlog_guard.add_event(event);
                    }
                }
            }

            state
                .lock_mut()
                .ok_or(ServeError::Done)?
                .update_largest_location(
                    subgroup_reader.group_id,
                    subgroup_object_reader.object_id,
                )?;

            let mut chunks_sent = 0;
            let mut bytes_sent = 0;
            loop {
                let chunk = match subgroup_object_reader.read().await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(err) => {
                        writer.reset(0);
                        return Err(err.into());
                    }
                };
                tracing::trace!(
                    "[PUBLISHER] serve_subgroup: sending payload chunk #{} for object #{} ({} bytes)",
                    chunks_sent + 1,
                    object_count + 1,
                    chunk.len()
                );
                bytes_sent += chunk.len();
                writer.write(&chunk).await?;
                chunks_sent += 1;
            }

            tracing::trace!(
                "[PUBLISHER] serve_subgroup: completed object #{} ({} chunks, {} bytes total)",
                object_count + 1,
                chunks_sent,
                bytes_sent
            );
            object_count += 1;
        }

        tracing::trace!(
            "[PUBLISHER] serve_subgroup: completed subgroup (group_id={}, subgroup_id={:?}, {} objects sent)",
            subgroup_reader.group_id,
            subgroup_reader.subgroup_id,
            object_count
        );

        if let Some(writer) = writer.as_mut() {
            writer.finish()?;
        }

        Ok(())
    }

    async fn serve_datagrams(
        &mut self,
        mut datagrams: serve::DatagramsReader,
        delivery_filter: DeliveryFilter,
    ) -> Result<(), SessionError> {
        tracing::debug!("[PUBLISHER] serve_datagrams: starting");

        let mut datagram_count = 0;
        while let Some(datagram) = datagrams.read().await? {
            if !delivery_filter.allows(datagram.group_id, datagram.object_id) {
                tracing::trace!(
                    "[PUBLISHER] serve_datagrams: filtered datagram group_id={}, object_id={}",
                    datagram.group_id,
                    datagram.object_id
                );
                continue;
            }

            let has_extension_headers = !datagram.extension_headers.is_empty();
            if datagram.status != data::ObjectStatus::NormalObject && !datagram.payload.is_empty() {
                return Err(EncodeError::InvalidValue.into());
            }
            let has_status =
                datagram.status != data::ObjectStatus::NormalObject || datagram.payload.is_empty();
            let datagram_type = match (has_status, has_extension_headers, datagram.end_of_group) {
                (true, _, true) => return Err(EncodeError::InvalidValue.into()),
                (true, true, false) => data::DatagramType::ObjectIdStatusExt,
                (true, false, false) => data::DatagramType::ObjectIdStatus,
                (false, true, true) => data::DatagramType::ObjectIdPayloadExtEndOfGroup,
                (false, false, true) => data::DatagramType::ObjectIdPayloadEndOfGroup,
                (false, true, false) => data::DatagramType::ObjectIdPayloadExt,
                (false, false, false) => data::DatagramType::ObjectIdPayload,
            };

            // Bound locally so the logging and largest-location updates below
            // read it directly instead of unwrapping the Option again.
            let object_id = datagram.object_id;
            let encoded_datagram = data::Datagram {
                datagram_type,
                track_alias: self.track_alias,
                group_id: datagram.group_id,
                object_id: Some(object_id),
                publisher_priority: Some(datagram.priority),
                extension_headers: if has_extension_headers {
                    Some(datagram.extension_headers.clone())
                } else {
                    None
                },
                status: has_status.then_some(datagram.status),
                payload: (!has_status).then_some(datagram.payload),
            };

            let payload_len = encoded_datagram
                .payload
                .as_ref()
                .map(|p| p.len())
                .unwrap_or(0);
            let mut buffer = bytes::BytesMut::with_capacity(payload_len + 100);
            encoded_datagram.encode(&mut buffer)?;

            tracing::trace!(
                "[PUBLISHER] serve_datagrams: sending datagram #{} - track_alias={}, group_id={}, object_id={}, priority={:?}, payload_len={}, extension_headers={:?}, total_encoded_len={}",
                datagram_count + 1,
                encoded_datagram.track_alias,
                encoded_datagram.group_id,
                object_id,
                encoded_datagram.publisher_priority,
                payload_len,
                encoded_datagram.extension_headers,
                buffer.len()
            );

            // Create mlog event for datagram created
            if let Some(ref mlog) = self.mlog {
                if let Ok(mut mlog_guard) = mlog.lock() {
                    let time = mlog_guard.elapsed_ms();
                    let stream_id = 0; // TODO: Placeholder, need actual QUIC stream ID
                    let _ = mlog_guard.add_event(mlog::object_datagram_created(
                        time,
                        stream_id,
                        &encoded_datagram,
                    ));
                }
            }

            self.publisher.send_datagram(buffer.into()).await?;

            if datagram.status == data::ObjectStatus::NormalObject {
                self.state
                    .lock_mut()
                    .ok_or(ServeError::Done)?
                    .update_largest_location(encoded_datagram.group_id, object_id)?;
            }

            datagram_count += 1;
        }

        tracing::trace!(
            "[PUBLISHER] serve_datagrams: completed ({} datagrams sent)",
            datagram_count
        );

        Ok(())
    }
}

pub(super) struct ObjectForwarderRecv {
    state: State<ObjectForwarderState>,
}

impl ObjectForwarderRecv {
    pub fn recv_unsubscribe(&mut self) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        if let Some(mut state) = state.into_mut() {
            state.unsubscribed = true;
            state.closed = Err(ServeError::Cancel);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding::Decode;
    use crate::session::test_support::{loopback_raw_session_pair, loopback_session_pair};
    use crate::session::Reader;
    use bytes::Bytes;

    async fn test_subscribed() -> (
        Subscribed,
        ObjectForwarderRecv,
        crate::watch::Queue<message::Message>,
        tokio::sync::mpsc::UnboundedReceiver<crate::session::BidiResponse>,
    ) {
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let (outgoing, outgoing_recv) = crate::watch::Queue::default().split();
        let publisher = Publisher::new(
            outgoing,
            crate::session::test_support::loopback_session().await,
            None,
            crate::session::RequestId::new(0, 1),
            bidi_task_tx,
        );
        let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel();
        publisher
            .bidi_response_map
            .lock()
            .unwrap()
            .insert(0, response_tx);
        let (subscribed, recv) = Subscribed::new(
            publisher,
            message::Subscribe {
                id: 0,
                track_namespace: crate::coding::TrackNamespace::from_utf8_path("test"),
                track_name: "track".into(),
                params: KeyValuePairs::default(),
            },
            None,
        )
        .unwrap();
        (subscribed, recv, outgoing_recv, response_rx)
    }

    async fn accept_subscription(
        responses: &mut tokio::sync::mpsc::UnboundedReceiver<crate::session::BidiResponse>,
    ) {
        let mut response = responses.recv().await.unwrap();
        assert!(matches!(response.message, message::Message::SubscribeOk(_)));
        response.complete(Ok(()));
    }

    async fn forward_datagram(
        publisher_session: web_transport::Session,
        peer_session: web_transport::Session,
        datagram: serve::Datagram,
    ) -> (data::Datagram, Option<Location>) {
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            crate::session::RequestId::new(0, 1),
            bidi_task_tx,
        );
        let (mut forwarder, _recv) = ObjectForwarder::new(publisher, 42, None);
        let (writer, reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "datagram",
        )
        .produce();
        let mut writer = writer.datagrams().unwrap();
        writer.write(datagram).unwrap();
        drop(writer);
        let serve::TrackReaderMode::Datagrams(reader) = reader.mode().await.unwrap() else {
            panic!("expected datagram mode");
        };

        let send = forwarder.serve_datagrams(
            reader,
            DeliveryFilter {
                forward: true,
                start_location: None,
                end_group_id: None,
            },
        );
        let receive = async move {
            let mut encoded = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                peer_session.recv_datagram(),
            )
            .await
            .unwrap()
            .unwrap();
            data::Datagram::decode(&mut encoded).unwrap()
        };
        let (send, received) = tokio::join!(send, receive);
        send.unwrap();
        let largest_location = forwarder.state.lock().largest_location;
        (received, largest_location)
    }

    fn payload_datagram() -> serve::Datagram {
        let mut properties = data::ExtensionHeaders::new();
        properties.set_intvalue(2, 7);
        serve::Datagram {
            group_id: 3,
            object_id: 0,
            priority: 200,
            status: data::ObjectStatus::NormalObject,
            end_of_group: true,
            payload: Bytes::from_static(b"payload"),
            extension_headers: properties,
        }
    }

    fn closed_subgroup_track(stream_count: u64) -> serve::TrackReader {
        let (writer, reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "track",
        )
        .produce();
        let mut subgroups = writer.subgroups().unwrap();
        for group_id in 0..stream_count {
            let mut subgroup = subgroups
                .create(serve::Subgroup {
                    group_id,
                    subgroup_id: 0,
                    priority: 128,
                })
                .unwrap();
            let mut object = subgroup.create_with_id(0, 1, None).unwrap();
            object.write(Bytes::from_static(b"x")).unwrap();
        }
        drop(subgroups);
        reader
    }

    fn assert_payload_datagram((received, largest): (data::Datagram, Option<Location>)) {
        assert_eq!(
            received.datagram_type,
            data::DatagramType::ObjectIdPayloadExtEndOfGroup
        );
        assert_eq!(received.track_alias, 42);
        assert_eq!(received.group_id, 3);
        assert_eq!(received.object_id, Some(0));
        assert_eq!(received.publisher_priority, Some(200));
        assert_eq!(received.status, None);
        assert_eq!(received.payload, Some(Bytes::from_static(b"payload")));
        assert!(received.extension_headers.unwrap().has(2));
        assert_eq!(largest, Some(Location::new(3, 0)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_datagram_semantics_over_webtransport() {
        let (publisher, peer) = loopback_session_pair().await;
        assert_payload_datagram(forward_datagram(publisher, peer, payload_datagram()).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_datagram_semantics_over_raw_quic() {
        let (publisher, peer) = loopback_raw_session_pair().await;
        assert_payload_datagram(forward_datagram(publisher, peer, payload_datagram()).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_datagram_status_without_payload() {
        let (publisher, peer) = loopback_session_pair().await;
        let (received, largest) = forward_datagram(
            publisher,
            peer,
            serve::Datagram {
                group_id: 8,
                object_id: 9,
                priority: 201,
                status: data::ObjectStatus::EndOfTrack,
                end_of_group: false,
                payload: Bytes::new(),
                extension_headers: data::ExtensionHeaders::default(),
            },
        )
        .await;
        assert_eq!(received.datagram_type, data::DatagramType::ObjectIdStatus);
        assert_eq!(received.object_id, Some(9));
        assert_eq!(received.publisher_priority, Some(201));
        assert_eq!(received.status, Some(data::ObjectStatus::EndOfTrack));
        assert_eq!(received.payload, None);
        assert_eq!(largest, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscription_filter_uses_pre_wait_track_position() {
        let (publisher_session, peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            crate::session::RequestId::new(0, 1),
            bidi_task_tx,
        );
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel();
        publisher
            .bidi_response_map
            .lock()
            .unwrap()
            .insert(0, response_tx);
        let mut params = KeyValuePairs::default();
        params
            .set_subscription_filter(&message::SubscriptionFilter {
                filter_type: message::FilterType::NextGroupStart,
                start_location: None,
                end_group_id: None,
            })
            .unwrap();
        let (subscribed, _recv) = Subscribed::new(
            publisher,
            message::Subscribe {
                id: 0,
                track_namespace: crate::coding::TrackNamespace::from_utf8_path("test"),
                track_name: "datagram".into(),
                params,
            },
            None,
        )
        .unwrap();
        let (writer, reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "datagram",
        )
        .produce();
        let mut writer = writer.datagrams().unwrap();
        writer.write(payload_datagram()).unwrap();
        drop(writer);

        let serve = subscribed.serve_with_largest_location(reader, None);
        let accept = async move {
            accept_subscription(&mut response_rx).await;
            let mut response = response_rx.recv().await.unwrap();
            assert!(matches!(
                response.message,
                message::Message::PublishDone(message::PublishDone {
                    stream_count: 0,
                    ..
                })
            ));
            response.complete(Ok(()));
        };
        let receive = async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                peer_session.recv_datagram(),
            )
            .await
            .unwrap()
            .unwrap()
        };
        let (serve, _, mut received) = tokio::join!(serve, accept, receive);
        serve.unwrap();
        let received = data::Datagram::decode(&mut received).unwrap();
        assert_eq!(received.group_id, 3);
        assert_eq!(received.object_id, Some(0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_status_datagram_with_end_of_group_bit() {
        let (publisher_session, _peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            crate::session::RequestId::new(0, 1),
            bidi_task_tx,
        );
        let (mut forwarder, _recv) = ObjectForwarder::new(publisher, 42, None);
        let (writer, reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "datagram",
        )
        .produce();
        let mut writer = writer.datagrams().unwrap();
        writer
            .write(serve::Datagram {
                group_id: 1,
                object_id: 2,
                priority: 200,
                status: data::ObjectStatus::EndOfTrack,
                end_of_group: true,
                payload: Bytes::new(),
                extension_headers: data::ExtensionHeaders::default(),
            })
            .unwrap();
        drop(writer);
        let serve::TrackReaderMode::Datagrams(reader) = reader.mode().await.unwrap() else {
            panic!("expected datagram mode");
        };

        assert!(matches!(
            forwarder
                .serve_datagrams(
                    reader,
                    DeliveryFilter {
                        forward: true,
                        start_location: None,
                        end_group_id: None,
                    },
                )
                .await,
            Err(SessionError::Encode(EncodeError::InvalidValue))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_non_normal_status_with_payload() {
        let (publisher_session, _peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            crate::session::RequestId::new(0, 1),
            bidi_task_tx,
        );
        let (mut forwarder, _recv) = ObjectForwarder::new(publisher, 42, None);
        let (writer, reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "datagram",
        )
        .produce();
        let mut writer = writer.datagrams().unwrap();
        writer
            .write(serve::Datagram {
                group_id: 1,
                object_id: 2,
                priority: 200,
                status: data::ObjectStatus::EndOfTrack,
                end_of_group: false,
                payload: Bytes::from_static(b"invalid"),
                extension_headers: data::ExtensionHeaders::default(),
            })
            .unwrap();
        drop(writer);
        let serve::TrackReaderMode::Datagrams(reader) = reader.mode().await.unwrap() else {
            panic!("expected datagram mode");
        };

        assert!(matches!(
            forwarder
                .serve_datagrams(
                    reader,
                    DeliveryFilter {
                        forward: true,
                        start_location: None,
                        end_group_id: None,
                    },
                )
                .await,
            Err(SessionError::Encode(EncodeError::InvalidValue))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn filtered_subgroup_deltas_follow_transmitted_object_ids() {
        let (publisher_session, peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            crate::session::RequestId::new(0, 1),
            bidi_task_tx,
        );
        let (forwarder, _recv) = ObjectForwarder::new(publisher, 42, None);

        let track = Arc::new(serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "track",
        ));
        let (mut source, source_reader) = serve::SubgroupInfo {
            track,
            group_id: 0,
            subgroup_id: 0,
            priority: 128,
        }
        .produce();
        for object_id in [0, 1, 5, 6, 9] {
            let extension_headers = if object_id == 5 {
                let mut headers = data::ExtensionHeaders::new();
                headers.set_intvalue(2, 9);
                Some(headers)
            } else {
                None
            };
            let mut object = source
                .create_with_id(object_id, 1, extension_headers)
                .unwrap();
            object.write(Bytes::from(vec![object_id as u8])).unwrap();
        }
        drop(source);

        let filter = DeliveryFilter {
            forward: true,
            start_location: Some(Location::new(0, 5)),
            end_group_id: None,
        };

        let receive = async move {
            let stream = peer_session.accept_uni().await.unwrap();
            let mut reader = Reader::new(stream);
            let stream_header: data::StreamHeader = reader.decode().await.unwrap();
            let header = stream_header.subgroup_header.unwrap();
            assert_eq!(header.track_alias, 42);
            assert!(header.header_type.has_properties());

            let mut received = Vec::new();
            for expected_payload in [5, 6, 9] {
                let object: data::SubgroupObjectExt = reader.decode().await.unwrap();
                if expected_payload == 5 {
                    assert!(object.extension_headers.has(2));
                }
                let payload = reader
                    .read_chunk(object.payload_length)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(payload.as_ref(), &[expected_payload]);
                received.push(object.object_id_delta);
            }
            received
        };
        let send = ObjectForwarder::serve_subgroup(
            42,
            source_reader,
            forwarder.publisher.clone(),
            forwarder.state.clone(),
            forwarder.stream_count.clone(),
            None,
            filter,
        );

        let (send_result, received_deltas) = tokio::join!(send, receive);
        send_result.unwrap();
        assert_eq!(received_deltas, [5, 0, 2]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_subgroup_semantics_and_clears_first_object_after_filtering() {
        for (start_location, expected_first_object) in
            [(None, true), (Some(Location::new(0, 5)), false)]
        {
            let (publisher_session, peer_session) = loopback_session_pair().await;
            let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
            let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
            let publisher = Publisher::new(
                outgoing,
                publisher_session,
                None,
                crate::session::RequestId::new(0, 1),
                bidi_task_tx,
            );
            let (forwarder, _recv) = ObjectForwarder::new(publisher, 42, None);

            let track = Arc::new(serve::Track::new(
                crate::coding::TrackNamespace::from_utf8_path("test"),
                "track",
            ));
            let (mut source, source_reader) = serve::SubgroupInfo {
                track,
                group_id: 0,
                subgroup_id: 0,
                priority: 200,
            }
            .produce_with_metadata(serve::SubgroupStreamMetadata {
                has_properties: false,
                end_of_group: true,
                first_object: true,
            });
            for object_id in [0, 5] {
                let mut object = source.create_with_id(object_id, 1, None).unwrap();
                object.write(Bytes::from(vec![object_id as u8])).unwrap();
            }
            drop(source);

            let filter = DeliveryFilter {
                forward: true,
                start_location,
                end_group_id: None,
            };
            let receive = async move {
                let stream = peer_session.accept_uni().await.unwrap();
                let mut reader = Reader::new(stream);
                let stream_header: data::StreamHeader = reader.decode().await.unwrap();
                let header = stream_header.subgroup_header.unwrap();
                assert_eq!(
                    header.header_type.subgroup_id_mode(),
                    Some(data::SubgroupIdMode::Explicit)
                );
                assert!(!header.header_type.has_properties());
                assert!(header.header_type.end_of_group());
                assert_eq!(
                    header.header_type.begins_with_first_object(),
                    expected_first_object
                );
                assert!(!header.header_type.uses_default_priority());
                assert_eq!(header.publisher_priority, 200);

                let expected_ids: &[u64] = if expected_first_object { &[0, 5] } else { &[5] };
                let mut previous = None;
                for expected_id in expected_ids {
                    let object: data::SubgroupObject = reader.decode().await.unwrap();
                    let actual_id =
                        data::decode_object_id_delta(&mut previous, object.object_id_delta)
                            .unwrap();
                    assert_eq!(actual_id, *expected_id);
                    assert_eq!(
                        reader
                            .read_chunk(object.payload_length)
                            .await
                            .unwrap()
                            .unwrap()
                            .as_ref(),
                        &[*expected_id as u8]
                    );
                }
                assert!(reader.done().await.unwrap());
            };
            let send = ObjectForwarder::serve_subgroup(
                42,
                source_reader,
                forwarder.publisher.clone(),
                forwarder.state.clone(),
                forwarder.stream_count.clone(),
                None,
                filter,
            );

            let (send_result, ()) = tokio::join!(send, receive);
            send_result.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn eog_subgroup_source_error_resets_downstream_instead_of_finishing() {
        let (publisher_session, peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            crate::session::RequestId::new(0, 1),
            bidi_task_tx,
        );
        let (forwarder, _recv) = ObjectForwarder::new(publisher, 42, None);
        let track = Arc::new(serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "track",
        ));
        let (mut source, source_reader) = serve::SubgroupInfo {
            track,
            group_id: 0,
            subgroup_id: 0,
            priority: 200,
        }
        .produce_with_metadata(serve::SubgroupStreamMetadata {
            has_properties: false,
            end_of_group: true,
            first_object: true,
        });
        let mut object = source.create_with_id(0, 1, None).unwrap();
        object.write(Bytes::from_static(b"x")).unwrap();
        drop(object);
        let (object_received, wait_for_object) = tokio::sync::oneshot::channel();

        let receive = tokio::time::timeout(std::time::Duration::from_secs(5), async move {
            let stream = peer_session.accept_uni().await.unwrap();
            let mut reader = Reader::new(stream);
            let header: data::StreamHeader = reader.decode().await.unwrap();
            assert!(header.header_type.end_of_group());
            let object: data::SubgroupObject = reader.decode().await.unwrap();
            assert_eq!(
                reader
                    .read_chunk(object.payload_length)
                    .await
                    .unwrap()
                    .unwrap(),
                b"x"[..]
            );
            object_received.send(()).unwrap();
            assert!(matches!(
                reader.done().await,
                Err(SessionError::WebTransport(web_transport::Error::Read(
                    web_transport::quinn::ReadError::Reset(0)
                )))
            ));
        });
        let close_source = async move {
            wait_for_object.await.unwrap();
            source.close(ServeError::internal_ctx("upstream reset"))
        };
        let send = ObjectForwarder::serve_subgroup(
            42,
            source_reader,
            forwarder.publisher.clone(),
            forwarder.state.clone(),
            forwarder.stream_count.clone(),
            None,
            DeliveryFilter {
                forward: true,
                start_location: None,
                end_group_id: None,
            },
        );

        let (send_result, receive_result, close_result) = tokio::join!(send, receive, close_source);
        close_result.unwrap();
        assert!(
            receive_result.is_ok(),
            "receive failed: {receive_result:?}; send result: {send_result:?}"
        );
        assert!(send_result.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsubscribe_cancels_active_subgroup_and_resets_downstream() {
        let (publisher_session, peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            crate::session::RequestId::new(0, 1),
            bidi_task_tx,
        );
        let (mut forwarder, mut forwarder_recv) = ObjectForwarder::new(publisher, 42, None);

        let (track_writer, track_reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "track",
        )
        .produce();
        let mut subgroups_writer = track_writer.subgroups().unwrap();
        let mut subgroup_writer = subgroups_writer
            .create(serve::Subgroup {
                group_id: 0,
                subgroup_id: 0,
                priority: 128,
            })
            .unwrap();
        let mut object = subgroup_writer.create_with_id(0, 1, None).unwrap();
        object.write(Bytes::from_static(b"x")).unwrap();
        drop(object);
        let serve::TrackReaderMode::Subgroups(subgroups_reader) =
            track_reader.mode().await.unwrap()
        else {
            panic!("expected subgroup delivery");
        };
        let (object_received, cancel_after_object) = tokio::sync::oneshot::channel();

        let serve = forwarder.serve_subgroups(
            subgroups_reader,
            DeliveryFilter {
                forward: true,
                start_location: None,
                end_group_id: None,
            },
        );
        let receive = async move {
            let stream = peer_session.accept_uni().await.unwrap();
            let mut reader = Reader::new(stream);
            let header: data::StreamHeader = reader.decode().await.unwrap();
            assert!(header.header_type.begins_with_first_object());
            let object: data::SubgroupObjectExt = reader.decode().await.unwrap();
            assert_eq!(
                reader
                    .read_chunk(object.payload_length)
                    .await
                    .unwrap()
                    .unwrap(),
                b"x"[..]
            );
            object_received.send(()).unwrap();
            assert!(matches!(
                reader.done().await,
                Err(SessionError::WebTransport(web_transport::Error::Read(
                    web_transport::quinn::ReadError::Reset(0)
                )))
            ));
        };
        let cancel = async move {
            cancel_after_object.await.unwrap();
            forwarder_recv.recv_unsubscribe()
        };

        let (serve_result, (), cancel_result) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(serve, receive, cancel)
            })
            .await
            .unwrap();
        cancel_result.unwrap();
        assert!(matches!(
            serve_result,
            Err(SessionError::Serve(ServeError::Cancel))
        ));
        drop(subgroup_writer);
        drop(subgroups_writer);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_track_error_drains_active_subgroup_before_returning() {
        let (publisher_session, peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            crate::session::RequestId::new(0, 1),
            bidi_task_tx,
        );
        let (mut forwarder, _forwarder_recv) = ObjectForwarder::new(publisher, 42, None);

        let (track_writer, track_reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "track",
        )
        .produce();
        let mut subgroups_writer = track_writer.subgroups().unwrap();
        let mut subgroup_writer = subgroups_writer
            .create(serve::Subgroup {
                group_id: 0,
                subgroup_id: 0,
                priority: 128,
            })
            .unwrap();
        let mut object = subgroup_writer.create_with_id(0, 1, None).unwrap();
        object.write(Bytes::from_static(b"x")).unwrap();
        drop(object);
        let serve::TrackReaderMode::Subgroups(subgroups_reader) =
            track_reader.mode().await.unwrap()
        else {
            panic!("expected subgroup delivery");
        };
        let (object_received, close_after_object) = tokio::sync::oneshot::channel();

        let serve = forwarder.serve_subgroups(
            subgroups_reader,
            DeliveryFilter {
                forward: true,
                start_location: None,
                end_group_id: None,
            },
        );
        let receive = async move {
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
            object_received.send(()).unwrap();
            assert!(reader.done().await.unwrap());
        };
        let close_source = async move {
            close_after_object.await.unwrap();
            subgroups_writer.close(ServeError::Closed(0x42)).unwrap();
            drop(subgroup_writer);
        };

        let (serve_result, (), ()) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(serve, receive, close_source)
            })
            .await
            .unwrap();
        assert!(matches!(
            serve_result,
            Err(SessionError::Serve(ServeError::Closed(0x42)))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_request_owner_overrides_source_drain_and_resets_subgroup() {
        let (publisher_session, peer_session) = loopback_session_pair().await;
        let (outgoing, _outgoing_recv) = crate::watch::Queue::default().split();
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = Publisher::new(
            outgoing,
            publisher_session,
            None,
            crate::session::RequestId::new(0, 1),
            bidi_task_tx,
        );
        let (mut forwarder, forwarder_recv) = ObjectForwarder::new(publisher, 42, None);
        let (track_writer, track_reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "track",
        )
        .produce();
        let mut subgroups_writer = track_writer.subgroups().unwrap();
        let mut subgroup_writer = subgroups_writer
            .create(serve::Subgroup {
                group_id: 0,
                subgroup_id: 0,
                priority: 128,
            })
            .unwrap();
        let mut object = subgroup_writer.create_with_id(0, 1, None).unwrap();
        object.write(Bytes::from_static(b"x")).unwrap();
        drop(object);
        let serve::TrackReaderMode::Subgroups(subgroups_reader) =
            track_reader.mode().await.unwrap()
        else {
            panic!("expected subgroup delivery");
        };
        let (object_received, cancel_after_object) = tokio::sync::oneshot::channel();

        let serve = forwarder.serve_subgroups(
            subgroups_reader,
            DeliveryFilter {
                forward: true,
                start_location: None,
                end_group_id: None,
            },
        );
        let receive = async move {
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
            object_received.send(()).unwrap();
            assert!(matches!(
                reader.done().await,
                Err(SessionError::WebTransport(web_transport::Error::Read(
                    web_transport::quinn::ReadError::Reset(0)
                )))
            ));
        };
        let cancel = async move {
            cancel_after_object.await.unwrap();
            subgroups_writer.close(ServeError::Closed(0x42)).unwrap();
            drop(forwarder_recv);
        };

        let (serve_result, (), ()) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(serve, receive, cancel)
            })
            .await
            .unwrap();
        serve_result.unwrap();
        drop(subgroup_writer);
    }

    #[tokio::test]
    async fn deadline_prevents_subscribe_ok() {
        let (subscribed, _recv, _outgoing, _responses) = test_subscribed().await;
        let (_writer, reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "track",
        )
        .produce();

        let error = subscribed
            .serve_with_deadline(reader, tokio::time::Instant::now())
            .await
            .unwrap_err();

        assert!(matches!(error, ServeWithDeadlineError::DeadlineExpired));
    }

    #[tokio::test]
    async fn withdrawal_prevents_subscribe_ok() {
        let (subscribed, mut recv, _outgoing, _responses) = test_subscribed().await;
        recv.recv_unsubscribe().unwrap();
        let (_writer, reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "track",
        )
        .produce();

        let error = subscribed
            .serve_with_deadline(
                reader,
                tokio::time::Instant::now() + std::time::Duration::from_secs(1),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ServeWithDeadlineError::Session(SessionError::Serve(ServeError::Cancel))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_waits_for_terminal_receipt_with_exact_zero_and_two_stream_counts() {
        for expected_stream_count in [0, 2] {
            let (subscribed, _recv, _outgoing, mut responses) = test_subscribed().await;
            let stream_count = subscribed.forwarder.stream_count_handle();
            let (writer, reader) = serve::Track::new(
                crate::coding::TrackNamespace::from_utf8_path("test"),
                "track",
            )
            .produce();
            let mut subgroups = writer.subgroups().unwrap();
            let serve = tokio::spawn(subscribed.serve(reader));

            accept_subscription(&mut responses).await;
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
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while stream_count.get() != group_id + 1 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            }
            drop(subgroups);
            let mut response =
                tokio::time::timeout(std::time::Duration::from_secs(2), responses.recv())
                    .await
                    .unwrap()
                    .unwrap();
            let message::Message::PublishDone(done) = &response.message else {
                panic!("expected PUBLISH_DONE, got {:?}", response.message);
            };
            assert_eq!(done.stream_count, expected_stream_count);
            assert!(!serve.is_finished());

            response.complete(Ok(()));
            serve.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn immediate_driver_cancellation_fails_terminal_delivery() {
        let (subscribed, _recv, mut outgoing, mut responses) = test_subscribed().await;
        let serve = tokio::spawn(subscribed.serve(closed_subgroup_track(0)));

        accept_subscription(&mut responses).await;
        drop(responses);

        assert!(matches!(
            serve.await.unwrap(),
            Err(SessionError::Serve(ServeError::Cancel))
        ));
        if let Ok(Some(message)) =
            tokio::time::timeout(std::time::Duration::from_millis(20), outgoing.pop()).await
        {
            panic!("unexpected terminal queue message: {message:?}");
        }
    }

    #[tokio::test]
    async fn cancellation_after_subscribe_ok_enqueue_does_not_reject_accepted_request() {
        let (subscribed, _recv, mut outgoing, mut responses) = test_subscribed().await;
        let _publisher = subscribed.forwarder.publisher.clone();
        let serve = tokio::spawn(subscribed.serve(closed_subgroup_track(0)));
        let response = responses.recv().await.unwrap();
        assert!(matches!(response.message, message::Message::SubscribeOk(_)));

        serve.abort();
        assert!(serve.await.unwrap_err().is_cancelled());
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), outgoing.pop())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(terminal, message::Message::PublishDone(_)));
        drop(response);
    }

    #[tokio::test]
    async fn source_failure_is_reported_after_publish_done_receipt() {
        let (subscribed, _recv, _outgoing, mut responses) = test_subscribed().await;
        let (writer, reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "track",
        )
        .produce();
        writer
            .subgroups()
            .unwrap()
            .close(ServeError::Closed(0x42))
            .unwrap();
        let serve = tokio::spawn(subscribed.serve(reader));

        accept_subscription(&mut responses).await;
        let mut response = responses.recv().await.unwrap();
        let message::Message::PublishDone(done) = &response.message else {
            panic!("expected PUBLISH_DONE, got {:?}", response.message);
        };
        assert_eq!(done.status_code, 0x42);
        assert_eq!(done.stream_count, 0);
        assert!(!serve.is_finished());

        response.complete(Ok(()));
        assert!(matches!(
            serve.await.unwrap(),
            Err(SessionError::Serve(ServeError::Closed(0x42)))
        ));
    }

    #[tokio::test]
    async fn peer_cancellation_suppresses_publish_done() {
        let (subscribed, mut recv, _outgoing, mut responses) = test_subscribed().await;
        let (writer, reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "track",
        )
        .produce();
        let mut subgroups = writer.subgroups().unwrap();
        let mut subgroup = subgroups
            .create(serve::Subgroup {
                group_id: 0,
                subgroup_id: 0,
                priority: 128,
            })
            .unwrap();
        subgroup.write(Bytes::from_static(b"x")).unwrap();
        let serve = tokio::spawn(subscribed.serve(reader));

        accept_subscription(&mut responses).await;
        recv.recv_unsubscribe().unwrap();
        assert!(matches!(
            serve.await.unwrap(),
            Err(SessionError::Serve(ServeError::Cancel))
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), responses.recv())
                .await
                .unwrap()
                .is_none()
        );
        drop(subgroup);
        drop(subgroups);
    }

    /// A publisher-side timeout after SUBSCRIBE_OK is a track failure, not an
    /// expired rendezvous window.
    #[tokio::test]
    async fn accepted_track_timeout_is_not_a_deadline_expiration() {
        let (subscribed, _recv, _outgoing, mut responses) = test_subscribed().await;
        let (writer, reader) = serve::Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "track",
        )
        .produce();

        let serve = subscribed.serve_with_deadline(
            reader,
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        );
        let close_after_accept = async move {
            accept_subscription(&mut responses).await;
            writer.close(ServeError::Timeout).unwrap();
            let mut response = responses.recv().await.unwrap();
            assert!(matches!(response.message, message::Message::PublishDone(_)));
            response.complete(Ok(()));
        };

        let (result, ()) = tokio::join!(serve, close_after_accept);
        assert!(matches!(
            result.unwrap_err(),
            ServeWithDeadlineError::Session(SessionError::Serve(ServeError::Timeout))
        ));
    }

    #[tokio::test]
    async fn rejection_preserves_retry_interval() {
        let (subscribed, _recv, mut outgoing, _responses) = test_subscribed().await;
        // The session retains a Publisher while individual requests come and go.
        let _publisher = subscribed.forwarder.publisher.clone();
        let retry_interval = 1_500;

        subscribed
            .close_with_retry(
                ServeError::Closed(RequestErrorCode::ExcessiveLoad as u64),
                retry_interval,
            )
            .unwrap();

        let outgoing = outgoing.pop().await;
        let Some(message::Message::RequestError(rejection)) = outgoing else {
            panic!("expected REQUEST_ERROR, got {outgoing:?}");
        };
        assert_eq!(rejection.error_code, RequestErrorCode::ExcessiveLoad as u64);
        assert_eq!(rejection.retry_interval, retry_interval);
    }

    #[test]
    fn subscribed_state_counts_opened_streams() {
        let stream_count = StreamCount::default();
        assert_eq!(stream_count.get(), 0);

        stream_count.opened();
        assert_eq!(stream_count.get(), 1);

        stream_count.opened();
        assert_eq!(stream_count.get(), 2);
    }

    #[test]
    fn stream_count_does_not_wrap() {
        let stream_count = StreamCount(Arc::new(AtomicU64::new(u64::MAX - 1)));

        stream_count.opened();
        assert_eq!(stream_count.get(), u64::MAX);

        stream_count.opened();
        assert_eq!(stream_count.get(), u64::MAX);
    }

    #[test]
    fn recv_unsubscribe_marks_unsubscribed_and_closes() {
        let state = State::<ObjectForwarderState>::default();
        let (_send, recv_state) = state.split();
        let mut recv = ObjectForwarderRecv { state: recv_state };

        assert!(!recv.state.lock().unsubscribed);

        recv.recv_unsubscribe().unwrap();

        let locked = recv.state.lock();
        assert!(locked.unsubscribed);
        assert!(matches!(locked.closed, Err(ServeError::Cancel)));
    }

    #[test]
    fn publish_done_code_maps_done_to_track_ended() {
        assert_eq!(
            Subscribed::publish_done_code(&ServeError::Done),
            message::PublishDoneCode::TrackEnded as u64
        );
    }

    #[test]
    fn publish_done_code_passes_through_closed_code() {
        assert_eq!(
            Subscribed::publish_done_code(&ServeError::Closed(0x12)),
            0x12
        );
    }

    #[test]
    fn publish_done_code_maps_other_errors_to_internal() {
        assert_eq!(
            Subscribed::publish_done_code(&ServeError::internal_ctx("test")),
            message::PublishDoneCode::InternalError as u64
        );
    }

    #[test]
    fn request_error_code_maps_rejection_reasons() {
        assert_eq!(
            Subscribed::request_error_code(&ServeError::NotFound),
            RequestErrorCode::DoesNotExist as u64
        );
        assert_eq!(
            Subscribed::request_error_code(&ServeError::Duplicate),
            RequestErrorCode::DuplicateSubscription as u64
        );
        // A rendezvous hold that expired is TIMEOUT, not DOES_NOT_EXIST.
        assert_eq!(
            Subscribed::request_error_code(&ServeError::Timeout),
            RequestErrorCode::Timeout as u64
        );
        assert_eq!(
            Subscribed::request_error_code(&ServeError::NotImplemented("fetch".to_string())),
            RequestErrorCode::NotSupported as u64
        );
        assert_eq!(
            Subscribed::request_error_code(&ServeError::Cancel),
            RequestErrorCode::Uninterested as u64
        );
        assert_eq!(
            Subscribed::request_error_code(&ServeError::Closed(0x42)),
            0x42
        );
    }

    #[test]
    fn expected_serve_shutdown_is_only_cancel_or_done() {
        assert!(Subscribed::is_expected_serve_shutdown(
            &SessionError::Serve(ServeError::Cancel)
        ));
        assert!(Subscribed::is_expected_serve_shutdown(
            &SessionError::Serve(ServeError::Done)
        ));
        assert!(!Subscribed::is_expected_serve_shutdown(
            &SessionError::Serve(ServeError::NotFound)
        ));
        assert!(!Subscribed::is_expected_serve_shutdown(
            &SessionError::Internal
        ));
    }
}
