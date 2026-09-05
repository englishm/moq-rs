// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A track is a collection of semi-reliable and semi-ordered streams, split into a [Writer] and [Reader] handle.
//!
//! A [Writer] creates streams with a sequence number and priority.
//! The sequence number is used to determine the order of streams, while the priority is used to determine which stream to transmit first.
//! This may seem counter-intuitive, but is designed for live streaming where the newest streams may be higher priority.
//! A cloned [Writer] can be used to create streams in parallel, but will error if a duplicate sequence number is used.
//!
//! A [Reader] may not receive all streams in order or at all.
//! These streams are meant to be transmitted over congested networks and the key to MoQ Tranport is to not block on them.
//! streams will be cached for a potentially limited duration added to the unreliable nature.
//! A cloned [Reader] will receive a copy of all new stream going forward (fanout).
//!
//! The track is closed with [ServeError::Closed] when all writers or readers are dropped.

use crate::watch::State;

use super::{
    Datagrams, DatagramsReader, DatagramsWriter, ObjectsWriter, ServeError, Stream, StreamReader,
    StreamWriter, Subgroups, SubgroupsReader, SubgroupsWriter,
};
use crate::coding::{Location, TrackName, TrackNamespace};
use paste::paste;
use std::{ops::Deref, sync::Arc};

/// Static information about a track.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub namespace: TrackNamespace,
    pub name: TrackName,
}

impl Track {
    pub fn new(namespace: TrackNamespace, name: impl Into<TrackName>) -> Self {
        Self {
            namespace,
            name: name.into(),
        }
    }

    pub fn produce(self) -> (TrackWriter, TrackReader) {
        // Create sharable TrackState and Info(Track)
        let (writer_track_state, reader_track_state) = State::default().split();
        let info = Arc::new(self);

        // Create TrackReader and TrackWriter with shared state and info
        let writer = TrackWriter::new(writer_track_state, info.clone());
        let reader = TrackReader::new(reader_track_state, info);

        (writer, reader)
    }
}

struct TrackState {
    /// The ReaderMode for this track. Set to None on creation.
    reader_mode: Option<TrackReaderMode>,
    /// Watchable closed state
    closed: Result<(), ServeError>,
}

impl Default for TrackState {
    fn default() -> Self {
        Self {
            reader_mode: None,
            closed: Ok(()),
        }
    }
}

/// Creates new streams for a track.
pub struct TrackWriter {
    state: State<TrackState>,
    pub info: Arc<Track>,
}

impl TrackWriter {
    /// Create a track with the given name (info/Track)
    fn new(state: State<TrackState>, info: Arc<Track>) -> Self {
        Self { state, info }
    }

    /// Create a new stream with the given priority, inserting it into the track.
    pub fn stream(self, priority: u8) -> Result<StreamWriter, ServeError> {
        // Create new StreamWriter/StreamReader pair
        let (writer, reader) = Stream {
            track: self.info.clone(),
            priority,
        }
        .produce();

        // Lock state to modify it
        let mut state = self.state.lock_mut().ok_or_else(|| {
            tracing::debug!(
                namespace = %self.info.namespace,
                track = %self.info.name,
                "track state dropped (Cancel) in stream()"
            );
            ServeError::Cancel
        })?;

        // Set the Stream mode to TrackReaderMode::Stream
        state.reader_mode = Some(reader.into());
        Ok(writer)
    }

    // TODO: rework this whole interface for clarity?
    /// Create a new subgroups stream with the given priority, inserting it into the track.
    pub fn subgroups(self) -> Result<SubgroupsWriter, ServeError> {
        let (writer, reader) = Subgroups {
            track: self.info.clone(),
        }
        .produce();

        // Lock state to modify it
        let mut state = self.state.lock_mut().ok_or_else(|| {
            tracing::debug!(
                namespace = %self.info.namespace,
                track = %self.info.name,
                "track state dropped (Cancel) in subgroups()"
            );
            ServeError::Cancel
        })?;

        // Set the Stream mode to TrackReaderMode::Subgroups
        state.reader_mode = Some(reader.into());
        Ok(writer)
    }

    pub fn datagrams(self) -> Result<DatagramsWriter, ServeError> {
        let (writer, reader) = Datagrams {
            track: self.info.clone(),
        }
        .produce();

        // Lock state to modify it
        let mut state = self.state.lock_mut().ok_or_else(|| {
            tracing::debug!(
                namespace = %self.info.namespace,
                track = %self.info.name,
                "track state dropped (Cancel) in datagrams()"
            );
            ServeError::Cancel
        })?;

        // Set the Stream mode to TrackReaderMode::Datagrams
        state.reader_mode = Some(reader.into());
        Ok(writer)
    }

    /// Close the track with an error.
    pub fn close(self, err: ServeError) -> Result<(), ServeError> {
        tracing::debug!(
            namespace = %self.info.namespace,
            track = %self.info.name,
            error = %err,
            "track closing"
        );
        let state = self.state.lock();
        state.closed.clone()?;

        let mut state = state.into_mut().ok_or_else(|| {
            tracing::debug!(
                namespace = %self.info.namespace,
                track = %self.info.name,
                "track state already dropped during close"
            );
            ServeError::Cancel
        })?;
        state.closed = Err(err);
        Ok(())
    }
}

impl Deref for TrackWriter {
    type Target = Track;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

/// Receives new streams for a track.
#[derive(Clone)]
pub struct TrackReader {
    state: State<TrackState>,
    pub info: Arc<Track>,
}

impl TrackReader {
    fn new(state: State<TrackState>, info: Arc<Track>) -> Self {
        Self { state, info }
    }

    /// Get the current mode of the track, waiting if necessary.
    pub async fn mode(&self) -> Result<TrackReaderMode, ServeError> {
        loop {
            {
                let state = self.state.lock();
                if let Some(mode) = &state.reader_mode {
                    return Ok(mode.clone());
                }

                state.closed.clone()?;
                match state.modified() {
                    Some(notify) => notify,
                    None => return Err(ServeError::Done),
                }
            }
            .await;
        }
    }

    // Returns the largest group/sequence
    pub fn largest_location(&self) -> Option<Location> {
        let mode = self.state.lock().reader_mode.clone();
        mode.as_ref()
            .and_then(|mode| mode.latest())
            .map(|(group_id, object_id)| Location::new(group_id, object_id))
    }

    /// Wait until the track is closed, returning the closing error.
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

    /// Check if the track is closed or the writer has been dropped.
    /// This is used to detect stale cached TrackReaders that should not be reused.
    pub fn is_closed(&self) -> bool {
        let state = self.state.lock();

        if state.closed.is_err() {
            return true;
        }

        // Clone the mode out before dropping the TrackState lock to avoid
        // nested lock deadlocks (mode readers hold their own State locks).
        if let Some(mode) = state.reader_mode.clone() {
            // Mode has been set — the TrackWriter was consumed during the
            // Track→Subgroups/Stream/Datagrams transition. Liveness is now
            // determined by whether the mode-level writer is still alive.
            drop(state);
            return mode.is_closed();
        }

        // No mode set yet — check if the writer was abandoned before
        // transitioning to a specific mode.
        state.modified().is_none()
    }
}

impl Deref for TrackReader {
    type Target = Track;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

macro_rules! track_readers {
    {$($name:ident,)*} => {
		paste! {
			#[derive(Clone)]
			pub enum TrackReaderMode {
				$($name([<$name Reader>])),*
			}

			$(impl From<[<$name Reader>]> for TrackReaderMode {
				fn from(reader: [<$name Reader >]) -> Self {
					Self::$name(reader)
				}
			})*

			impl TrackReaderMode {
				pub fn latest(&self) -> Option<(u64, u64)> {
					match self {
						$(Self::$name(reader) => reader.latest(),)*
					}
				}

				pub fn is_closed(&self) -> bool {
					match self {
						$(Self::$name(reader) => reader.is_closed(),)*
					}
				}
			}
		}
	}
}

track_readers!(Stream, Subgroups, Datagrams,);

macro_rules! track_writers {
    {$($name:ident,)*} => {
		paste! {
			pub enum TrackWriterMode {
				$($name([<$name Writer>])),*
			}

			$(impl From<[<$name Writer>]> for TrackWriterMode {
				fn from(writer: [<$name Writer>]) -> Self {
					Self::$name(writer)
				}
			})*

			impl TrackWriterMode {
				pub fn close(self, err: ServeError) -> Result<(), ServeError>{
					match self {
						$(Self::$name(writer) => writer.close(err),)*
					}
				}
			}
		}
	}
}

track_writers!(Track, Stream, Subgroups, Objects, Datagrams,);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding::TrackNamespace;
    use crate::serve::Subgroup;

    #[test]
    fn test_is_closed_false_before_mode_set() {
        let track = Track::new(TrackNamespace::from_utf8_path("ns"), "t".to_string());
        let (_writer, reader) = track.produce();
        assert!(!reader.is_closed());
    }

    #[test]
    fn test_is_closed_true_when_writer_dropped_without_mode() {
        let track = Track::new(TrackNamespace::from_utf8_path("ns"), "t".to_string());
        let (writer, reader) = track.produce();
        drop(writer);
        assert!(reader.is_closed());
    }

    #[test]
    fn test_is_closed_true_when_explicitly_closed() {
        let track = Track::new(TrackNamespace::from_utf8_path("ns"), "t".to_string());
        let (writer, reader) = track.produce();
        writer.close(ServeError::Cancel).unwrap();
        assert!(reader.is_closed());
    }

    #[test]
    fn test_is_closed_false_after_subgroups_transition_while_writer_alive() {
        let track = Track::new(TrackNamespace::from_utf8_path("ns"), "t".to_string());
        let (writer, reader) = track.produce();

        let _subgroups_writer = writer
            .subgroups()
            .expect("subgroups transition should succeed");

        assert!(
            !reader.is_closed(),
            "track should NOT be closed while SubgroupsWriter is alive"
        );
    }

    #[test]
    fn test_is_closed_true_after_subgroups_writer_dropped() {
        let track = Track::new(TrackNamespace::from_utf8_path("ns"), "t".to_string());
        let (writer, reader) = track.produce();

        let subgroups_writer = writer
            .subgroups()
            .expect("subgroups transition should succeed");
        drop(subgroups_writer);

        assert!(
            reader.is_closed(),
            "track should be closed after SubgroupsWriter is dropped"
        );
    }

    #[test]
    fn test_is_closed_true_after_subgroups_writer_explicitly_closed() {
        let track = Track::new(TrackNamespace::from_utf8_path("ns"), "t".to_string());
        let (writer, reader) = track.produce();

        let subgroups_writer = writer
            .subgroups()
            .expect("subgroups transition should succeed");
        subgroups_writer.close(ServeError::Cancel).unwrap();

        assert!(
            reader.is_closed(),
            "track should be closed after SubgroupsWriter is explicitly closed"
        );
    }

    #[test]
    fn test_is_closed_false_after_stream_transition_while_writer_alive() {
        let track = Track::new(TrackNamespace::from_utf8_path("ns"), "t".to_string());
        let (writer, reader) = track.produce();

        let _stream_writer = writer.stream(0).expect("stream transition should succeed");

        assert!(
            !reader.is_closed(),
            "track should NOT be closed while StreamWriter is alive"
        );
    }

    #[test]
    fn test_is_closed_true_after_stream_writer_dropped() {
        let track = Track::new(TrackNamespace::from_utf8_path("ns"), "t".to_string());
        let (writer, reader) = track.produce();

        let stream_writer = writer.stream(0).expect("stream transition should succeed");
        drop(stream_writer);

        assert!(
            reader.is_closed(),
            "track should be closed after StreamWriter is dropped"
        );
    }

    #[test]
    fn test_is_closed_false_after_datagrams_transition_while_writer_alive() {
        let track = Track::new(TrackNamespace::from_utf8_path("ns"), "t".to_string());
        let (writer, reader) = track.produce();

        let _datagrams_writer = writer
            .datagrams()
            .expect("datagrams transition should succeed");

        assert!(
            !reader.is_closed(),
            "track should NOT be closed while DatagramsWriter is alive"
        );
    }

    #[test]
    fn test_is_closed_true_after_datagrams_writer_dropped() {
        let track = Track::new(TrackNamespace::from_utf8_path("ns"), "t".to_string());
        let (writer, reader) = track.produce();

        let datagrams_writer = writer
            .datagrams()
            .expect("datagrams transition should succeed");
        drop(datagrams_writer);

        assert!(
            reader.is_closed(),
            "track should be closed after DatagramsWriter is dropped"
        );
    }

    #[test]
    fn test_is_closed_false_while_subgroups_actively_writing() {
        let track = Track::new(TrackNamespace::from_utf8_path("ns"), "t".to_string());
        let (writer, reader) = track.produce();

        let mut subgroups_writer = writer
            .subgroups()
            .expect("subgroups transition should succeed");

        let _subgroup_writer = subgroups_writer
            .create(Subgroup {
                group_id: 0,
                subgroup_id: 0,
                priority: 0,
            })
            .expect("create subgroup should succeed");

        assert!(
            !reader.is_closed(),
            "track should NOT be closed while actively writing subgroups"
        );
    }
}
