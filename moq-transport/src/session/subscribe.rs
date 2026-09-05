// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ops;

use crate::{
    coding::{KeyValuePairs, Location, TrackName, TrackNamespace},
    data,
    message::{self, FilterType, GroupOrder, SubscriptionFilter},
    serve::{self, ServeError, TrackWriter, TrackWriterMode},
};

use crate::watch::State;

use super::SessionError;
use super::Subscriber;

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
    /// subscription is unfiltered per draft-16 §9.2.2.5.
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
    track_alias: Option<u64>,
    closed: Result<(), ServeError>,
}

impl Default for SubscribeState {
    fn default() -> Self {
        Self {
            ok: Default::default(),
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

    pub info: SubscribeInfo,
}

impl Subscribe {
    pub(super) fn new(
        subscriber: Subscriber,
        request_id: u64,
        track: TrackWriter,
    ) -> (Subscribe, SubscribeRecv) {
        let subscribe_message = message::Subscribe {
            id: request_id,
            track_namespace: track.namespace.clone(),
            track_name: track.name.clone(),
            params: KeyValuePairs::default(),
        };
        let info = SubscribeInfo::new_from_subscribe(&subscribe_message).unwrap_or_else(|err| {
            tracing::warn!(session_id = %subscriber.session_id(), error = %err, "failed to decode outbound subscribe parameters");
            SubscribeInfo {
                id: request_id,
                track_namespace: track.namespace.clone(),
                track_name: track.name.clone(),
                subscriber_priority: 128,
                group_order: GroupOrder::Publisher,
                forward: true,
                filter_type: FilterType::AbsoluteStart,
                start_location: None,
                end_group_id: None,
                filter: None,
                params: Default::default(),
                track_status: false,
            }
        });

        let (send, recv) = State::default().split();

        let send = Subscribe {
            state: send,
            subscriber,
            info,
        };

        let recv = SubscribeRecv {
            state: recv,
            writer: Some(track.into()),
        };

        (send, recv)
    }

    pub(super) fn send_request(&mut self) {
        self.subscriber.send_message(message::Subscribe {
            id: self.info.id,
            track_namespace: self.info.track_namespace.clone(),
            track_name: self.info.track_name.clone(),
            params: self.info.params.clone(),
        });
    }

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
}

impl Drop for Subscribe {
    fn drop(&mut self) {
        self.subscriber
            .send_message(message::Unsubscribe { id: self.info.id });
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
}

impl SubscribeRecv {
    pub fn ok(&mut self, alias: u64) -> Result<(), ServeError> {
        let state = self.state.lock();
        if state.ok {
            return Err(ServeError::Duplicate);
        }

        if let Some(mut state) = state.into_mut() {
            state.ok = true;
            state.track_alias = Some(alias);
        }

        Ok(())
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

    pub fn subgroup(
        &mut self,
        header: data::SubgroupHeader,
    ) -> Result<serve::SubgroupWriter, ServeError> {
        let writer = self.writer.take().ok_or(ServeError::Done)?;

        let mut subgroups = match writer {
            // TODO SLG - understand why both of these are needed, clock demo won't run if I comment out TrackWriteMode::Track
            TrackWriterMode::Track(track) => track.subgroups()?,
            TrackWriterMode::Subgroups(subgroups) => subgroups,
            _ => return Err(ServeError::Mode),
        };

        let writer = subgroups.create(serve::Subgroup {
            group_id: header.group_id,
            // When subgroup_id is not present in the header type, it implicitly means subgroup 0
            subgroup_id: header.subgroup_id.unwrap_or(0),
            priority: header.publisher_priority,
        })?;

        self.writer = Some(subgroups.into());

        Ok(writer)
    }

    pub fn datagram(&mut self, datagram: data::Datagram) -> Result<(), ServeError> {
        let writer = self.writer.take().ok_or(ServeError::Done)?;

        match writer {
            TrackWriterMode::Track(track) => {
                // convert Track -> Datagrams writer, write, then put Datagrams back
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

    #[tokio::test]
    async fn closed_returns_ok_for_track_ended() {
        let rid = crate::session::RequestId::new(0, 100, 100, 0);
        let subscriber = crate::session::Subscriber::new(
            crate::session::Queue::default(),
            crate::session::Queue::default(),
            None,
            rid,
            crate::session::PendingRequests::default(),
            crate::session::SessionId::generate(),
        );
        let (writer, _reader) =
            serve::Track::new(TrackNamespace::from_utf8_path("test"), "track").produce();
        let (subscribe, recv) = Subscribe::new(subscriber, 1, writer);

        recv.error(ServeError::Done).unwrap();

        assert_eq!(subscribe.closed().await, Ok(()));
    }

    #[tokio::test]
    async fn closed_returns_error_for_non_track_ended() {
        let rid = crate::session::RequestId::new(0, 100, 100, 0);
        let subscriber = crate::session::Subscriber::new(
            crate::session::Queue::default(),
            crate::session::Queue::default(),
            None,
            rid,
            crate::session::PendingRequests::default(),
            crate::session::SessionId::generate(),
        );
        let (writer, _reader) =
            serve::Track::new(TrackNamespace::from_utf8_path("test"), "track").produce();
        let (subscribe, recv) = Subscribe::new(subscriber, 1, writer);

        recv.error(ServeError::Closed(message::PublishDoneCode::Expired as u64))
            .unwrap();

        assert!(matches!(
            subscribe.closed().await,
            Err(ServeError::Closed(code)) if code == message::PublishDoneCode::Expired as u64
        ));
    }
}
