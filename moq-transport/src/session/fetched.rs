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
