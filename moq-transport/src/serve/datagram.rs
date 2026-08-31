// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{fmt, sync::Arc};

use crate::{data, watch::State};

use super::{ServeError, Track};

pub struct Datagrams {
    pub track: Arc<Track>,
}

impl Datagrams {
    pub fn produce(self) -> (DatagramsWriter, DatagramsReader) {
        let (writer, reader) = State::default().split();

        let writer = DatagramsWriter::new(writer, self.track.clone());
        let reader = DatagramsReader::new(reader, self.track);

        (writer, reader)
    }
}

struct DatagramsState {
    // The latest datagram
    latest: Option<Datagram>,

    // The greatest Object location observed, independent of arrival order.
    largest: Option<(u64, u64)>,

    // Increased each time datagram changes.
    epoch: u64,

    // Set when the writer or all readers are dropped.
    closed: Result<(), ServeError>,
}

impl Default for DatagramsState {
    fn default() -> Self {
        Self {
            latest: None,
            largest: None,
            epoch: 0,
            closed: Ok(()),
        }
    }
}

pub struct DatagramsWriter {
    state: State<DatagramsState>,
    pub track: Arc<Track>,
}

impl DatagramsWriter {
    fn new(state: State<DatagramsState>, track: Arc<Track>) -> Self {
        Self { state, track }
    }

    pub fn write(&mut self, datagram: Datagram) -> Result<(), ServeError> {
        let mut state = self.state.lock_mut().ok_or(ServeError::Cancel)?;

        if datagram.status == data::ObjectStatus::NormalObject {
            let location = (datagram.group_id, datagram.object_id);
            state.largest = Some(
                state
                    .largest
                    .map_or(location, |largest| largest.max(location)),
            );
        }
        state.latest = Some(datagram);
        state.epoch += 1;

        Ok(())
    }

    pub fn close(self, err: ServeError) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        let mut state = state.into_mut().ok_or(ServeError::Cancel)?;
        state.closed = Err(err);

        Ok(())
    }
}

#[derive(Clone)]
pub struct DatagramsReader {
    state: State<DatagramsState>,
    pub track: Arc<Track>,

    epoch: u64,
}

impl DatagramsReader {
    fn new(state: State<DatagramsState>, track: Arc<Track>) -> Self {
        Self {
            state,
            track,
            epoch: 0,
        }
    }

    pub async fn read(&mut self) -> Result<Option<Datagram>, ServeError> {
        loop {
            {
                let state = self.state.lock();
                if self.epoch < state.epoch {
                    self.epoch = state.epoch;
                    return Ok(state.latest.clone());
                }

                state.closed.clone()?;
                match state.modified() {
                    Some(notify) => notify,
                    None => return Ok(None), // No more updates will come
                }
            }
            .await;
        }
    }

    // Returns the largest group/sequence
    pub fn latest(&self) -> Option<(u64, u64)> {
        let state = self.state.lock();
        state.largest
    }

    /// Check if the datagrams writer has been closed or dropped.
    pub fn is_closed(&self) -> bool {
        let state = self.state.lock();
        state.closed.is_err() || state.modified().is_none()
    }
}

/// Static information about the datagram.
#[derive(Clone)]
pub struct Datagram {
    pub group_id: u64,
    pub object_id: u64,
    pub priority: u8,
    pub status: data::ObjectStatus,
    pub end_of_group: bool,
    pub payload: bytes::Bytes,

    // Extension headers (for draft-14 compliance, particularly immutable extensions)
    pub extension_headers: crate::data::ExtensionHeaders,
}

impl Datagram {
    pub(crate) fn from_data(datagram: data::Datagram, default_priority: u8) -> Self {
        Self {
            group_id: datagram.group_id,
            object_id: datagram.object_id.unwrap_or(0),
            priority: datagram.publisher_priority.unwrap_or(default_priority),
            status: datagram.status.unwrap_or(data::ObjectStatus::NormalObject),
            end_of_group: datagram.datagram_type.end_of_group(),
            payload: datagram.payload.unwrap_or_default(),
            extension_headers: datagram.extension_headers.unwrap_or_default(),
        }
    }
}

impl fmt::Debug for Datagram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Datagram")
            .field("object_id", &self.object_id)
            .field("group_id", &self.group_id)
            .field("priority", &self.priority)
            .field("status", &self.status)
            .field("end_of_group", &self.end_of_group)
            .field("payload", &self.payload.len())
            .field("extension_headers", &self.extension_headers)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datagram(group_id: u64, object_id: u64) -> Datagram {
        Datagram {
            group_id,
            object_id,
            priority: 128,
            status: data::ObjectStatus::NormalObject,
            end_of_group: false,
            payload: bytes::Bytes::from_static(b"payload"),
            extension_headers: data::ExtensionHeaders::default(),
        }
    }

    #[tokio::test]
    async fn latest_delivery_and_largest_location_are_independent() {
        let track = Arc::new(Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "datagram",
        ));
        let (mut writer, mut reader) = Datagrams { track }.produce();
        writer.write(datagram(2, 0)).unwrap();
        writer.write(datagram(1, 9)).unwrap();

        let delivered = reader.read().await.unwrap().unwrap();
        assert_eq!((delivered.group_id, delivered.object_id), (1, 9));
        assert_eq!(reader.latest(), Some((2, 0)));
    }

    #[tokio::test]
    async fn terminal_status_markers_do_not_advance_largest_location() {
        let track = Arc::new(Track::new(
            crate::coding::TrackNamespace::from_utf8_path("test"),
            "datagram",
        ));
        let (mut writer, reader) = Datagrams { track }.produce();
        writer.write(datagram(2, 0)).unwrap();

        let mut end_of_group = datagram(3, 0);
        end_of_group.status = data::ObjectStatus::EndOfGroup;
        end_of_group.payload = bytes::Bytes::new();
        writer.write(end_of_group).unwrap();
        assert_eq!(reader.latest(), Some((2, 0)));

        let mut zero_length = datagram(2, 5);
        zero_length.payload = bytes::Bytes::new();
        writer.write(zero_length).unwrap();
        assert_eq!(reader.latest(), Some((2, 5)));

        let mut end_of_track = datagram(4, 0);
        end_of_track.status = data::ObjectStatus::EndOfTrack;
        end_of_track.payload = bytes::Bytes::new();
        writer.write(end_of_track).unwrap();
        assert_eq!(reader.latest(), Some((2, 5)));
    }
}
