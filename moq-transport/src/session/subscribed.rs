// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ops;
use std::sync::{Arc, Mutex};

use futures::stream::FuturesUnordered;
use futures::StreamExt;

use crate::coding::{Encode, Location, ReasonPhrase};
use crate::data::DataStreamResetCode;
use crate::mlog;
use crate::serve::{ServeError, TrackReaderMode};
use crate::watch::State;
use crate::{data, message, serve};

use super::{Publisher, SessionError, SubscribeInfo, Writer};

// This file defines Publisher handling of inbound Subscriptions

/// A subgroup data stream that is reset unless it is explicitly finished.
///
/// Draft-14 §10.4.3: a FIN means "every object in this subgroup was delivered".
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
    sink: SubgroupSink,
    /// Payload bytes still owed for the object whose header we already wrote.
    /// Non-zero means we are mid-object and MUST NOT FIN.
    owed: usize,
}

impl SubgroupOutput {
    fn stream(writer: Writer) -> Self {
        Self {
            sink: SubgroupSink::Stream(SubgroupStream::new(writer)),
            owed: 0,
        }
    }

    #[cfg(test)]
    fn buffer() -> Self {
        Self {
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

#[derive(Debug)]
struct SubscribedState {
    largest_location: Option<Location>,
    closed: Result<(), ServeError>,
}

impl SubscribedState {
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

impl Default for SubscribedState {
    fn default() -> Self {
        Self {
            largest_location: None,
            closed: Ok(()),
        }
    }
}

pub struct Subscribed {
    /// The sessions Publisher manager, used to send control messages,
    /// create new QUIC streams, and send datagrams
    publisher: Publisher,

    /// The tracknamespace and trackname for the subscription.
    pub info: SubscribeInfo,

    state: State<SubscribedState>,

    /// Tracks if SubscribeOk has been sent yet or not. Used to send
    /// SubscribeDone vs SubscribeError on drop.
    ok: bool,

    /// Optional mlog writer for logging transport events
    mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
}

impl Subscribed {
    pub(super) fn new(
        publisher: Publisher,
        msg: message::Subscribe,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> (Self, SubscribedRecv) {
        let (send, recv) = State::default().split();
        let info = SubscribeInfo::new_from_subscribe(&msg);
        let send = Self {
            publisher,
            state: send,
            info,
            ok: false,
            mlog,
        };

        // Prevents updates after being closed
        let recv = SubscribedRecv { state: recv };

        (send, recv)
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
        self.state
            .lock_mut()
            .ok_or(ServeError::Cancel)?
            .largest_location = largest_location;

        // Send SubscribeOk using send_message_and_wait to ensure it is sent at least to the QUIC stack before
        // we start serving the track.  If a subscriber gets the stream before SubscribeOk
        // then they won't recognize the track_alias in the stream header.
        self.publisher
            .send_message_and_wait(message::SubscribeOk {
                id: self.info.id,
                track_alias: self.info.id, // use subscription id as track alias
                expires: 0,                // TODO SLG
                group_order: message::GroupOrder::Descending, // TODO: resolve correct value from publisher / subscriber prefs
                content_exists: largest_location.is_some(),
                largest_location,
                params: Default::default(),
            })
            .await;

        self.ok = true; // So we send SubscribeDone on drop

        // Serve based on track mode
        match track.mode().await? {
            // TODO cancel track/datagrams on closed
            TrackReaderMode::Stream(_stream) => panic!("deprecated"),
            TrackReaderMode::Subgroups(subgroups) => self.serve_subgroups(subgroups).await,
            TrackReaderMode::Datagrams(datagrams) => self.serve_datagrams(datagrams).await,
        }
    }

    pub fn close(self, err: ServeError) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        let mut state = state.into_mut().ok_or(ServeError::Done)?;
        state.closed = Err(err);

        Ok(())
    }

    pub async fn closed(&self) -> Result<(), ServeError> {
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
}

impl ops::Deref for Subscribed {
    type Target = SubscribeInfo;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

impl Drop for Subscribed {
    fn drop(&mut self) {
        let state = self.state.lock();
        let err = state
            .closed
            .as_ref()
            .err()
            .cloned()
            .unwrap_or(ServeError::Done);
        drop(state); // Important to avoid a deadlock

        if self.ok {
            self.publisher.send_message(message::PublishDone {
                id: self.info.id,
                status_code: err.code(),
                stream_count: 0, // TODO SLG
                reason: ReasonPhrase(err.to_string()),
            });
        } else {
            self.publisher.send_message(message::SubscribeError {
                id: self.info.id,
                error_code: err.code(),
                reason_phrase: ReasonPhrase(err.to_string()),
            });
        };
    }
}

impl Subscribed {
    async fn serve_subgroups(
        &mut self,
        mut subgroups: serve::SubgroupsReader,
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();
        let mut done: Option<Result<(), ServeError>> = None;

        loop {
            tokio::select! {
                res = subgroups.next(), if done.is_none() => match res {
                    Ok(Some(subgroup)) => {
                        let header = data::SubgroupHeader {
                            header_type: data::StreamHeaderType::SubgroupIdExt,  // SubGroupId = Yes, Extensions = Yes, ContainsEndOfGroup = No
                            track_alias: self.info.id, // use subscription id as track_alias
                            group_id: subgroup.group_id,
                            subgroup_id: Some(subgroup.subgroup_id),
                            publisher_priority: subgroup.priority,
                        };

                        let publisher = self.publisher.clone();
                        let state = self.state.clone();
                        let info = subgroup.info.clone();
                        let mlog = self.mlog.clone();

                        tasks.push(async move {
                            if let Err(err) = Self::serve_subgroup(header, subgroup, publisher, state, mlog).await {
                                tracing::warn!("failed to serve subgroup: {:?}, error: {}", info, err);
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
        subgroup_reader: serve::SubgroupReader,
        mut publisher: Publisher,
        state: State<SubscribedState>,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        tracing::debug!(
            "[PUBLISHER] serve_subgroup: starting - group_id={}, subgroup_id={:?}, priority={}",
            subgroup_reader.group_id,
            subgroup_reader.subgroup_id,
            subgroup_reader.priority
        );

        let mut send_stream = publisher.open_uni().await?;
        tracing::trace!("[PUBLISHER] serve_subgroup: opened unidirectional stream");

        // TODO figure out u32 vs u64 priority
        send_stream.set_priority(subgroup_reader.priority as i32);

        let mut output = SubgroupOutput::stream(Writer::new(send_stream));
        let res =
            Self::serve_subgroup_objects(header, subgroup_reader, &mut output, state, mlog).await;

        // Draft-14 §10.4.3: FIN only if the whole subgroup was delivered,
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

    /// Map a forwarding failure onto a draft-14 §13.1.8 reset code.
    fn reset_code_for(err: &SessionError) -> DataStreamResetCode {
        match err {
            // The subscriber went away (UNSUBSCRIBE) or the track was cancelled;
            // §10.4.3 calls out UNSUBSCRIBE as a reset case explicitly.
            SessionError::Serve(ServeError::Done | ServeError::Cancel) => {
                DataStreamResetCode::Cancelled
            }
            SessionError::Serve(ServeError::Closed(_)) => DataStreamResetCode::SessionClosed,
            // Everything else, including an upstream object that ran short of
            // its declared payload length (`ServeError::Size`). Draft-16 added
            // MALFORMED_TRACK (0x12) for that case, but draft-14 §13.1.8 has no
            // equivalent, so it collapses into INTERNAL_ERROR here.
            _ => DataStreamResetCode::InternalError,
        }
    }

    async fn serve_subgroup_objects(
        header: data::SubgroupHeader,
        mut subgroup_reader: serve::SubgroupReader,
        output: &mut SubgroupOutput,
        state: State<SubscribedState>,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        tracing::debug!(
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
        while let Some(mut subgroup_object_reader) = subgroup_reader.next().await? {
            let subgroup_object = data::SubgroupObjectExt {
                object_id_delta: 0, // before delta logic, used to be subgroup_object_reader.object_id,
                extension_headers: subgroup_object_reader.extension_headers.clone(), // Pass through extension headers
                payload_length: subgroup_object_reader.size,
                status: if subgroup_object_reader.size == 0 {
                    // Only set status if payload length is zero
                    Some(subgroup_object_reader.status)
                } else {
                    None
                },
            };

            tracing::debug!(
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

        tracing::info!(
            "[PUBLISHER] serve_subgroup: completed subgroup (group_id={}, subgroup_id={:?}, {} objects sent)",
            subgroup_reader.group_id,
            subgroup_reader.subgroup_id,
            object_count
        );

        Ok(())
    }

    /// Test helper mirroring [`Self::serve_subgroup`]'s termination logic so tests
    /// can assert whether the stream would have been FINed or reset, without
    /// needing a real QUIC connection to open a stream on.
    #[cfg(test)]
    async fn serve_subgroup_to_parts(
        header: data::SubgroupHeader,
        subgroup_reader: serve::SubgroupReader,
        state: State<SubscribedState>,
    ) -> (
        bytes::BytesMut,
        Result<(), SessionError>,
        Option<SubgroupTermination>,
    ) {
        let mut output = SubgroupOutput::buffer();
        let res =
            Self::serve_subgroup_objects(header, subgroup_reader, &mut output, state, None).await;

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
    ) -> Result<(), SessionError> {
        tracing::debug!("[PUBLISHER] serve_datagrams: starting");

        let mut datagram_count = 0;
        while let Some(datagram) = datagrams.read().await? {
            // Determine datagram type based on extension headers presence
            let has_extension_headers = !datagram.extension_headers.is_empty();
            let datagram_type = if has_extension_headers {
                data::DatagramType::ObjectIdPayloadExt
            } else {
                data::DatagramType::ObjectIdPayload
            };

            let encoded_datagram = data::Datagram {
                datagram_type,
                track_alias: self.info.id, // use subscription id as track_alias
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

            tracing::debug!(
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

        tracing::info!(
            "[PUBLISHER] serve_datagrams: completed ({} datagrams sent)",
            datagram_count
        );

        Ok(())
    }
}

pub(super) struct SubscribedRecv {
    state: State<SubscribedState>,
}

impl SubscribedRecv {
    pub fn recv_unsubscribe(&mut self) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        if let Some(mut state) = state.into_mut() {
            state.closed = Err(ServeError::Cancel);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::{Buf, Bytes};

    use crate::coding::{Decode, TrackNamespace};

    /// Build a single-subgroup track reader carrying one complete object.
    async fn subgroup_with_one_object() -> (serve::SubgroupReader, data::SubgroupHeader) {
        let (track_writer, track_reader) =
            serve::Track::new(TrackNamespace::from_utf8_path("test"), "video".to_string())
                .produce();
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

    /// A fully delivered subgroup is the one case where draft-14 §10.4.3 permits
    /// a FIN.
    #[tokio::test]
    async fn complete_subgroup_is_finished_with_fin() {
        let (subgroup, header) = subgroup_with_one_object().await;
        let state = State::<SubscribedState>::default();

        let (_buffer, res, termination) =
            Subscribed::serve_subgroup_to_parts(header, subgroup, state).await;

        res.expect("serving a complete subgroup should succeed");
        assert_eq!(termination, Some(SubgroupTermination::Fin));
    }

    /// Draft-14 §10.4.3 lists UNSUBSCRIBE as a case that MUST reset rather than
    /// FIN, and the stream must not be cut inside an object.
    ///
    /// This is the regression test for the truncation bug: the forwarder used to
    /// encode an object header (promising `payload_length` bytes) and only then
    /// check whether the subscription was still alive. When an UNSUBSCRIBE landed
    /// in that window it returned early, dropped the `Writer`, and quinn
    /// implicitly FINed the stream mid-object.
    #[tokio::test]
    async fn unsubscribe_mid_subgroup_resets_at_an_object_boundary() {
        let (track_writer, track_reader) =
            serve::Track::new(TrackNamespace::from_utf8_path("test"), "video".to_string())
                .produce();
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
        let (unsubscribe_handle, state) = State::<SubscribedState>::default().split();

        let fut = Subscribed::serve_subgroup_to_parts(header.clone(), subgroup, state);
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
            Subscribed::reset_code_for(&ServeError::Done.into()),
            DataStreamResetCode::Cancelled
        );
        assert_eq!(
            Subscribed::reset_code_for(&ServeError::Cancel.into()),
            DataStreamResetCode::Cancelled
        );
        assert_eq!(
            Subscribed::reset_code_for(&ServeError::Closed(0x2).into()),
            DataStreamResetCode::SessionClosed
        );
        // Draft-14 §13.1.8 has no MALFORMED_TRACK, so an upstream object that ran
        // short of its declared length falls back to INTERNAL_ERROR.
        assert_eq!(
            Subscribed::reset_code_for(&ServeError::Size.into()),
            DataStreamResetCode::InternalError
        );
        assert_eq!(
            Subscribed::reset_code_for(&ServeError::internal_ctx("boom").into()),
            DataStreamResetCode::InternalError
        );
    }
}
