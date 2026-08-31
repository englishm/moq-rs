// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{ops, time::Duration};

use crate::{
    coding::{KeyValuePairs, Location, TrackName, TrackNamespace},
    data,
    message::{self, FilterType, GroupOrder, SubscriptionFilter},
    serve::{self, ServeError, TrackWriter, TrackWriterMode},
};

use crate::watch::State;

use super::SessionError;
use super::Subscriber;
use super::{DoneOutcome, StreamDrain};

const SUBSCRIBE_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);
type SubscribeRequestSink = tokio::sync::mpsc::UnboundedSender<message::Message>;

#[derive(Debug, Clone, Copy)]
pub struct DeliveryFilter {
    pub forward: bool,
    pub start_location: Option<Location>,
    pub end_group_id: Option<u64>,
}

impl DeliveryFilter {
    pub fn allows(&self, group_id: u64, object_id: u64) -> bool {
        if !self.forward {
            return false;
        }

        let location = Location::new(group_id, object_id);
        if let Some(start) = self.start_location {
            if location < start {
                return false;
            }
        }

        if let Some(end_group_id) = self.end_group_id {
            if group_id > end_group_id {
                return false;
            }
        }

        true
    }
}

// TODO rename to SubscriptionInfo when used for Publishes as well?
#[derive(Debug, Clone)]
pub struct SubscribeInfo {
    pub id: u64,
    pub track_namespace: TrackNamespace,
    pub track_name: TrackName,

    /// Subscriber Priority
    pub subscriber_priority: u8,
    pub group_order: GroupOrder,

    /// Forward Flag
    pub forward: bool,

    /// Filter type
    pub filter_type: FilterType,

    /// The starting location for this subscription. Only present for "AbsoluteStart" and "AbsoluteRange" filter types.
    pub start_location: Option<Location>,
    /// End group id, inclusive, for the subscription, if applicable. Only present for "AbsoluteRange" filter type.
    pub end_group_id: Option<u64>,

    /// None means the SUBSCRIPTION_FILTER parameter was omitted and the
    /// subscription is unfiltered.
    pub filter: Option<SubscriptionFilter>,

    /// Optional parameters
    pub params: KeyValuePairs,

    // Set to true if this is a track_status request only
    pub track_status: bool,
}

impl SubscribeInfo {
    pub fn new_from_subscribe(msg: &message::Subscribe) -> Result<Self, SessionError> {
        let filter = msg.params.subscription_filter()?;
        let filter_type = filter
            .as_ref()
            .map(|filter| filter.filter_type)
            .unwrap_or(FilterType::AbsoluteStart);
        let start_location = filter.as_ref().and_then(|filter| filter.start_location);
        let end_group_id = filter.as_ref().and_then(|filter| filter.end_group_id);
        msg.params.rendezvous_timeout()?;

        Ok(Self {
            id: msg.id,
            track_namespace: msg.track_namespace.clone(),
            track_name: msg.track_name.clone(),
            subscriber_priority: msg.params.subscriber_priority()?.unwrap_or(128),
            group_order: msg.params.group_order()?.unwrap_or(GroupOrder::Publisher),
            forward: msg.params.forward()?.unwrap_or(true),
            filter_type,
            start_location,
            end_group_id,
            filter,
            params: msg.params.clone(),
            track_status: false,
        })
    }

    pub fn delivery_filter(&self, largest_location: Option<Location>) -> DeliveryFilter {
        let Some(filter) = &self.filter else {
            return DeliveryFilter {
                forward: self.forward,
                start_location: None,
                end_group_id: None,
            };
        };

        let start_location = match filter.filter_type {
            FilterType::LargestObject => Some(next_object_location(largest_location)),
            FilterType::NextGroupStart => Some(next_group_location(largest_location)),
            FilterType::AbsoluteStart | FilterType::AbsoluteRange => filter.start_location,
        };

        DeliveryFilter {
            forward: self.forward,
            start_location,
            end_group_id: filter.end_group_id,
        }
    }
}

fn next_object_location(largest_location: Option<Location>) -> Location {
    let Some(location) = largest_location else {
        return Location::new(0, 0);
    };

    if let Some(object_id) = location.object_id.checked_add(1) {
        Location::new(location.group_id, object_id)
    } else {
        next_group_location(Some(location))
    }
}

fn next_group_location(largest_location: Option<Location>) -> Location {
    let Some(location) = largest_location else {
        return Location::new(0, 0);
    };

    Location::new(location.group_id.saturating_add(1), 0)
}

struct SubscribeState {
    ok: bool,
    peer_rejected: bool,
    track_alias: Option<u64>,
    closed: Result<(), ServeError>,
}

impl Default for SubscribeState {
    fn default() -> Self {
        Self {
            ok: Default::default(),
            peer_rejected: false,
            track_alias: None,
            closed: Ok(()),
        }
    }
}

// Held by the application
#[must_use = "unsubscribe on drop"]
pub struct Subscribe {
    state: State<SubscribeState>,
    subscriber: Subscriber,
    stream: Option<SubscribeRequestSink>,
    request_done: Option<tokio::sync::oneshot::Receiver<()>>,
    request_cancel: Option<tokio::sync::oneshot::Sender<u32>>,

    pub info: SubscribeInfo,
}

impl Subscribe {
    fn build_info(
        request_id: u64,
        track: &TrackWriter,
        params: KeyValuePairs,
    ) -> Result<(message::Subscribe, SubscribeInfo), SessionError> {
        let subscribe_message = message::Subscribe {
            id: request_id,
            track_namespace: track.namespace.clone(),
            track_name: track.name.clone(),
            params,
        };
        let info = SubscribeInfo::new_from_subscribe(&subscribe_message)?;
        Ok((subscribe_message, info))
    }

    /// Create a Subscribe without sending on the control stream, returning the
    /// wire message to send on the bidi request stream (draft-18) alongside it.
    /// The message is the one already built by `build_info`, so it is never
    /// constructed twice.
    pub(super) fn new(
        subscriber: Subscriber,
        request_id: u64,
        track: TrackWriter,
        params: KeyValuePairs,
    ) -> Result<(Subscribe, SubscribeRecv, message::Subscribe), SessionError> {
        let (msg, info) = Self::build_info(request_id, &track, params)?;
        let (send, recv) = Self::from_parts(subscriber, info, track);
        Ok((send, recv, msg))
    }

    fn from_parts(
        subscriber: Subscriber,
        info: SubscribeInfo,
        track: TrackWriter,
    ) -> (Subscribe, SubscribeRecv) {
        let (send, recv) = State::default().split();

        let send = Subscribe {
            state: send,
            subscriber,
            stream: None,
            request_done: None,
            request_cancel: None,
            info,
        };

        let recv = SubscribeRecv {
            state: recv,
            writer: Some(track.into()),
            default_publisher_priority: data::DEFAULT_PUBLISHER_PRIORITY,
            drain: StreamDrain::default(),
        };

        (send, recv)
    }

    pub(super) fn set_stream(
        &mut self,
        stream: SubscribeRequestSink,
        request_done: tokio::sync::oneshot::Receiver<()>,
        request_cancel: tokio::sync::oneshot::Sender<u32>,
    ) {
        self.stream = Some(stream);
        self.request_done = Some(request_done);
        self.request_cancel = Some(request_cancel);
    }

    /// End the subscription and wait until the peer terminates its request
    /// stream. This provides an ordering barrier before opening another
    /// SUBSCRIBE for the same track.
    pub async fn unsubscribe(mut self) {
        self.unsubscribe_before(SUBSCRIBE_TEARDOWN_TIMEOUT).await;
    }

    pub(super) async fn unsubscribe_before(&mut self, timeout: Duration) {
        self.send_unsubscribe();
        if let Some(mut request_done) = self.request_done.take() {
            if tokio::time::timeout(timeout, &mut request_done)
                .await
                .is_err()
            {
                tracing::warn!(
                    request_id = self.info.id,
                    "peer did not finish SUBSCRIBE request stream after UNSUBSCRIBE; resetting it"
                );
                if let Some(cancel) = self.request_cancel.take() {
                    let _ = cancel.send(super::CANCELLED_STREAM_CODE);
                }
                if tokio::time::timeout(timeout, &mut request_done)
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        request_id = self.info.id,
                        "SUBSCRIBE request stream did not stop after reset"
                    );
                }
            }
        }
        self.subscriber.remove_subscribe(self.info.id);
    }

    fn send_unsubscribe(&mut self) {
        let stream = self.stream.take();
        if self.state.lock().closed.is_ok() {
            if let Some(stream) = &stream {
                let _ = stream.send(message::Unsubscribe { id: self.info.id }.into());
            }
        }
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

    pub async fn ok(&self) -> Result<(), ServeError> {
        loop {
            {
                let state = self.state.lock();
                state.closed.clone()?;

                if state.ok {
                    return Ok(());
                }

                match state.modified() {
                    Some(notify) => notify,
                    None => return Err(ServeError::Done),
                }
            }
            .await;
        }
    }

    /// Whether the request ended with REQUEST_ERROR from the peer.
    pub fn peer_rejected(&self) -> bool {
        self.state.lock().peer_rejected
    }
}

impl Drop for Subscribe {
    fn drop(&mut self) {
        self.send_unsubscribe();
        self.subscriber.remove_subscribe(self.info.id);
    }
}

impl ops::Deref for Subscribe {
    type Target = SubscribeInfo;

    fn deref(&self) -> &SubscribeInfo {
        &self.info
    }
}

pub(super) struct SubscribeRecv {
    state: State<SubscribeState>,
    writer: Option<TrackWriterMode>,
    pub(super) default_publisher_priority: u8,
    /// PUBLISH_DONE Stream Count accounting (draft-18 §10.11).
    drain: StreamDrain<ServeError>,
}

impl SubscribeRecv {
    pub fn ok(&mut self, alias: u64, default_publisher_priority: u8) -> Result<(), ServeError> {
        let state = self.state.lock();
        if state.ok {
            return Err(ServeError::Duplicate);
        }

        if let Some(mut state) = state.into_mut() {
            state.ok = true;
            state.track_alias = Some(alias);
        }
        self.default_publisher_priority = default_publisher_priority;

        Ok(())
    }

    /// Record a data stream arriving for this subscription (§10.11 Stream
    /// Count). Must be paired with
    /// [`note_stream_finished`](Self::note_stream_finished).
    pub fn note_stream_received(&mut self) {
        self.drain.note_stream_received();
    }

    /// Record a data stream finishing.
    ///
    /// Returns `true` if this completed a deferred teardown, in which case the
    /// caller must release the subscription's session-level state.
    #[must_use]
    pub fn note_stream_finished(&mut self) -> bool {
        match self.drain.note_stream_finished() {
            Some(err) => {
                self.finish(err);
                true
            }
            None => false,
        }
    }

    /// Record PUBLISH_DONE (§10.11).
    ///
    /// The subscription is kept alive until the announced Stream Count has been
    /// received and every stream has closed, so Objects the publisher already
    /// sent are still delivered. See [`DoneOutcome`].
    pub fn recv_done(&mut self, err: ServeError, stream_count: u64) -> DoneOutcome {
        let (outcome, terminal) = self.drain.arm(err, stream_count);
        if let Some(err) = terminal {
            self.finish(err);
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
    /// PUBLISH_DONE does.
    pub fn abort(&mut self, err: ServeError) -> DoneOutcome {
        if self.drain.is_finished() {
            return DoneOutcome::Finished;
        }
        if self.drain.is_draining() {
            return DoneOutcome::AlreadyDraining;
        }

        let (outcome, terminal) = self.drain.arm(err, 0);
        if let Some(err) = terminal {
            self.finish(err);
        }
        outcome
    }

    /// Give up waiting for announced streams (§10.11 requires this backstop).
    ///
    /// Returns `true` if this call ended the subscription.
    pub fn drain_timeout(&mut self) -> bool {
        match self.drain.timeout() {
            Some(err) => {
                self.finish(err);
                true
            }
            None => false,
        }
    }

    /// Apply the terminal state, closing the writer so the reader sees the end
    /// of the track.
    pub(super) fn finish(&mut self, err: ServeError) {
        self.drain.mark_finished();
        if let Some(writer) = self.writer.take() {
            if let Err(err) = writer.close(err.clone()) {
                tracing::debug!(error = %err, "failed to close subscribe writer");
            }
        }

        let state = self.state.lock();
        if state.closed.is_err() {
            return;
        }
        if let Some(mut state) = state.into_mut() {
            state.closed = Err(err);
        }
    }

    pub fn error(mut self, err: ServeError) -> Result<(), ServeError> {
        if let Some(writer) = self.writer.take() {
            writer.close(err.clone())?;
        }

        let state = self.state.lock();
        state.closed.clone()?;

        let mut state = state.into_mut().ok_or(ServeError::Cancel)?;
        state.closed = Err(err);

        Ok(())
    }

    pub fn reject(self, err: ServeError) -> Result<(), ServeError> {
        if let Some(mut state) = self.state.lock().into_mut() {
            state.peer_rejected = true;
        }
        self.error(err)
    }

    pub fn subgroup(
        &mut self,
        header: data::SubgroupHeader,
    ) -> Result<serve::SubgroupWriter, ServeError> {
        let writer = self.writer.take().ok_or(ServeError::Done)?;

        // Every failure below restores the writer: one rejected subgroup must
        // not strand the track in a state where nothing can be written, which
        // would also lose the PUBLISH_DONE status code later.
        let mut subgroups = match writer {
            // TODO SLG - understand why both of these are needed, clock demo won't run if I comment out TrackWriteMode::Track
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
        let writer = match subgroups.create_with_metadata(
            serve::Subgroup {
                group_id: header.group_id,
                // When subgroup_id is not present in the header type, it implicitly means subgroup 0
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
                self.writer = Some(subgroups.into());
                return Err(err);
            }
        };

        self.writer = Some(subgroups.into());

        Ok(writer)
    }

    pub fn datagram(&mut self, datagram: data::Datagram) -> Result<(), ServeError> {
        let datagram = serve::Datagram::from_data(datagram, self.default_publisher_priority);
        let writer = self.writer.take().ok_or(ServeError::Done)?;

        match writer {
            TrackWriterMode::Track(track) => {
                // convert Track -> Datagrams writer, write, then put Datagrams back
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
                // preserve whatever unexpected mode was present, then report error
                self.writer = Some(other);
                Err(ServeError::Mode)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subscribe_info_with(params: KeyValuePairs) -> SubscribeInfo {
        SubscribeInfo::new_from_subscribe(&message::Subscribe {
            id: 0,
            track_namespace: TrackNamespace::from_utf8_path("test"),
            track_name: "track".into(),
            params,
        })
        .unwrap()
    }

    async fn test_recv_with_reader() -> (Subscribe, SubscribeRecv, serve::TrackReader) {
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscriber = crate::session::Subscriber::new(
            crate::watch::Queue::default(),
            crate::session::test_support::loopback_session().await,
            crate::session::Transport::WebTransport,
            None,
            crate::session::RequestId::new(0, 1),
            bidi_task_tx,
            Default::default(),
        );
        let (writer, reader) =
            crate::serve::Track::new(TrackNamespace::from_utf8_path("test"), "track").produce();
        let (send, recv, _msg) =
            Subscribe::new(subscriber, 0, writer, KeyValuePairs::default()).unwrap();
        (send, recv, reader)
    }

    async fn test_recv() -> (Subscribe, SubscribeRecv) {
        let (send, recv, _reader) = test_recv_with_reader().await;
        (send, recv)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn datagram_resolves_default_priority_and_preserves_semantics() {
        let (_send, mut recv, reader) = test_recv_with_reader().await;
        recv.ok(42, 200).unwrap();

        let mut properties = data::ExtensionHeaders::new();
        properties.set_intvalue(2, 7);
        recv.datagram(data::Datagram {
            datagram_type: data::DatagramType::PayloadExtEndOfGroupDefaultPriority,
            track_alias: 42,
            group_id: 3,
            object_id: None,
            publisher_priority: None,
            extension_headers: Some(properties),
            status: None,
            payload: Some(bytes::Bytes::from_static(b"payload")),
        })
        .unwrap();

        let serve::TrackReaderMode::Datagrams(mut datagrams) = reader.mode().await.unwrap() else {
            panic!("expected datagram mode");
        };
        let datagram = datagrams.read().await.unwrap().unwrap();
        assert_eq!(datagram.group_id, 3);
        assert_eq!(datagram.object_id, 0);
        assert_eq!(datagram.priority, 200);
        assert_eq!(datagram.status, data::ObjectStatus::NormalObject);
        assert!(datagram.end_of_group);
        assert_eq!(datagram.payload, bytes::Bytes::from_static(b"payload"));
        assert!(datagram.extension_headers.has(2));
    }

    /// §10.11: the request stream can die while a data stream is still being
    /// read. Tearing down here discards Objects already in flight — the same
    /// loss the drain exists to prevent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_waits_for_a_stream_still_being_read() {
        let (_send, mut recv) = test_recv().await;

        recv.note_stream_received();
        assert_eq!(
            recv.abort(ServeError::Cancel),
            DoneOutcome::DrainArmed,
            "teardown defers while a stream is mid-transfer"
        );
        assert!(
            recv.writer.is_some(),
            "the writer must survive so the in-flight Object can be written"
        );

        assert!(recv.note_stream_finished(), "closing the stream ends it");
        assert!(recv.writer.is_none());
    }

    /// With nothing in flight the subscription must fail immediately, so an
    /// application awaiting `Subscribe::ok()` is not left waiting for the life
    /// of the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_fails_an_idle_subscription_immediately() {
        let (send, mut recv) = test_recv().await;

        assert_eq!(recv.abort(ServeError::Cancel), DoneOutcome::Finished);
        assert_eq!(send.ok().await, Err(ServeError::Cancel));
    }

    /// A drain already carries the publisher's own status code, so an abort
    /// must not replace it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_leaves_an_armed_drain_alone() {
        let (send, mut recv) = test_recv().await;

        assert_eq!(
            recv.recv_done(ServeError::Closed(0x6), 4),
            DoneOutcome::DrainArmed
        );
        assert_eq!(recv.abort(ServeError::Cancel), DoneOutcome::AlreadyDraining);

        assert!(recv.drain_timeout());
        assert_eq!(send.closed().await, Err(ServeError::Closed(0x6)));
    }

    #[test]
    fn rendezvous_timeout_is_parsed_from_params() {
        let mut params = KeyValuePairs::default();
        params.set_rendezvous_timeout(30_000);

        assert_eq!(
            subscribe_info_with(params)
                .params
                .rendezvous_timeout()
                .unwrap(),
            Some(30_000)
        );
    }

    #[test]
    fn invalid_outbound_params_are_rejected() {
        let (writer, _reader) =
            crate::serve::Track::new(TrackNamespace::from_utf8_path("test"), "track").produce();
        let mut params = KeyValuePairs::default();
        params.set_bytesvalue(message::parameter_type::RENDEZVOUS_TIMEOUT, vec![1]);

        assert!(Subscribe::build_info(0, &writer, params).is_err());
    }

    /// Absent carries draft-18's default of 0, so a relay must not hold the
    /// subscription. Kept as None rather than defaulted to 0 so a relay can tell
    /// "not requested" from "explicitly zero" if it ever wants to.
    #[test]
    fn omitted_rendezvous_timeout_is_none() {
        assert_eq!(
            subscribe_info_with(KeyValuePairs::default())
                .params
                .rendezvous_timeout()
                .unwrap(),
            None
        );
    }

    /// Zero is meaningful and distinct from absent on the wire, even though both
    /// mean "answer immediately".
    #[test]
    fn explicit_zero_rendezvous_timeout_is_preserved() {
        let mut params = KeyValuePairs::default();
        params.set_rendezvous_timeout(0);

        assert_eq!(
            subscribe_info_with(params)
                .params
                .rendezvous_timeout()
                .unwrap(),
            Some(0)
        );
    }

    #[test]
    fn omitted_subscription_filter_is_unfiltered() {
        let info = subscribe_info_with(KeyValuePairs::default());
        let filter = info.delivery_filter(Some(Location::new(10, 20)));

        assert!(info.filter.is_none());
        assert!(filter.allows(0, 0));
        assert!(filter.allows(10, 20));
        assert!(filter.allows(100, 0));
    }

    #[test]
    fn largest_object_filter_starts_after_largest_object() {
        let mut params = KeyValuePairs::default();
        params
            .set_subscription_filter(&SubscriptionFilter::largest_object())
            .unwrap();
        let info = subscribe_info_with(params);
        let filter = info.delivery_filter(Some(Location::new(2, 3)));

        assert!(!filter.allows(2, 3));
        assert!(filter.allows(2, 4));
        assert!(filter.allows(3, 0));
    }

    #[test]
    fn absolute_range_filter_limits_start_and_end_group() {
        let mut params = KeyValuePairs::default();
        params
            .set_subscription_filter(&SubscriptionFilter {
                filter_type: FilterType::AbsoluteRange,
                start_location: Some(Location::new(2, 3)),
                end_group_id: Some(4),
            })
            .unwrap();
        let info = subscribe_info_with(params);
        let filter = info.delivery_filter(None);

        assert!(!filter.allows(2, 2));
        assert!(filter.allows(2, 3));
        assert!(filter.allows(4, 10));
        assert!(!filter.allows(5, 0));
    }

    #[test]
    fn forward_false_blocks_delivery() {
        let mut params = KeyValuePairs::default();
        params.set_forward(false);
        let info = subscribe_info_with(params);
        let filter = info.delivery_filter(None);

        assert!(!filter.allows(0, 0));
        assert!(!filter.allows(100, 100));
    }
}
