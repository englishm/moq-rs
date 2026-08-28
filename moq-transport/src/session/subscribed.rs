// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ops;
use std::sync::{Arc, Mutex};

use futures::stream::FuturesUnordered;
use futures::StreamExt;

use crate::coding::{Encode, KeyValuePairs, Location, ReasonPhrase};
use crate::data::DataStreamResetCode;
use crate::message::RequestErrorCode;
use crate::mlog;
use crate::serve::{ServeError, TrackReaderMode};
use crate::watch::State;
use crate::{data, message, serve};

use super::{DeliveryFilter, Publisher, SessionError, SessionId, SubscribeInfo, Writer};

// This file defines Publisher handling of inbound Subscriptions

#[derive(Debug)]
struct ObjectForwarderState {
    largest_location: Option<Location>,
    stream_count: u64,
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
            unsubscribed: false,
            closed: Ok(()),
        }
    }
}

pub struct Subscribed {
    /// The tracknamespace and trackname for the subscription.
    pub info: SubscribeInfo,

    forwarder: ObjectForwarder,

    /// Tracks if SubscribeOk has been sent yet or not. Used to send
    /// PUBLISH_DONE vs REQUEST_ERROR on drop.
    ok: bool,
}

pub(super) struct ObjectForwarder {
    /// The sessions Publisher manager, used to create streams and datagrams.
    publisher: Publisher,
    state: State<ObjectForwarderState>,
    track_alias: u64,
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

    fn terminal_state(&self) -> (ServeError, u64, bool) {
        let state = self.state.lock();
        let err = state
            .closed
            .as_ref()
            .err()
            .cloned()
            .unwrap_or(ServeError::Done);
        (err, state.stream_count, state.unsubscribed)
    }

    fn close(&self, err: ServeError) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        let mut state = state.into_mut().ok_or(ServeError::Done)?;
        state.closed = Err(err);

        Ok(())
    }

    async fn closed(&self) -> Result<(), ServeError> {
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

/// A subgroup data stream that is reset unless it is explicitly finished.
///
/// Draft-16 §10.4.3: a FIN means "every object in this subgroup was delivered".
/// Any earlier termination MUST be a `RESET_STREAM`, and the listed causes
/// include early termination due to UNSUBSCRIBE and a publisher ending the
/// subscription early — exactly the paths a relay hits when downstream interest
/// disappears or an upstream track dies mid-object.
///
/// `quinn::SendStream::drop` implicitly calls `finish()`, so simply dropping the
/// writer on a cancelled or failed forwarding task FINs the stream wherever it
/// happened to stop. If that is mid-object the receiver has already been
/// promised a payload length it will never get, and treats the truncated
/// subgroup as a malformed track. This wrapper inverts that default: the stream
/// is reset on drop unless [`SubgroupStream::finish`] ran, so the safe outcome
/// is the automatic one.
struct SubgroupStream {
    writer: Writer,
    /// Set once the stream has been explicitly finished or reset, after which
    /// `Drop` must not touch it again.
    terminated: bool,
}

impl SubgroupStream {
    fn new(writer: Writer) -> Self {
        Self {
            writer,
            terminated: false,
        }
    }

    fn finish(&mut self) -> Result<(), SessionError> {
        if self.terminated {
            return Ok(());
        }
        self.terminated = true;
        self.writer.finish()
    }

    fn reset(&mut self, code: DataStreamResetCode) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        self.writer.reset(code.into());
    }
}

impl Drop for SubgroupStream {
    fn drop(&mut self) {
        // Covers async cancellation, where no error path gets a chance to run:
        // dropping the forwarding future must not leave quinn to implicitly FIN
        // a partially written subgroup. `Cancelled` is the right default because
        // a dropped forwarding task means the subscription ended early.
        self.reset(DataStreamResetCode::Cancelled);
    }
}

/// How a subgroup stream was terminated. Recorded by the test sink so tests can
/// assert FIN-vs-RESET behaviour without a real QUIC connection.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubgroupTermination {
    Fin,
    Reset(DataStreamResetCode),
}

enum SubgroupSink {
    Stream(SubgroupStream),
    #[cfg(test)]
    Buffer {
        buffer: bytes::BytesMut,
        termination: Option<SubgroupTermination>,
    },
}

/// Writes a subgroup to a sink while tracking whether we are mid-object.
///
/// The accounting lives here rather than in the sink so there is exactly one
/// place that knows whether a FIN is currently legal.
struct SubgroupOutput {
    session_id: SessionId,
    sink: SubgroupSink,
    /// Payload bytes still owed for the object whose header we already wrote.
    /// Non-zero means we are mid-object and MUST NOT FIN.
    owed: usize,
}

impl SubgroupOutput {
    fn stream(writer: Writer) -> Self {
        Self {
            session_id: writer.session_id().clone(),
            sink: SubgroupSink::Stream(SubgroupStream::new(writer)),
            owed: 0,
        }
    }

    #[cfg(test)]
    fn buffer() -> Self {
        Self {
            session_id: SessionId::generate(),
            sink: SubgroupSink::Buffer {
                buffer: bytes::BytesMut::new(),
                termination: None,
            },
            owed: 0,
        }
    }

    async fn encode<T: Encode>(&mut self, msg: &T) -> Result<(), SessionError> {
        match &mut self.sink {
            SubgroupSink::Stream(stream) => stream.writer.encode(msg).await,
            #[cfg(test)]
            SubgroupSink::Buffer { buffer, .. } => {
                msg.encode(buffer)?;
                Ok(())
            }
        }
    }

    async fn write(&mut self, buf: &[u8]) -> Result<(), SessionError> {
        match &mut self.sink {
            SubgroupSink::Stream(stream) => stream.writer.write(buf).await?,
            #[cfg(test)]
            SubgroupSink::Buffer { buffer, .. } => buffer.extend_from_slice(buf),
        }

        self.owed = self.owed.saturating_sub(buf.len());
        Ok(())
    }

    /// Record that an object header promising `len` payload bytes was written.
    fn begin_object(&mut self, len: usize) {
        self.owed = len;
    }

    /// True when every promised payload byte has been written.
    fn at_object_boundary(&self) -> bool {
        self.owed == 0
    }

    /// FIN the stream, asserting the whole subgroup was delivered.
    ///
    /// Only legal at an object boundary; finishing while payload bytes are still
    /// owed is the truncation this type exists to prevent, so it resets instead.
    fn finish(&mut self) -> Result<(), SessionError> {
        if !self.at_object_boundary() {
            tracing::warn!(
                session_id = %self.session_id,
                owed = self.owed,
                "refusing to FIN a subgroup stream mid-object; resetting instead"
            );
            self.reset(DataStreamResetCode::InternalError);
            return Err(ServeError::Size.into());
        }

        match &mut self.sink {
            SubgroupSink::Stream(stream) => stream.finish(),
            #[cfg(test)]
            SubgroupSink::Buffer { termination, .. } => {
                termination.get_or_insert(SubgroupTermination::Fin);
                Ok(())
            }
        }
    }

    /// RESET the stream, signalling an incomplete subgroup.
    fn reset(&mut self, code: DataStreamResetCode) {
        match &mut self.sink {
            SubgroupSink::Stream(stream) => stream.reset(code),
            #[cfg(test)]
            SubgroupSink::Buffer { termination, .. } => {
                termination.get_or_insert(SubgroupTermination::Reset(code));
            }
        }
    }

    #[cfg(test)]
    fn into_parts(self) -> (bytes::BytesMut, Option<SubgroupTermination>) {
        match self.sink {
            SubgroupSink::Buffer {
                buffer,
                termination,
            } => (buffer, termination),
            SubgroupSink::Stream(_) => unreachable!("test output should use a buffer"),
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
        let track_alias = info.id;
        let (forwarder, recv) = ObjectForwarder::new(publisher, track_alias, mlog);
        let send = Self {
            info,
            forwarder,
            ok: false,
        };

        Ok((send, recv))
    }

    pub async fn serve(mut self, track: serve::TrackReader) -> Result<(), SessionError> {
        let res = self.serve_inner(track).await;
        if let Err(err) = &res {
            self.close(err.clone().into())?;
        }

        res
    }

    async fn serve_inner(&mut self, track: serve::TrackReader) -> Result<(), SessionError> {
        // Update largest location before sending SubscribeOk
        let largest_location = track.largest_location();
        self.forwarder.set_largest_location(largest_location)?;

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
                track_alias: self.info.id,
                params,
                track_extensions: Default::default(),
            })
            .await;

        self.ok = true; // So we send PUBLISH_DONE on drop

        let delivery_filter = self.info.delivery_filter(largest_location);

        self.forwarder.serve(track, delivery_filter).await
    }

    pub fn close(self, err: ServeError) -> Result<(), ServeError> {
        self.forwarder.close(err)
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
        let (err, stream_count, unsubscribed) = self.forwarder.terminal_state();

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
                    retry_interval: 0,
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
}

impl ObjectForwarder {
    async fn serve_subgroups(
        &mut self,
        mut subgroups: serve::SubgroupsReader,
        delivery_filter: DeliveryFilter,
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();
        let mut done: Option<Result<(), ServeError>> = None;

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
                        let session_id = self.publisher.session_id().clone();

                        tasks.push(async move {
                            if let Err(err) = Self::serve_subgroup(header, subgroup, publisher, state, mlog, delivery_filter).await {
                                if Subscribed::is_expected_serve_shutdown(&err) {
                                    tracing::debug!(session_id = %session_id, subgroup_info = ?info, error = %err, "stopped serving subgroup");
                                } else {
                                    tracing::warn!(session_id = %session_id, subgroup_info = ?info, error = %err, "failed to serve subgroup");
                                }
                            }
                        });
                    },
                    Ok(None) => done = Some(Ok(())),
                    Err(err) => done = Some(Err(err)),
                },
                res = self.closed(), if done.is_none() => done = Some(res),
                _ = tasks.next(), if !tasks.is_empty() => {},
                else => return Ok(done.unwrap()?),
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

        let Some(first_object) =
            Self::next_allowed_object(&mut subgroup_reader, delivery_filter).await?
        else {
            return Ok(());
        };

        let mut send_stream = publisher.open_uni().await?;
        tracing::trace!("[PUBLISHER] serve_subgroup: opened unidirectional stream");

        state
            .lock_mut()
            .ok_or(ServeError::Done)?
            .record_stream_opened();

        // TODO figure out u32 vs u64 priority
        send_stream.set_priority(subgroup_reader.priority as i32);

        let mut output =
            SubgroupOutput::stream(Writer::new(publisher.session_id().clone(), send_stream));
        let res = Self::serve_subgroup_objects(
            header,
            subgroup_reader,
            first_object,
            &mut output,
            state,
            mlog,
            delivery_filter,
        )
        .await;

        // Draft-16 §10.4.3: FIN only if the whole subgroup was delivered,
        // otherwise RESET_STREAM. Without this the `Writer` would be dropped and
        // quinn would implicitly FIN wherever we stopped, which silently
        // truncates the in-flight object.
        match res {
            Ok(()) => output.finish(),
            Err(err) => {
                output.reset(Self::reset_code_for(&err));
                Err(err)
            }
        }
    }

    /// Map a forwarding failure onto a draft-16 §13.4.4 reset code.
    fn reset_code_for(err: &SessionError) -> DataStreamResetCode {
        match err {
            // The subscriber went away (UNSUBSCRIBE) or the track was cancelled;
            // §10.4.3 calls out UNSUBSCRIBE as a reset case explicitly.
            SessionError::Serve(ServeError::Done | ServeError::Cancel) => {
                DataStreamResetCode::Cancelled
            }
            // Upstream delivered fewer payload bytes than its object header
            // promised, so the track itself is malformed.
            SessionError::Serve(ServeError::Size) => DataStreamResetCode::MalformedTrack,
            SessionError::Serve(ServeError::Closed(_)) => DataStreamResetCode::SessionClosed,
            _ => DataStreamResetCode::InternalError,
        }
    }

    async fn next_allowed_object(
        subgroup_reader: &mut serve::SubgroupReader,
        delivery_filter: DeliveryFilter,
    ) -> Result<Option<serve::SubgroupObjectReader>, ServeError> {
        while let Some(subgroup_object_reader) = subgroup_reader.next().await? {
            if delivery_filter.allows(subgroup_reader.group_id, subgroup_object_reader.object_id) {
                return Ok(Some(subgroup_object_reader));
            }

            tracing::trace!(
                "[PUBLISHER] serve_subgroup: filtered object group_id={}, object_id={}",
                subgroup_reader.group_id,
                subgroup_object_reader.object_id
            );
        }

        Ok(None)
    }

    async fn serve_subgroup_objects(
        header: data::SubgroupHeader,
        mut subgroup_reader: serve::SubgroupReader,
        first_object: serve::SubgroupObjectReader,
        output: &mut SubgroupOutput,
        state: State<ObjectForwarderState>,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
        delivery_filter: DeliveryFilter,
    ) -> Result<(), SessionError> {
        tracing::trace!(
            "[PUBLISHER] serve_subgroup: sending header - track_alias={}, group_id={}, subgroup_id={:?}, priority={}, header_type={:?}",
            header.track_alias,
            header.group_id,
            header.subgroup_id,
            header.publisher_priority,
            header.header_type
        );

        output.encode(&header).await?;

        // Log subgroup header created/sent
        if let Some(ref mlog) = mlog {
            if let Ok(mut mlog_guard) = mlog.lock() {
                let time = mlog_guard.elapsed_ms();
                let stream_id = 0; // TODO: Placeholder, need actual QUIC stream ID
                let event = mlog::subgroup_header_created(time, stream_id, &header);
                let _ = mlog_guard.add_event(event);
            }
        }

        let mut object_count = 0;
        let mut next_object = Some(first_object);
        loop {
            let mut subgroup_object_reader = match next_object.take() {
                Some(reader) => reader,
                None => {
                    match Self::next_allowed_object(&mut subgroup_reader, delivery_filter).await? {
                        Some(reader) => reader,
                        None => break,
                    }
                }
            };

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

            // Check the subscription is still live and the location is valid
            // *before* writing the object header. The header promises
            // `payload_length` bytes, so bailing out after writing it leaves the
            // receiver waiting on payload we will never send. Previously this
            // check ran after the encode, so a downstream UNSUBSCRIBE landing
            // here truncated the object.
            state
                .lock_mut()
                .ok_or(ServeError::Done)?
                .update_largest_location(
                    subgroup_reader.group_id,
                    subgroup_object_reader.object_id,
                )?;

            output.encode(&subgroup_object).await?;
            // From here until the payload is fully written we are mid-object and
            // must not FIN.
            output.begin_object(subgroup_object.payload_length);

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
                output.write(&chunk).await?;
                chunks_sent += 1;
            }

            tracing::trace!(
                "[PUBLISHER] serve_subgroup: completed object #{} ({} chunks, {} bytes total)",
                object_count + 1,
                chunks_sent,
                bytes_sent
            );

            // The reader ran out of chunks before satisfying the length we already
            // promised. Surface it as an error so the stream is reset rather than
            // FINed at a byte offset the receiver will read as a partial object.
            if !output.at_object_boundary() {
                tracing::warn!(
                    session_id = %output.session_id,
                    group_id = subgroup_reader.group_id,
                    object_id = subgroup_object_reader.object_id,
                    promised = subgroup_object.payload_length,
                    sent = bytes_sent,
                    "upstream object ended short of its declared payload length"
                );
                return Err(ServeError::Size.into());
            }

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

    #[cfg(test)]
    async fn serve_subgroup_to_buffer(
        header: data::SubgroupHeader,
        subgroup_reader: serve::SubgroupReader,
        state: State<ObjectForwarderState>,
        delivery_filter: DeliveryFilter,
    ) -> Result<bytes::BytesMut, SessionError> {
        let (buffer, res, _) =
            Self::serve_subgroup_to_parts(header, subgroup_reader, state, delivery_filter).await;
        res?;
        Ok(buffer)
    }

    /// Test helper mirroring [`Self::serve_subgroup`]'s termination logic so tests
    /// can assert whether the stream would have been FINed or reset.
    #[cfg(test)]
    async fn serve_subgroup_to_parts(
        header: data::SubgroupHeader,
        mut subgroup_reader: serve::SubgroupReader,
        state: State<ObjectForwarderState>,
        delivery_filter: DeliveryFilter,
    ) -> (
        bytes::BytesMut,
        Result<(), SessionError>,
        Option<SubgroupTermination>,
    ) {
        let first_object =
            match Self::next_allowed_object(&mut subgroup_reader, delivery_filter).await {
                Ok(Some(first_object)) => first_object,
                Ok(None) => return (bytes::BytesMut::new(), Ok(()), None),
                Err(err) => return (bytes::BytesMut::new(), Err(err.into()), None),
            };

        match state.lock_mut() {
            Some(mut state) => state.record_stream_opened(),
            None => return (bytes::BytesMut::new(), Err(ServeError::Done.into()), None),
        }

        let mut output = SubgroupOutput::buffer();
        let res = Self::serve_subgroup_objects(
            header,
            subgroup_reader,
            first_object,
            &mut output,
            state,
            None,
            delivery_filter,
        )
        .await;

        let res = match res {
            Ok(()) => output.finish(),
            Err(err) => {
                output.reset(Self::reset_code_for(&err));
                Err(err)
            }
        };

        let (buffer, termination) = output.into_parts();
        (buffer, res, termination)
    }

    async fn serve_datagrams(
        &mut self,
        mut datagrams: serve::DatagramsReader,
        delivery_filter: DeliveryFilter,
    ) -> Result<(), SessionError> {
        tracing::debug!(session_id = %self.publisher.session_id(), "[PUBLISHER] serve_datagrams: starting");

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

            let encoded_datagram = data::Datagram {
                datagram_type,
                track_alias: self.track_alias,
                group_id: datagram.group_id,
                object_id: Some(datagram.object_id),
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
                encoded_datagram.object_id.unwrap(),
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
                .update_largest_location(
                    encoded_datagram.group_id,
                    encoded_datagram.object_id.unwrap(),
                )?;

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

    #[tokio::test]
    async fn object_forwarder_forwards_subgroup_object_to_output() {
        use bytes::{Buf, Bytes};

        use crate::{coding::Decode, coding::TrackNamespace};

        let (track_writer, track_reader) =
            serve::Track::new(TrackNamespace::from_utf8_path("test"), "video").produce();
        let mut subgroups_writer = track_writer.subgroups().unwrap();
        let mut subgroup_writer = subgroups_writer
            .create(serve::Subgroup {
                group_id: 7,
                subgroup_id: 2,
                priority: 9,
            })
            .unwrap();
        subgroup_writer.write(Bytes::from_static(b"hello")).unwrap();
        drop(subgroup_writer);
        drop(subgroups_writer);

        let mut subgroups = match track_reader.mode().await.unwrap() {
            TrackReaderMode::Subgroups(subgroups) => subgroups,
            _ => panic!("expected subgroups mode"),
        };
        let subgroup = subgroups
            .next()
            .await
            .unwrap()
            .expect("subgroup should be available");
        let state = State::<ObjectForwarderState>::default();
        let header = data::SubgroupHeader {
            header_type: data::StreamHeaderType::SubgroupIdExt,
            track_alias: 42,
            group_id: subgroup.group_id,
            subgroup_id: Some(subgroup.subgroup_id),
            publisher_priority: subgroup.priority,
        };

        let output = ObjectForwarder::serve_subgroup_to_buffer(
            header.clone(),
            subgroup,
            state.clone(),
            DeliveryFilter {
                forward: true,
                start_location: None,
                end_group_id: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(state.lock().stream_count, 1);

        let mut output = output.freeze();
        let header_type = data::StreamHeaderType::decode(&mut output).unwrap();
        let decoded_header = data::SubgroupHeader::decode(header_type, &mut output).unwrap();
        assert_eq!(decoded_header, header);

        let object = data::SubgroupObjectExt::decode(&mut output).unwrap();
        assert_eq!(object.object_id_delta, 0);
        assert!(object.extension_headers.is_empty());
        assert_eq!(object.payload_length, 5);
        assert_eq!(object.status, None);

        let payload = output.copy_to_bytes(object.payload_length);
        assert_eq!(&payload[..], b"hello");
        assert!(!output.has_remaining());
    }

    /// Build a single-subgroup track reader carrying one complete object.
    #[cfg(test)]
    async fn subgroup_with_one_object() -> (serve::SubgroupReader, data::SubgroupHeader) {
        use bytes::Bytes;

        use crate::coding::TrackNamespace;

        let (track_writer, track_reader) =
            serve::Track::new(TrackNamespace::from_utf8_path("test"), "video").produce();
        let mut subgroups_writer = track_writer.subgroups().unwrap();
        let mut subgroup_writer = subgroups_writer
            .create(serve::Subgroup {
                group_id: 1,
                subgroup_id: 0,
                priority: 0,
            })
            .unwrap();
        subgroup_writer.write(Bytes::from_static(b"hello")).unwrap();
        drop(subgroup_writer);
        drop(subgroups_writer);

        let mut subgroups = match track_reader.mode().await.unwrap() {
            TrackReaderMode::Subgroups(subgroups) => subgroups,
            _ => panic!("expected subgroups mode"),
        };
        let subgroup = subgroups.next().await.unwrap().expect("subgroup available");
        let header = data::SubgroupHeader {
            header_type: data::StreamHeaderType::SubgroupIdExt,
            track_alias: 1,
            group_id: subgroup.group_id,
            subgroup_id: Some(subgroup.subgroup_id),
            publisher_priority: subgroup.priority,
        };

        (subgroup, header)
    }

    #[cfg(test)]
    fn all_objects() -> DeliveryFilter {
        DeliveryFilter {
            forward: true,
            start_location: None,
            end_group_id: None,
        }
    }

    /// A fully delivered subgroup is the one case where draft-16 §10.4.3 permits
    /// a FIN.
    #[tokio::test]
    async fn complete_subgroup_is_finished_with_fin() {
        let (subgroup, header) = subgroup_with_one_object().await;
        let state = State::<ObjectForwarderState>::default();

        let (_buffer, res, termination) =
            ObjectForwarder::serve_subgroup_to_parts(header, subgroup, state, all_objects()).await;

        res.expect("serving a complete subgroup should succeed");
        assert_eq!(termination, Some(SubgroupTermination::Fin));
    }

    /// Draft-16 §10.4.3 lists UNSUBSCRIBE as a case that MUST reset rather than
    /// FIN, and the stream must not be cut inside an object.
    ///
    /// This is the regression test for the truncation bug: the forwarder used to
    /// encode an object header (promising `payload_length` bytes) and only then
    /// check whether the subscription was still alive. When an UNSUBSCRIBE landed
    /// in that window it returned early, dropped the `Writer`, and quinn
    /// implicitly FINed the stream mid-object.
    #[tokio::test]
    async fn unsubscribe_mid_subgroup_resets_at_an_object_boundary() {
        use bytes::{Buf, Bytes};

        use crate::coding::{Decode, TrackNamespace};

        let (track_writer, track_reader) =
            serve::Track::new(TrackNamespace::from_utf8_path("test"), "video").produce();
        let mut subgroups_writer = track_writer.subgroups().unwrap();
        let mut subgroup_writer = subgroups_writer
            .create(serve::Subgroup {
                group_id: 1,
                subgroup_id: 0,
                priority: 0,
            })
            .unwrap();

        // First object is available immediately; the second arrives only after we
        // simulate the UNSUBSCRIBE.
        subgroup_writer.write(Bytes::from_static(b"hello")).unwrap();

        let mut subgroups = match track_reader.mode().await.unwrap() {
            TrackReaderMode::Subgroups(subgroups) => subgroups,
            _ => panic!("expected subgroups mode"),
        };
        let subgroup = subgroups.next().await.unwrap().expect("subgroup available");
        let header = data::SubgroupHeader {
            header_type: data::StreamHeaderType::SubgroupIdExt,
            track_alias: 1,
            group_id: subgroup.group_id,
            subgroup_id: Some(subgroup.subgroup_id),
            publisher_priority: subgroup.priority,
        };

        // Dropping one half of the split state is what UNSUBSCRIBE does to the
        // forwarder: `lock_mut` starts returning None.
        let (unsubscribe_handle, state) = State::<ObjectForwarderState>::default().split();

        let fut = ObjectForwarder::serve_subgroup_to_parts(
            header.clone(),
            subgroup,
            state,
            all_objects(),
        );
        tokio::pin!(fut);

        // Let the forwarder deliver the first object and then park waiting for
        // the next one.
        tokio::select! {
            _ = &mut fut => panic!("forwarder should still be awaiting the next object"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        drop(unsubscribe_handle);
        subgroup_writer.write(Bytes::from_static(b"world")).unwrap();

        let (buffer, res, termination) = fut.await;

        assert!(res.is_err(), "forwarding should fail once unsubscribed");
        assert_eq!(
            termination,
            Some(SubgroupTermination::Reset(DataStreamResetCode::Cancelled)),
            "an early-terminated subgroup must be reset, never FINed"
        );

        // The bytes on the wire must end on an object boundary: the subgroup
        // header plus exactly the first complete object, with no header for the
        // object we never delivered.
        let mut buffer = buffer.freeze();
        let header_type = data::StreamHeaderType::decode(&mut buffer).unwrap();
        assert_eq!(
            data::SubgroupHeader::decode(header_type, &mut buffer).unwrap(),
            header
        );

        let object = data::SubgroupObjectExt::decode(&mut buffer).unwrap();
        assert_eq!(object.payload_length, 5);
        assert_eq!(&buffer.copy_to_bytes(object.payload_length)[..], b"hello");
        assert!(
            !buffer.has_remaining(),
            "no partial object should follow the last complete one"
        );
    }

    /// FIN must be refused while payload bytes are still owed, even if a caller
    /// asks for one; otherwise the receiver sees a truncated object.
    #[tokio::test]
    async fn finish_mid_object_resets_instead_of_truncating() {
        let mut output = SubgroupOutput::buffer();

        output.begin_object(10);
        output.write(b"abc").await.unwrap();
        assert!(!output.at_object_boundary());

        let err = output.finish().expect_err("FIN mid-object must be refused");
        assert!(matches!(err, SessionError::Serve(ServeError::Size)));

        let (_buffer, termination) = output.into_parts();
        assert_eq!(
            termination,
            Some(SubgroupTermination::Reset(
                DataStreamResetCode::InternalError
            ))
        );
    }

    #[tokio::test]
    async fn owed_payload_tracking_follows_writes() {
        let mut output = SubgroupOutput::buffer();
        assert!(output.at_object_boundary(), "no object in flight");

        output.begin_object(5);
        assert!(!output.at_object_boundary());

        output.write(b"hel").await.unwrap();
        assert!(!output.at_object_boundary());

        output.write(b"lo").await.unwrap();
        assert!(output.at_object_boundary(), "object fully delivered");

        output.finish().expect("FIN legal at object boundary");
    }

    #[test]
    fn reset_codes_follow_the_failure_cause() {
        // §10.4.3: subscription ended early.
        assert_eq!(
            ObjectForwarder::reset_code_for(&ServeError::Done.into()),
            DataStreamResetCode::Cancelled
        );
        assert_eq!(
            ObjectForwarder::reset_code_for(&ServeError::Cancel.into()),
            DataStreamResetCode::Cancelled
        );
        // Upstream gave us fewer bytes than its object header promised.
        assert_eq!(
            ObjectForwarder::reset_code_for(&ServeError::Size.into()),
            DataStreamResetCode::MalformedTrack
        );
        assert_eq!(
            ObjectForwarder::reset_code_for(&ServeError::Closed(0x2).into()),
            DataStreamResetCode::SessionClosed
        );
        assert_eq!(
            ObjectForwarder::reset_code_for(&ServeError::internal_ctx("boom").into()),
            DataStreamResetCode::InternalError
        );
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
