use std::ops;

use super::Subscriber;
use crate::{
    coding::Tuple,
    data::FetchHeader,
    message::{self, GroupOrder},
    serve::{ServeError, StreamWriter, TrackWriter, TrackWriterMode},
    watch::State,
};

struct FetchState {
    ok: bool,
    closed: Result<(), ServeError>,
}

impl Default for FetchState {
    fn default() -> Self {
        Self {
            ok: Default::default(),
            closed: Ok(()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchInfo {
    pub namespace: Tuple,
    pub name: String,
}

#[must_use = "unsubscribe on drop"]
pub struct Fetch {
    state: State<FetchState>,
    subscriber: Subscriber,
    id: u64,

    pub info: FetchInfo,
}

impl Fetch {
    pub(super) fn new(
        mut subscriber: Subscriber,
        id: u64,
        track: TrackWriter,
    ) -> (Fetch, FetchRecv) {
        subscriber.send_message(message::Fetch {
            id,
            track_namespace: track.namespace.clone(),
            track_name: track.name.clone(),
            start_group: 0,
            start_object: 0,
            end_group: 2,
            end_object: 0,
            group_order: GroupOrder::Ascending,
            subscriber_priority: 127,
            params: Default::default(),
        });

        let info = FetchInfo {
            namespace: track.namespace.clone(),
            name: track.name.clone(),
        };

        let (send, recv) = State::default().split();

        let send = Fetch {
            state: send,
            subscriber,
            id,
            info,
        };

        let recv = FetchRecv {
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

impl Drop for Fetch {
    fn drop(&mut self) {
        self.subscriber
            .send_message(message::FetchCancel { id: self.id })
    }
}

impl ops::Deref for Fetch {
    type Target = FetchInfo;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

pub(super) struct FetchRecv {
    state: State<FetchState>,
    writer: Option<TrackWriterMode>,
}

impl FetchRecv {
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

    pub fn stream(&mut self, header: FetchHeader) -> Result<StreamWriter, ServeError> {
        // TODO: rework all this silliness
        // (Fetches break a lot of assumptions in this code and doing this right is a lot of work)

        let writer = self.writer.take().ok_or(ServeError::Done)?;

        let stream = match writer {
            TrackWriterMode::Track(init) => init.stream(0)?,
            TrackWriterMode::Stream(stream) => stream,
            _ => {
                log::debug!("here: {}:{}", file!(), line!());
                log::debug!("writer type: {}", std::any::type_name_of_val(&writer));
                return Err(ServeError::Mode)
            },
        };

        let writer = stream.clone();

        self.writer = Some(stream.into());

        Ok(writer)
    }
}
