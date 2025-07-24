//! A broadcast is a collection of tracks, split into two handles: [Writer] and [Reader].
//!
//! The [Writer] can create tracks, either manually or on request.
//! It receives all requests by a [Reader] for a tracks that don't exist.
//! The simplest implementation is to close every unknown track with [ServeError::NotFound].
//!
//! A [Reader] can request tracks by name.
//! If the track already exists, it will be returned.
//! If the track doesn't exist, it will be sent to [Unknown] to be handled.
//! A [Reader] can be cloned to create multiple subscriptions.
//!
//! The broadcast is automatically closed with [ServeError::Done] when [Writer] is dropped, or all [Reader]s are dropped.
use std::{collections::HashMap, ops::Deref, sync::Arc};

use super::{ServeError, Track, TrackReader, TrackWriter};
use crate::coding::{Tuple, TupleField};
use crate::watch::{Queue, State};

/// Static information about a broadcast.
#[derive(Debug)]
pub struct Tracks {
    pub namespace: Tuple,
}

impl Tracks {
    pub fn new(namespace: Tuple) -> Self {
        Self { namespace }
    }

    pub fn produce(self) -> (TracksWriter, TracksRequest, TracksReader) {
        let info = Arc::new(self);
        let state = State::default().split();
        let queue = Queue::default().split();

        let writer = TracksWriter::new(state.0.clone(), info.clone());
        let request = TracksRequest::new(state.0, queue.0, info.clone());
        let reader = TracksReader::new(state.1, queue.1, info);

        (writer, request, reader)
    }
}

#[derive(Default)]
pub struct TracksState {
    tracks: HashMap<TupleField, TrackReader>,
}

/// Publish new tracks for a broadcast by name.
pub struct TracksWriter {
    state: State<TracksState>,
    pub info: Arc<Tracks>,
}

impl TracksWriter {
    fn new(state: State<TracksState>, info: Arc<Tracks>) -> Self {
        Self { state, info }
    }

    /// Create a new track with the given name, inserting it into the broadcast.
    /// None is returned if all [TracksReader]s have been dropped.
    /// Note: Expects a UTF-8 track name which is internally represented as a TupleField
    pub fn create(&mut self, track: &str) -> Option<TrackWriter> {
        let (writer, reader) = Track {
            namespace: self.namespace.clone(),
            name: TupleField::from_utf8(track),
        }
        .produce();

        let track_tuplefield = TupleField::from_utf8(track);

        // NOTE: We overwrite the track if it already exists.
        self.state
            .lock_mut()?
            .tracks
            .insert(track_tuplefield, reader);

        Some(writer)
    }

    pub fn remove(&mut self, track: &str) -> Option<TrackReader> {
        let track_tuplefield = TupleField::from_utf8(track);
        self.state.lock_mut()?.tracks.remove(&track_tuplefield)
    }
}

impl Deref for TracksWriter {
    type Target = Tracks;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

pub struct TracksRequest {
    #[allow(dead_code)] // Avoid dropping the write side
    state: State<TracksState>,
    incoming: Option<Queue<TrackWriter>>,
    pub info: Arc<Tracks>,
}

impl TracksRequest {
    fn new(state: State<TracksState>, incoming: Queue<TrackWriter>, info: Arc<Tracks>) -> Self {
        Self {
            state,
            incoming: Some(incoming),
            info,
        }
    }

    /// Wait for a request to create a new track.
    /// None is returned if all [TracksReader]s have been dropped.
    pub async fn next(&mut self) -> Option<TrackWriter> {
        self.incoming.as_mut()?.pop().await
    }
}

impl Deref for TracksRequest {
    type Target = Tracks;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

impl Drop for TracksRequest {
    fn drop(&mut self) {
        // Close any tracks still in the Queue
        for track in self.incoming.take().unwrap().close() {
            let _ = track.close(ServeError::NotFound);
        }
    }
}

/// Subscribe to a broadcast by requesting tracks.
///
/// This can be cloned to create handles.
#[derive(Clone)]
pub struct TracksReader {
    state: State<TracksState>,
    queue: Queue<TrackWriter>,
    pub info: Arc<Tracks>,
}

impl TracksReader {
    fn new(state: State<TracksState>, queue: Queue<TrackWriter>, info: Arc<Tracks>) -> Self {
        Self { state, queue, info }
    }

    /// Get or request a track from the broadcast by name.
    /// None is returned if [TracksWriter] or [TracksRequest] cannot fufill the request.
    pub fn subscribe(&mut self, name: &str) -> Option<TrackReader> {
        let state = self.state.lock();

        let tuplefield_name = TupleField::from_utf8(name);

        if let Some(track) = state.tracks.get(&tuplefield_name) {
            return Some(track.clone());
        }

        let mut state = state.into_mut()?;
        let track = Track {
            namespace: self.namespace.clone(),
            name: TupleField::from_utf8(name),
        }
        .produce();

        if self.queue.push(track.0).is_err() {
            return None;
        }

        let name_tuplefield = TupleField::from_utf8(name);

        // We requested the track sucessfully so we can deduplicate it.
        state.tracks.insert(name_tuplefield, track.1.clone());

        Some(track.1.clone())
    }
}

impl Deref for TracksReader {
    type Target = Tracks;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}
