use std::ops;

use crate::{message, serve::{self, ServeError, TrackReaderMode}, watch::State};

use super::{FetchInfo, Publisher, SessionError};

struct FetchedState {
    // TODO: do we need to track max group id or other state here?
    closed: Result<(), ServeError>,
}

impl Default for FetchedState {
    fn default() -> Self {
        Self { closed: Ok(()) }
    }
}

pub struct Fetched {
    publisher: Publisher,
    state: State<FetchedState>,
    msg: message::Fetch,
    ok: bool,

    pub info: FetchInfo,
}

impl ops::Deref for Fetched {
    type Target = FetchInfo;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

impl Fetched {
    pub(super) fn new(publisher: Publisher, msg: message::Fetch) -> (Self, FetchedRecv) {
        let (send, recv) = State::default().split();
        let info = FetchInfo {
            namespace: msg.track_namespace.clone(),
            name: msg.track_name.clone(),
        };
        let send = Self {
            publisher,
            state: send,
            msg,
            ok: false,
            info,
        };

        let recv = FetchedRecv { state: recv };

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

        // TODO: properly handle tracks with no objects yet
        let latest = track.latest().ok_or(ServeError::Cancel)?;

        //TODO:
        // - determine if end of track

        self.publisher.send_message(message::FetchOk {
            id: self.msg.id,
            group_order: self.msg.group_order.clone(),
            end_of_track: 0,
            largest_group_id: latest.0,
            largest_object_id: latest.1,
            params: self.msg.params.clone(),
        });

        self.ok = true;

        // TODO: alway serve Fetch responses on a single stream
        match track.mode().await? {
            // TODO cancel track/datagrams on closed
            TrackReaderMode::Stream(stream) => self.serve_track(stream).await,
            TrackReaderMode::Subgroups(subgroups) => self.serve_subgroups(subgroups).await,
            TrackReaderMode::Datagrams(datagrams) => self.serve_datagrams(datagrams).await,
        }
    }

    async fn serve_track(&mut self, mut _track: serve::StreamReader) -> Result<(), SessionError> {
        let mut _stream = self.publisher.open_uni().await?;

        todo!();
    }

    async fn serve_subgroups(&mut self, mut _track: serve::SubgroupsReader) -> Result<(), SessionError> {
        let mut _stream = self.publisher.open_uni().await?;

        todo!();
    }

    async fn serve_datagrams(&mut self, mut _track: serve::DatagramsReader) -> Result<(), SessionError> {
        let mut _stream = self.publisher.open_uni().await?;

        todo!();
    }

    pub fn close(self, err: ServeError) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        let mut state = state.into_mut().ok_or(ServeError::Done)?;
        state.closed = Err(err);

        Ok(())
    }
}

pub(super) struct FetchedRecv {
    state: State<FetchedState>,
}

impl FetchedRecv {
    pub fn recv_fetch_cancel(&mut self) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        if let Some(mut state) = state.into_mut() {
            state.closed = Err(ServeError::Cancel);
        }

        Ok(())
    }
}
