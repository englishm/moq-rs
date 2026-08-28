// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ops;
use std::sync::{Arc, Mutex};

use futures::stream::FuturesUnordered;
use futures::StreamExt;

use crate::coding::{Encode, KeyValuePairs, Location, ReasonPhrase};
use crate::message::RequestErrorCode;
use crate::mlog;
use crate::serve::{ServeError, TrackReaderMode};
use crate::watch::State;
use crate::{data, message, serve};

use super::{DeliveryFilter, Publisher, SessionError, SubscribeInfo, Writer};

// This file defines Publisher handling of inbound Subscriptions

#[derive(Debug)]
struct ObjectForwarderState {
    largest_location: Option<Location>,
    stream_count: u64,
    retry_interval: u64,
    /// Set to true when UNSUBSCRIBE is received.  When true, Drop skips sending
    /// PUBLISH_DONE or REQUEST_ERROR because the subscriber already terminated.
    unsubscribed: bool,
    closed: Result<(), ServeError>,
}

impl ObjectForwarderState {
    fn record_stream_opened(&mut self) {
        self.stream_count = self.stream_count.saturating_add(1);
    }

    fn update_largest_location(&mut self, group_id: u64, object_id: u64) -> Result<(), ServeError> {
        if let Some(current_largest_location) = self.largest_location {
            let update_largest_location = Location::new(group_id, object_id);
            if update_largest_location > current_largest_location {
                self.largest_location = Some(update_largest_location);
            }
        }

        Ok(())
    }
}

impl Default for ObjectForwarderState {
    fn default() -> Self {
        Self {
            largest_location: None,
            stream_count: 0,
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

    /// Tracks if SubscribeOk has been sent yet or not. Used to send
    /// PUBLISH_DONE vs REQUEST_ERROR on drop.
    ok: bool,
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

    /// Subgroup streams opened so far. The PUBLISH path needs this separately
    /// from [`Self::terminal_state`], because there the terminal message is sent
    /// by `Published::drop` after the forwarder is gone.
    pub(super) fn stream_count(&self) -> u64 {
        self.state.lock().stream_count
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
            state.stream_count,
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
        };

        Ok((send, recv))
    }

    pub async fn serve(self, track: serve::TrackReader) -> Result<(), SessionError> {
        self.serve_before(track, None).await
    }

    /// Serve a track only if acceptance is committed before `deadline`.
    pub async fn serve_with_deadline(
        mut self,
        track: serve::TrackReader,
        deadline: tokio::time::Instant,
    ) -> Result<(), ServeWithDeadlineError> {
        let result = self.serve_inner(track, Some(deadline)).await;
        let deadline_expired =
            !self.ok && matches!(result, Err(SessionError::Serve(ServeError::Timeout)));

        let Err(err) = result else {
            return Ok(());
        };

        if deadline_expired {
            // A simultaneous UNSUBSCRIBE may have closed the shared state first,
            // but the relay still needs the deadline result for its response and metric.
            let _ = self.close(err.clone().into());
            return Err(ServeWithDeadlineError::DeadlineExpired);
        }

        self.close(err.clone().into()).map_err(SessionError::from)?;
        Err(err.into())
    }

    async fn serve_before(
        mut self,
        track: serve::TrackReader,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<(), SessionError> {
        let res = self.serve_inner(track, deadline).await;
        if let Err(err) = &res {
            self.close(err.clone().into())?;
        }

        res
    }

    async fn serve_inner(
        &mut self,
        track: serve::TrackReader,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<(), SessionError> {
        // Update largest location before sending SubscribeOk
        let largest_location = track.largest_location();
        self.forwarder
            .prepare_subscribe_ok(largest_location, deadline)?;

        // Send SubscribeOk using send_message_and_wait to ensure it is sent at least to the QUIC stack before
        // we start serving the track.  If a subscriber gets the stream before SubscribeOk
        // then they won't recognize the track_alias in the stream header.
        let mut params = KeyValuePairs::default();
        if let Some(largest) = largest_location {
            params
                .set_largest_object(largest)
                .map_err(|_| SessionError::Internal)?;
        }

        self.forwarder
            .publisher
            .send_message_and_wait(message::SubscribeOk {
                id: self.info.id,
                track_alias: self.info.id, // use subscription id as track alias
                params,
                track_extensions: Default::default(),
            })
            .await;

        self.ok = true; // So we send SubscribeDone on drop

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

        // Subscriber already sent UNSUBSCRIBE — no terminal message needed.
        if unsubscribed {
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
                        let header = data::SubgroupHeader {
                            header_type: data::StreamHeaderType::SubgroupIdExt,  // SubGroupId = Yes, Extensions = Yes, ContainsEndOfGroup = No
                            track_alias: self.track_alias,
                            group_id: subgroup.group_id,
                            subgroup_id: Some(subgroup.subgroup_id),
                            publisher_priority: subgroup.priority,
                        };

                        let publisher = self.publisher.clone();
                        let state = self.state.clone();
                        let info = subgroup.info.clone();
                        let mlog = self.mlog.clone();

                        tasks.push(async move {
                            let res = Self::serve_subgroup(header, subgroup, publisher, state, mlog, delivery_filter).await;
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
                res = self.closed(), if done.is_none() => done = Some(res),
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
        header: data::SubgroupHeader,
        mut subgroup_reader: serve::SubgroupReader,
        mut publisher: Publisher,
        state: State<ObjectForwarderState>,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
        delivery_filter: DeliveryFilter,
    ) -> Result<(), SessionError> {
        tracing::trace!(
            "[PUBLISHER] serve_subgroup: starting - group_id={}, subgroup_id={:?}, priority={}",
            subgroup_reader.group_id,
            subgroup_reader.subgroup_id,
            subgroup_reader.priority
        );

        let mut writer: Option<Writer> = None;
        let mut object_count = 0;
        while let Some(mut subgroup_object_reader) = subgroup_reader.next().await? {
            if !delivery_filter.allows(subgroup_reader.group_id, subgroup_object_reader.object_id) {
                tracing::trace!(
                    "[PUBLISHER] serve_subgroup: filtered object group_id={}, object_id={}",
                    subgroup_reader.group_id,
                    subgroup_object_reader.object_id
                );
                continue;
            }

            if writer.is_none() {
                let mut send_stream = publisher.open_uni().await?;
                tracing::trace!("[PUBLISHER] serve_subgroup: opened unidirectional stream");

                state
                    .lock_mut()
                    .ok_or(ServeError::Done)?
                    .record_stream_opened();

                // TODO figure out u32 vs u64 priority
                send_stream.set_priority(subgroup_reader.priority as i32);

                let mut new_writer = Writer::new(send_stream);

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
            let subgroup_object = data::SubgroupObjectExt {
                // TODO(itzmanish): compute real delta when the receive side uses object IDs
                // for ordering. Both sender and receiver must agree on the same prev tracking
                // semantics before this is meaningful.
                object_id_delta: 0,
                extension_headers: subgroup_object_reader.extension_headers.clone(), // Pass through extension headers
                payload_length: subgroup_object_reader.size,
                status: if subgroup_object_reader.size == 0 {
                    // Only set status if payload length is zero
                    Some(subgroup_object_reader.status)
                } else {
                    None
                },
            };

            tracing::trace!(
                "[PUBLISHER] serve_subgroup: sending object #{} - object_id={}, object_id_delta={}, payload_length={}, status={:?}, extension_headers={:?}",
                object_count + 1,
                subgroup_object_reader.object_id,
                subgroup_object.object_id_delta,
                subgroup_object.payload_length,
                subgroup_object.status,
                subgroup_object.extension_headers
            );

            writer.encode(&subgroup_object).await?;

            // Log subgroup object created/sent
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

            state
                .lock_mut()
                .ok_or(ServeError::Done)?
                .update_largest_location(
                    subgroup_reader.group_id,
                    subgroup_object_reader.object_id,
                )?;

            let mut chunks_sent = 0;
            let mut bytes_sent = 0;
            while let Some(chunk) = subgroup_object_reader.read().await? {
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

            // Determine datagram type based on extension headers presence
            let has_extension_headers = !datagram.extension_headers.is_empty();
            let datagram_type = if has_extension_headers {
                data::DatagramType::ObjectIdPayloadExt
            } else {
                data::DatagramType::ObjectIdPayload
            };

            // Bound locally so the logging and largest-location updates below
            // read it directly instead of unwrapping the Option again.
            let object_id = datagram.object_id;
            let encoded_datagram = data::Datagram {
                datagram_type,
                track_alias: self.track_alias,
                group_id: datagram.group_id,
                object_id: Some(object_id),
                publisher_priority: datagram.priority,
                extension_headers: if has_extension_headers {
                    Some(datagram.extension_headers.clone())
                } else {
                    None
                },
                status: None,
                payload: Some(datagram.payload),
            };

            let payload_len = encoded_datagram
                .payload
                .as_ref()
                .map(|p| p.len())
                .unwrap_or(0);
            let mut buffer = bytes::BytesMut::with_capacity(payload_len + 100);
            encoded_datagram.encode(&mut buffer)?;

            tracing::trace!(
                "[PUBLISHER] serve_datagrams: sending datagram #{} - track_alias={}, group_id={}, object_id={}, priority={}, payload_len={}, extension_headers={:?}, total_encoded_len={}",
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

            self.state
                .lock_mut()
                .ok_or(ServeError::Done)?
                .update_largest_location(encoded_datagram.group_id, object_id)?;

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

    async fn test_subscribed() -> (
        Subscribed,
        ObjectForwarderRecv,
        crate::watch::Queue<message::Message>,
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
        (subscribed, recv, outgoing_recv)
    }

    #[tokio::test]
    async fn deadline_prevents_subscribe_ok() {
        let (subscribed, _recv, _outgoing) = test_subscribed().await;
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
        let (subscribed, mut recv, _outgoing) = test_subscribed().await;
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

    /// A publisher-side timeout after SUBSCRIBE_OK is a track failure, not an
    /// expired rendezvous window.
    #[tokio::test]
    async fn accepted_track_timeout_is_not_a_deadline_expiration() {
        let (subscribed, _recv, mut outgoing) = test_subscribed().await;
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
            assert!(matches!(
                outgoing.pop().await,
                Some(message::Message::SubscribeOk(_))
            ));
            writer.close(ServeError::Timeout).unwrap();
        };

        let (result, ()) = tokio::join!(serve, close_after_accept);
        assert!(matches!(
            result.unwrap_err(),
            ServeWithDeadlineError::Session(SessionError::Serve(ServeError::Timeout))
        ));
    }

    #[tokio::test]
    async fn rejection_preserves_retry_interval() {
        let (subscribed, _recv, mut outgoing) = test_subscribed().await;
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
        let mut state = ObjectForwarderState::default();
        assert_eq!(state.stream_count, 0);

        state.record_stream_opened();
        assert_eq!(state.stream_count, 1);

        state.record_stream_opened();
        assert_eq!(state.stream_count, 2);
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
