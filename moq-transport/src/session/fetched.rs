use crate::{message, serve::ServeError, watch::State};

use super::{FetchInfo, Publisher};

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
