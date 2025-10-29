use std::ops;
use std::sync::{Arc, Mutex};

use futures::stream::FuturesUnordered;
use futures::StreamExt;

use crate::coding::{Encode, KeyValuePairs, Location, ReasonPhrase};
use crate::mlog;
use crate::serve::{ServeError, TrackReaderMode};
use crate::watch::State;
use crate::{data, message, serve};

use super::{Publisher, SessionError, SubscribeInfo, Writer};

// This file defines Publisher handling of inbound Subscriptions

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

    /// The Subscribe request message that created this subscription
    msg: message::Subscribe,

    /// The tracknamespace and trackname for the subscription.
    /// TODO SLG - is this needed? we have this information in the stored Subscribe
    ///            message.
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
        let info = SubscribeInfo {
            namespace: msg.track_namespace.clone(),
            name: msg.track_name.clone(),
        };

        let send = Self {
            publisher,
            state: send,
            msg,
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
        let largest_location = track.largest();
        self.state
            .lock_mut()
            .ok_or(ServeError::Cancel)?
            .largest_location = largest_location;

        // Send SubscribeOk using send_message_and_wait to ensure it is sent before
        // we start serving the track.  If a subscriber get's the stream before SubscribeOk
        // then they won't recognize the track_alias in the stream header.
        self.publisher
            .send_message_and_wait(message::SubscribeOk {
                id: self.msg.id,
                track_alias: self.msg.id, // use subscription id as track alias
                expires: 0,               // TODO SLG
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
                id: self.msg.id,
                status_code: err.code(),
                stream_count: 0, // TODO SLG
                reason: ReasonPhrase(err.to_string()),
            });
        } else {
            self.publisher.send_message(message::SubscribeError {
                id: self.msg.id,
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
                            track_alias: self.msg.id, // use subscription id as track_alias
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
                                log::warn!("failed to serve subgroup: {:?}, error: {}", info, err);
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
        state: State<SubscribedState>,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        log::debug!(
            "[PUBLISHER] serve_subgroup: starting - group_id={}, subgroup_id={:?}, priority={}",
            subgroup_reader.group_id,
            subgroup_reader.subgroup_id,
            subgroup_reader.priority
        );

        let mut send_stream = publisher.open_uni().await?;
        log::trace!("[PUBLISHER] serve_subgroup: opened unidirectional stream");

        // TODO figure out u32 vs u64 priority
        send_stream.set_priority(subgroup_reader.priority as i32);

        let mut writer = Writer::new(send_stream);

        log::debug!(
            "[PUBLISHER] serve_subgroup: sending header - track_alias={}, group_id={}, subgroup_id={:?}, priority={}, header_type={:?}",
            header.track_alias,
            header.group_id,
            header.subgroup_id,
            header.publisher_priority,
            header.header_type
        );

        writer.encode(&header).await?;

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
                extension_headers: KeyValuePairs::new(), // TODO SLG - empty for now
                payload_length: subgroup_object_reader.size,
                status: if subgroup_object_reader.size == 0 {
                    // Only set status if payload length is zero
                    Some(subgroup_object_reader.status)
                } else {
                    None
                },
            };

            log::debug!(
                "[PUBLISHER] serve_subgroup: sending object #{} - object_id={}, object_id_delta={}, payload_length={}, status={:?}",
                object_count + 1,
                subgroup_object_reader.object_id,
                subgroup_object.object_id_delta,
                subgroup_object.payload_length,
                subgroup_object.status
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
                log::trace!(
                    "[PUBLISHER] serve_subgroup: sending payload chunk #{} for object #{} ({} bytes)",
                    chunks_sent + 1,
                    object_count + 1,
                    chunk.len()
                );
                bytes_sent += chunk.len();
                writer.write(&chunk).await?;
                chunks_sent += 1;
            }

            log::trace!(
                "[PUBLISHER] serve_subgroup: completed object #{} ({} chunks, {} bytes total)",
                object_count + 1,
                chunks_sent,
                bytes_sent
            );
            object_count += 1;
        }

        log::info!(
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
    ) -> Result<(), SessionError> {
        log::debug!("[PUBLISHER] serve_datagrams: starting");

        let mut datagram_count = 0;
        while let Some(datagram) = datagrams.read().await? {
            let encoded_datagram = data::Datagram {
                datagram_type: data::DatagramType::ObjectIdPayload, // TODO SLG
                track_alias: self.msg.id, // use subscription id as track_alias
                group_id: datagram.group_id,
                object_id: Some(datagram.object_id),
                publisher_priority: datagram.priority,
                extension_headers: None,
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

            log::debug!(
                "[PUBLISHER] serve_datagrams: sending datagram #{} - group_id={}, object_id={}, priority={}, payload_len={}, total_encoded_len={}",
                datagram_count + 1,
                encoded_datagram.group_id,
                encoded_datagram.object_id.unwrap(),
                encoded_datagram.publisher_priority,
                payload_len,
                buffer.len()
            );

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

        log::info!(
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
