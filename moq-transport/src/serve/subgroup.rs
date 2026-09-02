// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A stream is a stream of objects with a header, split into a [Writer] and [Reader] handle.
//!
//! A [Writer] writes an ordered stream of objects.
//! Each object can have a sequence number, allowing the reader to detect gaps objects.
//!
//! A [Reader] reads an ordered stream of objects.
//! The reader can be cloned, in which case each reader receives a copy of each object. (fanout)
//!
//! The stream is closed with [ServeError::Closed] when all writers or readers are dropped.
use std::{
    collections::VecDeque,
    ops::Deref,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Weak,
    },
};

use bytes::Bytes;

use crate::data::ObjectStatus;
use crate::watch::State;

use super::{ServeError, Track};

pub struct Subgroups {
    pub track: Arc<Track>,
}

impl Subgroups {
    pub fn produce(self) -> (SubgroupsWriter, SubgroupsReader) {
        let (writer, reader) = State::default().split();

        let writer = SubgroupsWriter::new(writer, self.track.clone());
        let reader = SubgroupsReader::new(reader, self.track);

        (writer, reader)
    }
}

impl Deref for Subgroups {
    type Target = Track;

    fn deref(&self) -> &Self::Target {
        &self.track
    }
}

// State shared between the writer and reader.
struct SubgroupsState {
    latest_subgroup_reader: Option<SubgroupReader>,
    pending_subgroups: VecDeque<(usize, SubgroupReader)>,
    reader_cursors: Vec<Weak<AtomicUsize>>,
    active_subgroups: Vec<((u64, u64), Weak<SubgroupInfo>)>,
    next_sequence: usize,
    closed: Result<(), ServeError>,
}

const MAX_PENDING_SUBGROUPS: usize = 1024;

impl Default for SubgroupsState {
    fn default() -> Self {
        Self {
            latest_subgroup_reader: None,
            pending_subgroups: VecDeque::new(),
            reader_cursors: Vec::new(),
            active_subgroups: Vec::new(),
            next_sequence: 0,
            closed: Ok(()),
        }
    }
}

impl SubgroupsState {
    fn prune_consumed(&mut self) {
        self.reader_cursors
            .retain(|reader| reader.strong_count() > 0);
        let oldest_unread = self
            .reader_cursors
            .iter()
            .filter_map(Weak::upgrade)
            .map(|cursor| cursor.load(Ordering::Relaxed))
            .min()
            .unwrap_or(self.next_sequence);

        // Preserve the current latest subgroup for a reader activated after an
        // idle period, even when every active reader has advanced past it.
        while self.pending_subgroups.len() > 1
            && self
                .pending_subgroups
                .front()
                .is_some_and(|(sequence, _)| *sequence < oldest_unread)
        {
            self.pending_subgroups.pop_front();
        }
    }
}

pub struct SubgroupsWriter {
    pub info: Arc<Track>,
    state: State<SubgroupsState>,
    next_subgroup_id: u64, // Not in the state to avoid a lock
    next_group_id: u64,    // Not in the state to avoid a lock
    last_group_id: u64,    // Not in the state to avoid a lock
}

impl SubgroupsWriter {
    fn new(state: State<SubgroupsState>, track: Arc<Track>) -> Self {
        Self {
            info: track,
            state,
            next_subgroup_id: 0,
            next_group_id: 0,
            last_group_id: 0,
        }
    }

    // Helper to increment the group by one.
    pub fn append(&mut self, priority: u8) -> Result<SubgroupWriter, ServeError> {
        let group_id;
        let subgroup_id;

        // TODO: refactor here... For now, every subgroup is mapped to a new group...
        let start_new_group = true;

        if start_new_group {
            group_id = self.next_group_id;
            subgroup_id = 0;
        } else {
            group_id = self.last_group_id;
            subgroup_id = self.next_subgroup_id;
        }

        self.create(Subgroup {
            group_id,
            subgroup_id,
            priority,
        })
    }

    /// Create a new subgroup with the given parameters, inserting it into the track.
    pub fn create(&mut self, subgroup: Subgroup) -> Result<SubgroupWriter, ServeError> {
        self.create_with_metadata(subgroup, SubgroupStreamMetadata::default())
    }

    pub(crate) fn create_with_metadata(
        &mut self,
        subgroup: Subgroup,
        metadata: SubgroupStreamMetadata,
    ) -> Result<SubgroupWriter, ServeError> {
        let subgroup = SubgroupInfo {
            track: self.info.clone(),
            group_id: subgroup.group_id,
            subgroup_id: subgroup.subgroup_id,
            priority: subgroup.priority,
        };
        let (writer, reader) = subgroup.produce_with_metadata(metadata);

        let mut state = self.state.lock_mut().ok_or(ServeError::Cancel)?;
        state.closed.clone()?;

        state
            .active_subgroups
            .retain(|(_, subgroup)| subgroup.strong_count() > 0);
        let key = (writer.group_id, writer.subgroup_id);
        if state
            .active_subgroups
            .iter()
            .any(|(active, _)| *active == key)
        {
            return Err(ServeError::Duplicate);
        }

        state.prune_consumed();
        if state.pending_subgroups.len() >= MAX_PENDING_SUBGROUPS {
            let err =
                ServeError::Internal("subgroup reader exceeded its pending stream limit".into());
            state.closed = Err(err.clone());
            return Err(err);
        }

        let sequence = state.next_sequence;
        let Some(next_sequence) = state.next_sequence.checked_add(1) else {
            let err = ServeError::Internal("subgroup delivery sequence exhausted".into());
            state.closed = Err(err.clone());
            return Err(err);
        };
        state.next_sequence = next_sequence;
        state
            .pending_subgroups
            .push_back((sequence, reader.clone()));
        state
            .active_subgroups
            .push((key, Arc::downgrade(&reader.info)));

        let advances_latest = state
            .latest_subgroup_reader
            .as_ref()
            .is_none_or(|latest| key > (latest.group_id, latest.subgroup_id));
        if advances_latest {
            self.next_subgroup_id = writer.subgroup_id.saturating_add(1);
            self.next_group_id = writer.group_id.saturating_add(1);
            self.last_group_id = writer.group_id;
            state.latest_subgroup_reader = Some(reader);
        }

        Ok(writer)
    }

    /// Close the segment with an error.
    pub fn close(self, err: ServeError) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        let mut state = state.into_mut().ok_or(ServeError::Cancel)?;
        state.closed = Err(err);

        Ok(())
    }
}

impl Deref for SubgroupsWriter {
    type Target = Track;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

pub struct SubgroupsReader {
    pub info: Arc<Track>,
    state: State<SubgroupsState>,
    cursor: Option<Arc<AtomicUsize>>,
}

impl SubgroupsReader {
    fn new(state: State<SubgroupsState>, track_info: Arc<Track>) -> Self {
        Self {
            info: track_info,
            state,
            cursor: None,
        }
    }

    fn register(&mut self) {
        if self.cursor.is_some() {
            return;
        }

        let state = self.state.lock();
        let sequence = state
            .pending_subgroups
            .back()
            .map_or(state.next_sequence, |(sequence, _)| *sequence);
        let cursor = Arc::new(AtomicUsize::new(sequence));
        let mut state = state.into_mut_closed();
        state
            .reader_cursors
            .retain(|reader| reader.strong_count() > 0);
        state.reader_cursors.push(Arc::downgrade(&cursor));
        self.cursor = Some(cursor);
    }

    pub async fn next(&mut self) -> Result<Option<SubgroupReader>, ServeError> {
        self.register();

        loop {
            {
                let state = self.state.lock();
                if let (Some(cursor), Some((first_sequence, _))) =
                    (&self.cursor, state.pending_subgroups.front())
                {
                    let sequence = cursor.load(Ordering::Relaxed);
                    let index = sequence.checked_sub(*first_sequence).ok_or_else(|| {
                        ServeError::Internal("subgroup reader fell behind retained history".into())
                    })?;
                    if let Some(subgroup) = state
                        .pending_subgroups
                        .get(index)
                        .map(|(_, subgroup)| subgroup.clone())
                    {
                        cursor.store(sequence + 1, Ordering::Relaxed);
                        let mut state = state.into_mut_closed();
                        state.prune_consumed();
                        return Ok(Some(subgroup));
                    }
                }

                state.closed.clone()?;
                match state.modified() {
                    Some(notify) => notify,
                    None => return Ok(None),
                }
            }
            .await; // Try again when the state changes
        }
    }

    #[cfg(test)]
    pub(crate) fn delivery_cursor(&mut self) -> Arc<AtomicUsize> {
        self.register();
        self.cursor.as_ref().cloned().unwrap()
    }

    // Returns the largest group/sequence
    pub fn latest(&self) -> Option<(u64, u64)> {
        let state = self.state.lock();
        state
            .latest_subgroup_reader
            .as_ref()
            .and_then(|group| group.latest().map(|object_id| (group.group_id, object_id)))
    }

    /// Check if the subgroups writer has been closed or dropped.
    pub fn is_closed(&self) -> bool {
        let state = self.state.lock();
        state.closed.is_err() || state.modified().is_none()
    }
}

impl Clone for SubgroupsReader {
    fn clone(&self) -> Self {
        let state = self.state.lock();
        let sequence = self.cursor.as_ref().map_or_else(
            || {
                state
                    .pending_subgroups
                    .back()
                    .map_or(state.next_sequence, |(sequence, _)| *sequence)
            },
            |cursor| cursor.load(Ordering::Relaxed),
        );
        let cursor = Arc::new(AtomicUsize::new(sequence));
        let mut state = state.into_mut_closed();
        state
            .reader_cursors
            .retain(|reader| reader.strong_count() > 0);
        state.reader_cursors.push(Arc::downgrade(&cursor));

        Self {
            info: self.info.clone(),
            state: self.state.clone(),
            cursor: Some(cursor),
        }
    }
}

impl Drop for SubgroupsReader {
    fn drop(&mut self) {
        self.cursor = None;
        let mut state = self.state.lock().into_mut_closed();
        state.prune_consumed();
    }
}

impl Deref for SubgroupsReader {
    type Target = Track;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

/// Parameters that can be specified by the user
#[derive(Debug, Clone, PartialEq)]
pub struct Subgroup {
    // The sequence number of the group within the track.
    // NOTE: These may be received out of order or with gaps.
    pub group_id: u64,

    // The sequence number of the subgroup within the group.
    // NOTE: These may be received out of order or with gaps.
    pub subgroup_id: u64,

    // The priority of the group within the track.
    pub priority: u8,
}

/// Static information about the group
#[derive(Debug, Clone, PartialEq)]
pub struct SubgroupInfo {
    pub track: Arc<Track>,

    // The sequence number of the group within the track.
    // NOTE: These may be received out of order or with gaps.
    pub group_id: u64,

    // The sequence number of the subgroup within the group.
    // NOTE: These may be received out of order or with gaps.
    pub subgroup_id: u64,

    // The priority of the group within the track.
    pub priority: u8,
}

impl SubgroupInfo {
    pub fn produce(self) -> (SubgroupWriter, SubgroupReader) {
        self.produce_with_metadata(SubgroupStreamMetadata::default())
    }

    pub(crate) fn produce_with_metadata(
        self,
        metadata: SubgroupStreamMetadata,
    ) -> (SubgroupWriter, SubgroupReader) {
        let (writer, reader) = State::new(SubgroupState::new(metadata)).split();
        let info = Arc::new(self);

        let writer = SubgroupWriter::new(writer, info.clone());
        let reader = SubgroupReader::new(reader, info);

        (writer, reader)
    }
}

impl Deref for SubgroupInfo {
    type Target = Track;

    fn deref(&self) -> &Self::Target {
        &self.track
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct SubgroupStreamMetadata {
    pub has_properties: bool,
    pub end_of_group: bool,
    pub first_object: bool,
}

impl Default for SubgroupStreamMetadata {
    fn default() -> Self {
        Self {
            // Existing application-created subgroups have always used the
            // properties-capable wire grammar.
            has_properties: true,
            end_of_group: false,
            // A locally created stream begins with the first Object published
            // in its subgroup. Relay receive paths supply explicit metadata.
            first_object: true,
        }
    }
}

struct SubgroupState {
    // The data that has been received thus far.
    objects: Vec<SubgroupObjectReader>,

    metadata: SubgroupStreamMetadata,

    // Set when the writer or all readers are dropped.
    closed: Result<(), ServeError>,
}

impl SubgroupState {
    fn new(metadata: SubgroupStreamMetadata) -> Self {
        Self {
            objects: Vec::new(),
            metadata,
            closed: Ok(()),
        }
    }
}

/// Used to write data to a stream and notify readers.
pub struct SubgroupWriter {
    // Mutable stream state.
    state: State<SubgroupState>,

    // Immutable stream state.
    pub info: Arc<SubgroupInfo>,

    // The next object sequence number to use.
    next_object_id: Option<u64>,
}

impl SubgroupWriter {
    fn new(state: State<SubgroupState>, group: Arc<SubgroupInfo>) -> Self {
        Self {
            state,
            info: group,
            next_object_id: Some(0),
        }
    }

    /// Create the next object ID with the given payload.
    pub fn write(&mut self, payload: bytes::Bytes) -> Result<(), ServeError> {
        let mut object = self.create(payload.len(), None)?;
        object.write(payload)?;
        Ok(())
    }

    /// Write an object over multiple writes.
    ///
    /// BAD STUFF will happen if the size is wrong; this is an advanced feature.
    pub fn create(
        &mut self,
        size: usize,
        extension_headers: Option<crate::data::ExtensionHeaders>,
    ) -> Result<SubgroupObjectWriter, ServeError> {
        let object_id = self.next_object_id.ok_or(ServeError::Duplicate)?;
        self.create_with_id(object_id, size, extension_headers)
    }

    /// Write an object with an explicit absolute Object ID.
    pub fn create_with_id(
        &mut self,
        object_id: u64,
        size: usize,
        extension_headers: Option<crate::data::ExtensionHeaders>,
    ) -> Result<SubgroupObjectWriter, ServeError> {
        let next_object_id = self.next_object_id.ok_or(ServeError::Duplicate)?;
        if object_id < next_object_id {
            return Err(ServeError::Duplicate);
        }

        let (writer, reader) = SubgroupObject {
            group: self.info.clone(),
            object_id,
            status: ObjectStatus::NormalObject,
            size,
            extension_headers: extension_headers.unwrap_or_default(),
        }
        .produce();

        let mut state = self.state.lock_mut().ok_or(ServeError::Cancel)?;
        state.objects.push(reader);
        self.next_object_id = object_id.checked_add(1);

        Ok(writer)
    }

    /// Close the stream with an error.
    pub fn close(self, err: ServeError) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        let mut state = state.into_mut().ok_or(ServeError::Cancel)?;
        state.closed = Err(err);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.state.lock().objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Deref for SubgroupWriter {
    type Target = SubgroupInfo;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

/// Notified when a stream has new data available.
#[derive(Clone)]
pub struct SubgroupReader {
    // Modify the stream state.
    state: State<SubgroupState>,

    // Immutable stream state.
    pub info: Arc<SubgroupInfo>,

    // The number of chunks that we've read.
    // NOTE: Cloned readers inherit this index, but then run in parallel.
    read_index: usize,
}

impl SubgroupReader {
    fn new(state: State<SubgroupState>, subgroup: Arc<SubgroupInfo>) -> Self {
        Self {
            state,
            info: subgroup,
            read_index: 0,
        }
    }

    pub fn latest(&self) -> Option<u64> {
        let state = self.state.lock();
        state.objects.last().map(|o| o.object_id)
    }

    pub(crate) fn metadata(&self) -> SubgroupStreamMetadata {
        self.state.lock().metadata
    }

    pub async fn read_next(&mut self) -> Result<Option<Bytes>, ServeError> {
        let object = self.next().await?;
        match object {
            Some(mut object) => Ok(Some(object.read_all().await?)),
            None => Ok(None),
        }
    }

    pub async fn next(&mut self) -> Result<Option<SubgroupObjectReader>, ServeError> {
        loop {
            {
                let state = self.state.lock();

                if self.read_index < state.objects.len() {
                    let object = state.objects[self.read_index].clone();
                    self.read_index += 1;
                    return Ok(Some(object));
                }

                state.closed.clone()?;
                match state.modified() {
                    Some(notify) => notify,
                    None => return Ok(None),
                }
            }
            .await; // Try again when the state changes
        }
    }

    pub fn pos(&self) -> usize {
        self.read_index
    }

    pub fn len(&self) -> usize {
        self.state.lock().objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Deref for SubgroupReader {
    type Target = SubgroupInfo;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

/// A subset of Object, since we use the group's info.
#[derive(Clone, PartialEq, Debug)]
pub struct SubgroupObject {
    pub group: Arc<SubgroupInfo>,

    pub object_id: u64,

    // The size of the object.
    pub size: usize,

    // Object status
    pub status: ObjectStatus,

    // Extension headers (for draft-14 compliance, particularly immutable extensions)
    pub extension_headers: crate::data::ExtensionHeaders,
}

impl SubgroupObject {
    pub fn produce(self) -> (SubgroupObjectWriter, SubgroupObjectReader) {
        let (writer, reader) = State::default().split();
        let info = Arc::new(self);

        let writer = SubgroupObjectWriter::new(writer, info.clone());
        let reader = SubgroupObjectReader::new(reader, info);

        (writer, reader)
    }
}

impl Deref for SubgroupObject {
    type Target = SubgroupInfo;

    fn deref(&self) -> &Self::Target {
        &self.group
    }
}

struct SubgroupObjectState {
    // The data that has been received thus far.
    chunks: Vec<Bytes>,

    // Set when the writer is dropped.
    closed: Result<(), ServeError>,
}

impl Default for SubgroupObjectState {
    fn default() -> Self {
        Self {
            chunks: Vec::new(),
            closed: Ok(()),
        }
    }
}

/// Used to write data to a segment and notify readers.
pub struct SubgroupObjectWriter {
    // Mutable segment state.
    state: State<SubgroupObjectState>,

    // Immutable segment state.
    pub info: Arc<SubgroupObject>,

    // The amount of promised data that has yet to be written.
    remain: usize,
}

impl SubgroupObjectWriter {
    /// Create a new segment with the given info.
    fn new(state: State<SubgroupObjectState>, object: Arc<SubgroupObject>) -> Self {
        Self {
            state,
            remain: object.size,
            info: object,
        }
    }

    /// Write a new chunk of bytes.
    pub fn write(&mut self, chunk: Bytes) -> Result<(), ServeError> {
        if chunk.len() > self.remain {
            return Err(ServeError::Size);
        }
        self.remain -= chunk.len();

        let mut state = self.state.lock_mut().ok_or(ServeError::Cancel)?;
        state.chunks.push(chunk);

        Ok(())
    }

    /// Close the segment with an error.
    pub fn close(self, err: ServeError) -> Result<(), ServeError> {
        if self.remain != 0 {
            return Err(ServeError::Size);
        }

        let state = self.state.lock();
        state.closed.clone()?;

        let mut state = state.into_mut().ok_or(ServeError::Cancel)?;
        state.closed = Err(err);

        Ok(())
    }
}

impl Drop for SubgroupObjectWriter {
    fn drop(&mut self) {
        if self.remain == 0 {
            return;
        }

        if let Some(mut state) = self.state.lock_mut() {
            state.closed = Err(ServeError::Size);
        }
    }
}

impl Deref for SubgroupObjectWriter {
    type Target = SubgroupObject;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

/// Notified when a segment has new data available.
#[derive(Clone)]
pub struct SubgroupObjectReader {
    // Modify the segment state.
    state: State<SubgroupObjectState>,

    // Immutable segment state.
    pub info: Arc<SubgroupObject>,

    // The number of chunks that we've read.
    // NOTE: Cloned readers inherit this index, but then run in parallel.
    index: usize,
}

impl SubgroupObjectReader {
    fn new(state: State<SubgroupObjectState>, object: Arc<SubgroupObject>) -> Self {
        Self {
            state,
            info: object,
            index: 0,
        }
    }

    /// Block until the next chunk of bytes is available.
    pub async fn read(&mut self) -> Result<Option<Bytes>, ServeError> {
        loop {
            {
                let state = self.state.lock();

                if self.index < state.chunks.len() {
                    let chunk = state.chunks[self.index].clone();
                    self.index += 1;
                    return Ok(Some(chunk));
                }

                state.closed.clone()?;
                match state.modified() {
                    Some(notify) => notify,
                    None => return Ok(None), // No more changes will come
                }
            }
            .await; // Try again when the state changes
        }
    }

    pub async fn read_all(&mut self) -> Result<Bytes, ServeError> {
        let mut chunks = Vec::new();
        while let Some(chunk) = self.read().await? {
            chunks.push(chunk);
        }

        Ok(Bytes::from(chunks.concat()))
    }
}

impl Deref for SubgroupObjectReader {
    type Target = SubgroupObject;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding::TrackNamespace;

    fn subgroup() -> (SubgroupWriter, SubgroupReader) {
        SubgroupInfo {
            track: Arc::new(Track::new(TrackNamespace::from_utf8_path("test"), "track")),
            group_id: 0,
            subgroup_id: 0,
            priority: 128,
        }
        .produce()
    }

    fn subgroups() -> (SubgroupsWriter, SubgroupsReader) {
        Subgroups {
            track: Arc::new(Track::new(TrackNamespace::from_utf8_path("test"), "track")),
        }
        .produce()
    }

    fn create_subgroup(
        writer: &mut SubgroupsWriter,
        group_id: u64,
        subgroup_id: u64,
    ) -> SubgroupWriter {
        writer
            .create(Subgroup {
                group_id,
                subgroup_id,
                priority: 128,
            })
            .unwrap()
    }

    async fn assert_subgroups(reader: &mut SubgroupsReader, expected: &[(u64, u64)]) {
        for &(group_id, subgroup_id) in expected {
            let subgroup = reader.next().await.unwrap().unwrap();
            assert_eq!(
                (subgroup.group_id, subgroup.subgroup_id),
                (group_id, subgroup_id)
            );
        }
    }

    #[test]
    fn locally_created_subgroup_begins_with_first_object() {
        let (_writer, reader) = subgroup();
        assert!(reader.metadata().first_object);
    }

    #[tokio::test]
    async fn delivers_reverse_order_subgroups() {
        let (mut writer, template) = subgroups();
        let mut reader = template.clone();

        let _one = create_subgroup(&mut writer, 0, 1);
        let _zero = create_subgroup(&mut writer, 0, 0);

        assert_subgroups(&mut reader, &[(0, 1), (0, 0)]).await;
    }

    #[tokio::test]
    async fn delivers_burst_without_coalescing() {
        let (mut writer, template) = subgroups();
        let mut reader = template.clone();

        let _zero = create_subgroup(&mut writer, 0, 0);
        let _one = create_subgroup(&mut writer, 0, 1);

        assert_subgroups(&mut reader, &[(0, 0), (0, 1)]).await;
    }

    #[tokio::test]
    async fn delivers_two_groups_in_arrival_order() {
        let (mut writer, template) = subgroups();
        let mut reader = template.clone();
        let mut subgroups = Vec::new();
        let expected = [(0, 1), (0, 0), (1, 1), (1, 0)];

        for &(group_id, subgroup_id) in &expected {
            subgroups.push(create_subgroup(&mut writer, group_id, subgroup_id));
        }

        assert_subgroups(&mut reader, &expected).await;
    }

    #[tokio::test]
    async fn cloned_reader_inherits_pending_and_future_subgroups() {
        let (mut writer, template) = subgroups();
        let mut first = template.clone();
        let _zero = create_subgroup(&mut writer, 0, 0);
        let mut second = first.clone();
        let _one = create_subgroup(&mut writer, 0, 1);

        assert_subgroups(&mut first, &[(0, 0), (0, 1)]).await;
        assert_subgroups(&mut second, &[(0, 0), (0, 1)]).await;
    }

    #[tokio::test]
    async fn cloned_readers_receive_subgroups_independently() {
        let (mut writer, template) = subgroups();
        let mut first = template.clone();
        let mut second = template.clone();
        let _zero = create_subgroup(&mut writer, 0, 0);
        let _one = create_subgroup(&mut writer, 0, 1);

        assert_subgroups(&mut second, &[(0, 0), (0, 1)]).await;
        assert_subgroups(&mut first, &[(0, 0), (0, 1)]).await;
    }

    #[test]
    fn rejects_active_duplicate_after_out_of_order_delivery() {
        let (mut writer, template) = subgroups();
        let _reader = template.clone();
        let _one = create_subgroup(&mut writer, 0, 1);
        let _zero = create_subgroup(&mut writer, 0, 0);

        assert!(matches!(
            writer.create(Subgroup {
                group_id: 0,
                subgroup_id: 0,
                priority: 128,
            }),
            Err(ServeError::Duplicate)
        ));
    }

    #[tokio::test]
    async fn backlog_overflow_closes_the_subgroup_source() {
        let (mut writer, template) = subgroups();
        let mut reader = template.clone();
        let mut open = Vec::new();
        for group_id in 0..MAX_PENDING_SUBGROUPS as u64 {
            open.push(create_subgroup(&mut writer, group_id, 0));
        }

        assert!(matches!(
            writer.create(Subgroup {
                group_id: MAX_PENDING_SUBGROUPS as u64,
                subgroup_id: 0,
                priority: 128,
            }),
            Err(ServeError::Internal(_))
        ));

        for group_id in 0..MAX_PENDING_SUBGROUPS as u64 {
            assert_subgroups(&mut reader, &[(group_id, 0)]).await;
        }
        assert!(matches!(reader.next().await, Err(ServeError::Internal(_))));
        assert!(matches!(
            writer.create(Subgroup {
                group_id: MAX_PENDING_SUBGROUPS as u64,
                subgroup_id: 0,
                priority: 128,
            }),
            Err(ServeError::Internal(_))
        ));
    }

    #[tokio::test]
    async fn consumed_history_does_not_exhaust_the_bound() {
        let (mut writer, template) = subgroups();
        let mut reader = template.clone();

        for group_id in 0..(MAX_PENDING_SUBGROUPS as u64 * 2) {
            let _subgroup = create_subgroup(&mut writer, group_id, 0);
            assert_subgroups(&mut reader, &[(group_id, 0)]).await;
        }

        assert_eq!(writer.state.lock().pending_subgroups.len(), 1);
    }

    #[tokio::test]
    async fn terminal_history_is_pruned_as_the_reader_advances() {
        let (mut writer, template) = subgroups();
        let mut reader = template.clone();

        for group_id in 0..8 {
            let _subgroup = create_subgroup(&mut writer, group_id, 0);
        }
        drop(writer);

        for group_id in 0..8 {
            assert_subgroups(&mut reader, &[(group_id, 0)]).await;
        }
        assert_eq!(reader.state.lock().pending_subgroups.len(), 1);
        assert!(reader.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn dropping_the_slowest_reader_prunes_terminal_history() {
        let (mut writer, template) = subgroups();
        let mut fast = template.clone();
        let slow = fast.clone();

        for group_id in 0..8 {
            let _subgroup = create_subgroup(&mut writer, group_id, 0);
        }
        drop(writer);
        for group_id in 0..8 {
            assert_subgroups(&mut fast, &[(group_id, 0)]).await;
        }
        assert_eq!(fast.state.lock().pending_subgroups.len(), 8);

        drop(slow);
        assert_eq!(fast.state.lock().pending_subgroups.len(), 1);
    }

    #[tokio::test]
    async fn delivery_sequence_exhaustion_closes_the_source() {
        let (mut writer, template) = subgroups();
        let mut reader = template.clone();
        writer.state.lock_mut().unwrap().next_sequence = usize::MAX;

        assert!(matches!(
            writer.create(Subgroup {
                group_id: 0,
                subgroup_id: 0,
                priority: 128,
            }),
            Err(ServeError::Internal(_))
        ));
        assert!(matches!(reader.next().await, Err(ServeError::Internal(_))));
    }

    #[tokio::test]
    async fn dropping_a_slow_reader_releases_backlog_capacity() {
        let (mut writer, template) = subgroups();
        let mut fast = template.clone();
        let slow = fast.clone();

        for group_id in 0..(MAX_PENDING_SUBGROUPS as u64 - 1) {
            let _subgroup = create_subgroup(&mut writer, group_id, 0);
            assert_subgroups(&mut fast, &[(group_id, 0)]).await;
        }

        drop(slow);
        for group_id in (MAX_PENDING_SUBGROUPS as u64 - 1)..=(MAX_PENDING_SUBGROUPS as u64) {
            let _subgroup = create_subgroup(&mut writer, group_id, 0);
            assert_subgroups(&mut fast, &[(group_id, 0)]).await;
        }
    }

    #[tokio::test]
    async fn create_with_id_preserves_nonzero_and_gapped_object_ids() {
        let (mut writer, mut reader) = subgroup();

        for object_id in [7, 9] {
            let mut object = writer.create_with_id(object_id, 1, None).unwrap();
            object.write(Bytes::from_static(b"x")).unwrap();
        }

        for expected in [7, 9] {
            let object = reader.next().await.unwrap().unwrap();
            assert_eq!(object.object_id, expected);
        }
    }

    #[tokio::test]
    async fn create_with_id_accepts_u64_max_as_the_final_object() {
        let (mut writer, mut reader) = subgroup();

        drop(writer.create_with_id(u64::MAX, 0, None).unwrap());

        assert_eq!(reader.next().await.unwrap().unwrap().object_id, u64::MAX);
        assert!(writer.create(0, None).is_err());
        assert!(writer.create_with_id(u64::MAX, 0, None).is_err());
    }

    #[test]
    fn create_with_id_rejects_duplicate_and_decreasing_ids() {
        let (mut writer, _reader) = subgroup();

        drop(writer.create_with_id(7, 0, None).unwrap());

        assert!(matches!(
            writer.create_with_id(7, 0, None),
            Err(ServeError::Duplicate)
        ));
        assert!(matches!(
            writer.create_with_id(6, 0, None),
            Err(ServeError::Duplicate)
        ));
    }
}
