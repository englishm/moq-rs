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
use std::{cmp, ops::Deref, sync::Arc};

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
    epoch: u64, // Updated each time latest changes
    closed: Result<(), ServeError>,
}

impl Default for SubgroupsState {
    fn default() -> Self {
        Self {
            latest_subgroup_reader: None,
            epoch: 0,
            closed: Ok(()),
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
        let subgroup = SubgroupInfo {
            track: self.info.clone(),
            group_id: subgroup.group_id,
            subgroup_id: subgroup.subgroup_id,
            priority: subgroup.priority,
        };
        let (writer, reader) = subgroup.produce();

        let mut state = self.state.lock_mut().ok_or(ServeError::Cancel)?;

        if let Some(latest) = &state.latest_subgroup_reader {
            // TODO: Check this logic again
            if writer.group_id.cmp(&latest.group_id) == cmp::Ordering::Equal {
                match writer.subgroup_id.cmp(&latest.subgroup_id) {
                    cmp::Ordering::Less => return Ok(writer), // dropped immediately, lul
                    cmp::Ordering::Equal => return Err(ServeError::Duplicate),
                    cmp::Ordering::Greater => state.latest_subgroup_reader = Some(reader),
                }
            } else if writer.group_id.cmp(&latest.group_id) == cmp::Ordering::Greater {
                state.latest_subgroup_reader = Some(reader);
            } else {
                return Ok(writer); // drop here as well
            }
        } else {
            state.latest_subgroup_reader = Some(reader);
        }

        self.next_subgroup_id = state.latest_subgroup_reader.as_ref().unwrap().subgroup_id + 1;
        self.next_group_id = state.latest_subgroup_reader.as_ref().unwrap().group_id + 1;
        self.last_group_id = state.latest_subgroup_reader.as_ref().unwrap().group_id;
        state.epoch += 1;

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

#[derive(Clone)]
pub struct SubgroupsReader {
    pub info: Arc<Track>,
    state: State<SubgroupsState>,
    epoch: u64,
}

impl SubgroupsReader {
    fn new(state: State<SubgroupsState>, track_info: Arc<Track>) -> Self {
        Self {
            info: track_info,
            state,
            epoch: 0,
        }
    }

    pub async fn next(&mut self) -> Result<Option<SubgroupReader>, ServeError> {
        loop {
            {
                let state = self.state.lock();

                if self.epoch != state.epoch {
                    self.epoch = state.epoch;
                    return Ok(state.latest_subgroup_reader.clone());
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
        let (writer, reader) = State::default().split();
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

struct SubgroupState {
    // The data that has been received thus far.
    objects: Vec<SubgroupObjectReader>,

    // Set when the writer or all readers are dropped.
    closed: Result<(), ServeError>,
}

impl Default for SubgroupState {
    fn default() -> Self {
        Self {
            objects: Vec::new(),
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
    next_object_id: u64,
}

impl SubgroupWriter {
    fn new(state: State<SubgroupState>, group: Arc<SubgroupInfo>) -> Self {
        Self {
            state,
            info: group,
            next_object_id: 0,
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
        let (writer, reader) = SubgroupObject {
            group: self.info.clone(),
            object_id: self.next_object_id,
            status: ObjectStatus::NormalObject,
            size,
            extension_headers: extension_headers.unwrap_or_default(),
        }
        .produce();

        self.next_object_id += 1;

        let mut state = self.state.lock_mut().ok_or(ServeError::Cancel)?;
        state.objects.push(reader);

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

    /// Helper: create a SubgroupsWriter/Reader pair with a dummy track.
    fn make_subgroups() -> (SubgroupsWriter, SubgroupsReader) {
        let track = Arc::new(Track::new(
            TrackNamespace::from_utf8_path("test/ns"),
            "test-track".to_string(),
        ));
        Subgroups { track }.produce()
    }

    /// Helper: deterministic payload for (group_id, object_id).
    fn payload(group_id: u64, object_id: u64) -> Bytes {
        Bytes::from(format!("g{}-o{}", group_id, object_id))
    }

    // ---------------------------------------------------------------
    // U1: Single group, single object
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn single_group_single_object() {
        let (mut writer, mut reader) = make_subgroups();

        let mut sg = writer.append(0).unwrap();
        let group_id = sg.group_id;
        sg.write(payload(group_id, 0)).unwrap();
        drop(sg); // close the subgroup

        let sub = reader.next().await.unwrap().expect("expected a subgroup");
        assert_eq!(sub.group_id, group_id);

        let mut obj = sub.clone();
        let data = obj.read_next().await.unwrap().expect("expected an object");
        assert_eq!(data, payload(group_id, 0));

        // No more objects
        assert!(obj.read_next().await.unwrap().is_none());
    }

    // ---------------------------------------------------------------
    // U2: Single group, multiple objects
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn single_group_multiple_objects() {
        let (mut writer, mut reader) = make_subgroups();

        let mut sg = writer.append(0).unwrap();
        let gid = sg.group_id;
        for oid in 0..5u64 {
            sg.write(payload(gid, oid)).unwrap();
        }
        drop(sg);

        let mut sub = reader.next().await.unwrap().unwrap();
        for oid in 0..5u64 {
            let obj = sub.next().await.unwrap().expect("expected object");
            assert_eq!(obj.object_id, oid);
            let mut obj = obj;
            let data = obj.read_all().await.unwrap();
            assert_eq!(data, payload(gid, oid));
        }
        assert!(sub.next().await.unwrap().is_none());
    }

    // ---------------------------------------------------------------
    // U3: Multiple groups, multiple objects
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn multiple_groups_multiple_objects() {
        let (mut writer, mut reader) = make_subgroups();

        let num_groups = 3u64;
        let objects_per_group = 4u64;

        for _ in 0..num_groups {
            let mut sg = writer.append(0).unwrap();
            let gid = sg.group_id;
            for oid in 0..objects_per_group {
                let p = payload(gid, oid);
                sg.write(p).unwrap();
            }
            drop(sg);
        }
        drop(writer); // close the subgroups writer

        // Reader sees each group as the latest when epoch bumps.
        // Because latest-only, we collect what we can.
        let mut received: Vec<(u64, u64, Bytes)> = Vec::new();
        while let Ok(Some(mut sub)) = reader.next().await {
            let gid = sub.group_id;
            while let Ok(Some(mut obj)) = sub.next().await {
                let oid = obj.object_id;
                let data = obj.read_all().await.unwrap();
                received.push((gid, oid, data));
            }
        }

        // Every received tuple must match what was published.
        for (gid, oid, data) in &received {
            assert_eq!(data, &payload(*gid, *oid));
        }

        // At minimum the last group must be received with all its objects.
        let last_group_id = num_groups - 1;
        let last_group_objects: Vec<_> = received
            .iter()
            .filter(|(g, _, _)| *g == last_group_id)
            .collect();
        assert_eq!(
            last_group_objects.len(),
            objects_per_group as usize,
            "last group must be received with all {} objects",
            objects_per_group
        );
    }

    // ---------------------------------------------------------------
    // U4: Variable payload sizes
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn variable_payload_sizes() {
        let (mut writer, mut reader) = make_subgroups();

        let payloads: Vec<Bytes> = vec![
            Bytes::new(),                  // 0-byte
            Bytes::from_static(b"x"),      // 1-byte
            Bytes::from(vec![0xAB; 1024]), // 1 KB
            Bytes::from(vec![0xCD; 4096]), // 4 KB
        ];

        let mut sg = writer.append(0).unwrap();
        for p in &payloads {
            sg.write(p.clone()).unwrap();
        }
        drop(sg);

        let mut sub = reader.next().await.unwrap().unwrap();
        for expected in &payloads {
            let mut obj = sub.next().await.unwrap().unwrap();
            let data = obj.read_all().await.unwrap();
            assert_eq!(data, *expected);
        }
        assert!(sub.next().await.unwrap().is_none());
    }

    // ---------------------------------------------------------------
    // U5: Multi-chunk writes
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn multi_chunk_writes() {
        let (mut writer, mut reader) = make_subgroups();

        let chunk1 = Bytes::from_static(b"hello ");
        let chunk2 = Bytes::from_static(b"world");
        let full = Bytes::from_static(b"hello world");

        let mut sg = writer.append(0).unwrap();
        let mut obj_writer = sg.create(full.len(), None).unwrap();
        obj_writer.write(chunk1).unwrap();
        obj_writer.write(chunk2).unwrap();
        drop(obj_writer);
        drop(sg);

        let mut sub = reader.next().await.unwrap().unwrap();
        let mut obj = sub.next().await.unwrap().unwrap();
        let data = obj.read_all().await.unwrap();
        assert_eq!(data, full);
    }

    // ---------------------------------------------------------------
    // U6: Latest-only semantics
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn latest_only_semantics() {
        let (mut writer, reader) = make_subgroups();

        // Write group 0
        let mut sg0 = writer.append(0).unwrap();
        sg0.write(payload(sg0.group_id, 0)).unwrap();
        drop(sg0);

        // Write group 1
        let mut sg1 = writer.append(0).unwrap();
        let gid1 = sg1.group_id;
        sg1.write(payload(gid1, 0)).unwrap();
        drop(sg1);

        // A fresh clone of reader should only see group 1 (the latest).
        let mut fresh = reader.clone();
        let sub = fresh.next().await.unwrap().unwrap();
        assert_eq!(sub.group_id, gid1);
    }

    // ---------------------------------------------------------------
    // U7: Older group silently dropped
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn older_group_silently_dropped() {
        let (mut writer, mut reader) = make_subgroups();

        // Write group 5 via create()
        let sg5 = writer
            .create(Subgroup {
                group_id: 5,
                subgroup_id: 0,
                priority: 0,
            })
            .unwrap();
        drop(sg5);

        // Write group 3 (older) -- should not become latest
        let sg3 = writer
            .create(Subgroup {
                group_id: 3,
                subgroup_id: 0,
                priority: 0,
            })
            .unwrap();
        drop(sg3);

        // Reader should see group 5 as latest (epoch bumped once for group 5).
        let sub = reader.next().await.unwrap().unwrap();
        assert_eq!(sub.group_id, 5);
    }

    // ---------------------------------------------------------------
    // U8: Duplicate group_id + subgroup_id rejected
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn duplicate_rejected() {
        let (mut writer, _reader) = make_subgroups();

        let _sg = writer
            .create(Subgroup {
                group_id: 5,
                subgroup_id: 0,
                priority: 0,
            })
            .unwrap();

        let result = writer.create(Subgroup {
            group_id: 5,
            subgroup_id: 0,
            priority: 0,
        });

        match result {
            Err(ServeError::Duplicate) => {} // expected
            Err(e) => panic!("expected Duplicate, got {:?}", e),
            Ok(_) => panic!("expected Duplicate error, got Ok"),
        }
    }

    // ---------------------------------------------------------------
    // U9: Higher subgroup_id within same group
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn higher_subgroup_id_updates_latest() {
        let (mut writer, mut reader) = make_subgroups();

        let _sg0 = writer
            .create(Subgroup {
                group_id: 1,
                subgroup_id: 0,
                priority: 0,
            })
            .unwrap();

        let _sg1 = writer
            .create(Subgroup {
                group_id: 1,
                subgroup_id: 1,
                priority: 0,
            })
            .unwrap();

        // Reader should see subgroup_id=1 as latest (two epoch bumps).
        // Drain to the latest with a timeout guard to prevent indefinite hang.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut latest_sub = None;
            // Read available updates (non-blocking after writer is done).
            // We'll read what we can and the last one should be subgroup_id=1.
            loop {
                let sub = reader.next().await.unwrap();
                match sub {
                    Some(s) => latest_sub = Some(s),
                    None => break,
                }
                // Break after we've seen subgroup_id=1 to avoid blocking forever
                // since writer is still alive.
                if latest_sub.as_ref().map(|s| s.subgroup_id) == Some(1) {
                    break;
                }
            }
            latest_sub
        })
        .await;

        let latest_sub = result.expect("higher_subgroup_id_updates_latest timed out after 5 seconds");
        assert_eq!(latest_sub.unwrap().subgroup_id, 1);
    }

    // ---------------------------------------------------------------
    // U10: Writer close propagates
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn writer_close_propagates() {
        let (mut writer, mut reader) = make_subgroups();

        let mut sg = writer.append(0).unwrap();
        sg.write(Bytes::from_static(b"data")).unwrap();
        drop(sg);

        writer.close(ServeError::Done).unwrap();

        // Reader should get the subgroup, then Done error.
        let _sub = reader.next().await.unwrap().unwrap();
        let result = reader.next().await;
        match result {
            Err(ServeError::Done) => {} // expected
            Err(e) => panic!("expected Done, got {:?}", e),
            Ok(_) => panic!("expected Done error, got Ok"),
        }
    }

    // ---------------------------------------------------------------
    // U11: Object size mismatch
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn object_size_mismatch() {
        let (mut writer, mut reader) = make_subgroups();

        let mut sg = writer.append(0).unwrap();

        // Declare size=10, write only 5 bytes, then drop writer
        let mut obj_writer = sg.create(10, None).unwrap();
        obj_writer.write(Bytes::from(vec![0u8; 5])).unwrap();
        drop(obj_writer); // Drop triggers Size error because remain != 0

        drop(sg);

        let mut sub = reader.next().await.unwrap().unwrap();
        let mut obj = sub.next().await.unwrap().unwrap();

        // Read the 5 bytes that were written
        let chunk = obj.read().await.unwrap();
        assert!(chunk.is_some());

        // Next read should get the Size error
        let result = obj.read().await;
        assert_eq!(result.unwrap_err(), ServeError::Size);
    }

    // ---------------------------------------------------------------
    // U12: Concurrent readers (fan-out)
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn concurrent_readers_fanout() {
        let (mut writer, mut reader1) = make_subgroups();
        let mut reader2 = reader1.clone();

        let mut sg = writer.append(0).unwrap();
        let gid = sg.group_id;
        for oid in 0..3u64 {
            sg.write(payload(gid, oid)).unwrap();
        }
        drop(sg);
        drop(writer);

        // Both readers should see the same subgroup.
        let mut sub1 = reader1.next().await.unwrap().unwrap();
        let mut sub2 = reader2.next().await.unwrap().unwrap();

        assert_eq!(sub1.group_id, sub2.group_id);

        // Read all objects from both and compare.
        let mut data1 = Vec::new();
        while let Ok(Some(mut obj)) = sub1.next().await {
            data1.push((obj.object_id, obj.read_all().await.unwrap()));
        }

        let mut data2 = Vec::new();
        while let Ok(Some(mut obj)) = sub2.next().await {
            data2.push((obj.object_id, obj.read_all().await.unwrap()));
        }

        assert_eq!(data1, data2);
        assert_eq!(data1.len(), 3);
    }
}
