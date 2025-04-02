use std::ops;

use crate::{
    coding::Tuple,
    data,
    message::{self, FilterType, GroupOrder, SubscribePair},
    serve::{self, ServeError, TrackWriter, TrackWriterMode},
};

use crate::watch::State;

use super::Subscriber;

#[derive(Debug, Clone)]
pub struct SubscribeInfo {
    pub namespace: Tuple,
    pub name: String,
}

struct SubscribeState {
    ok: bool,
    closed: Result<(), ServeError>,
}

impl Default for SubscribeState {
    fn default() -> Self {
        Self {
            ok: Default::default(),
            closed: Ok(()),
        }
    }
}

// Held by the application
#[must_use = "unsubscribe on drop"]
pub struct Subscribe {
    state: State<SubscribeState>,
    subscriber: Subscriber,
    id: u64,

    pub info: SubscribeInfo,
}

#[derive(Debug, Clone)]
pub enum SubscribeFilter {
    AbsoluteStart(SubscribePair),
    AbsoluteRange(SubscribePair, u64),
    LatestObject
}

impl SubscribeFilter {
    fn unwrap(self) -> (FilterType, Option<SubscribePair>, Option<u64>)  {
        match self {
            SubscribeFilter::AbsoluteStart(start) => (FilterType::AbsoluteStart, Some(start), None),
            SubscribeFilter::AbsoluteRange(start, end_group) => (FilterType::AbsoluteRange, Some(start), Some(end_group)),
            SubscribeFilter::LatestObject => (FilterType::LatestObject, None, None),
        }
    }
}

impl From<&message::Subscribe> for SubscribeFilter {
    fn from(subscribe: &message::Subscribe) -> Self {
        match subscribe.filter_type {
            FilterType::AbsoluteStart => Self::AbsoluteStart(
                subscribe.start.clone().expect("AbsoluteStart, but no StartGroup nor StartObject")
            ),
            FilterType::AbsoluteRange => Self::AbsoluteRange(
                subscribe.start.clone().expect("AbsoluteRange, but no StartGroup nor StartObject"),
                subscribe.end_group.clone().expect("AbsoluteRange, but no EndGroup"),
            ),
            FilterType::LatestObject => Self::LatestObject,
        }
    }
}

impl From<&message::SubscribeUpdate> for SubscribeFilter {
    fn from(update: &message::SubscribeUpdate) -> Self {
        if update.end_group != 0 {
            Self::AbsoluteRange(update.start.clone(), update.end_group - 1)
        } else if !(update.start.group == 0 && update.start.object == 0) {
            Self::AbsoluteStart(update.start.clone()) // A value of 0 means the subscription is open-ended.
        } else {
            Self::LatestObject // If starts from the beginning and is open-ended, it must be LatestObject.
        }
    }
}

impl Subscribe {
    pub(super) fn new(
        mut subscriber: Subscriber,
        id: u64,
        track: TrackWriter,
        filter: SubscribeFilter,
    ) -> (Subscribe, SubscribeRecv) {
        let (filter_type, start, end_group) = filter.clone().unwrap();
        subscriber.send_message(message::Subscribe {
            id,
            track_alias: id,
            track_namespace: track.namespace.clone(),
            track_name: track.name.clone(),
            // TODO add prioritization logic on the publisher side
            subscriber_priority: 127, // default to mid value, see: https://github.com/moq-wg/moq-transport/issues/504
            group_order: GroupOrder::Publisher, // defer to publisher send order
            filter_type,
            start,
            end_group,
            params: Default::default(),
        });

        let info = SubscribeInfo {
            namespace: track.namespace.clone(),
            name: track.name.clone(),
        };

        let (send, recv) = State::default().split();

        let send = Subscribe {
            state: send,
            subscriber,
            id,
            info,
        };

        let recv = SubscribeRecv {
            state: recv,
            writer: Some(track.into()),
        };

        (send, recv)
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

impl Drop for Subscribe {
    fn drop(&mut self) {
        self.subscriber
            .send_message(message::Unsubscribe { id: self.id });
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
    pub fn ok(&mut self) -> Result<(), ServeError> {
        let state = self.state.lock();
        if state.ok {
            return Err(ServeError::Duplicate);
        }

        if let Some(mut state) = state.into_mut() {
            state.ok = true;
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

    pub fn track(&mut self, header: data::TrackHeader) -> Result<serve::StreamWriter, ServeError> {
        let writer = self.writer.take().ok_or(ServeError::Done)?;

        let stream = match writer {
            TrackWriterMode::Track(init) => init.stream(header.publisher_priority)?,
            _ => return Err(ServeError::Mode),
        };

        self.writer = Some(stream.clone().into());

        Ok(stream)
    }

    pub fn subgroup(
        &mut self,
        header: data::SubgroupHeader,
    ) -> Result<serve::SubgroupWriter, ServeError> {
        let writer = self.writer.take().ok_or(ServeError::Done)?;

        let mut subgroups = match writer {
            TrackWriterMode::Track(init) => init.groups()?,
            TrackWriterMode::Subgroups(subgroups) => subgroups,
            _ => return Err(ServeError::Mode),
        };

        let writer = subgroups.create(serve::Subgroup {
            group_id: header.group_id,
            subgroup_id: header.subgroup_id,
            priority: header.publisher_priority,
        })?;

        self.writer = Some(subgroups.into());

        Ok(writer)
    }

    pub fn datagram(&mut self, datagram: data::Datagram) -> Result<(), ServeError> {
        let writer = self.writer.take().ok_or(ServeError::Done)?;

        let mut datagrams = match writer {
            TrackWriterMode::Track(init) => init.datagrams()?,
            TrackWriterMode::Datagrams(datagrams) => datagrams,
            _ => return Err(ServeError::Mode),
        };

        datagrams.write(serve::Datagram {
            group_id: datagram.group_id,
            object_id: datagram.object_id,
            priority: datagram.publisher_priority,
            status: datagram.object_status,
            payload: datagram.payload,
        })?;

        Ok(())
    }
}
