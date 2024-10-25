//! A stream is a stream of objects with a header, split into a [Writer] and [Reader] handle.
//!
//! A [Writer] writes an ordered stream of objects.
//! Each object can have a sequence number, allowing the reader to detect gaps objects.
//!
//! A [Reader] reads an ordered stream of objects.
//! The reader can be cloned, in which case each reader receives a copy of each object. (fanout)
//!
//! The stream is closed with [ServeError::Closed] when all writers or readers are dropped.
use bytes::Bytes;
use std::{cmp, ops::Deref, sync::Arc};

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
	latest: Option<SubgroupReader>,
	epoch: u64, // Updated each time latest changes
	closed: Result<(), ServeError>,
}

impl Default for SubgroupsState {
	fn default() -> Self {
		Self {
			latest: None,
			epoch: 0,
			closed: Ok(()),
		}
	}
}

pub struct SubgroupsWriter {
	pub info: Arc<Track>,
	state: State<SubgroupsState>,
	next: u64, // Not in the state to avoid a lock
}

impl SubgroupsWriter {
	fn new(state: State<SubgroupsState>, track: Arc<Track>) -> Self {
		Self {
			info: track,
			state,
			next: 0,
		}
	}

	// Helper to increment the group by one.
	pub fn append(&mut self, priority: u8) -> Result<SubgroupWriter, ServeError> {
		self.create(Subgroup {
			subgroup_id: self.next,
			priority,
		})
	}

	pub fn create(&mut self, subgroup: Subgroup) -> Result<SubgroupWriter, ServeError> {
		let subgroup = SubgroupInfo {
			track: self.info.clone(),
			group_id: subgroup.group_id,
			subgroup_id: subgroup.subgroup_id,
			priority: subgroup.priority,
		};
		let (writer, reader) = subgroup.produce();

		let mut state = self.state.lock_mut().ok_or(ServeError::Cancel)?;

		if let Some(latest) = &state.latest {
			match writer.group_id.cmp(&latest.group_id) {
				cmp::Ordering::Less => return Ok(writer), // dropped immediately, lul
				cmp::Ordering::Equal => return Err(ServeError::Duplicate),
				cmp::Ordering::Greater => state.latest = Some(reader),
			}
		} else {
			state.latest = Some(reader);
		}
		// TODO: group_id should be incremented somewhere
		self.next = state.latest.as_ref().unwrap().subgroup_id + 1;
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
	fn new(state: State<SubgroupsState>, track: Arc<Track>) -> Self {
		Self {
			info: track,
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
					return Ok(state.latest.clone());
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
		state.latest.as_ref().map(|group| (group.group_id, group.latest()))
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
	next: u64,
}

impl SubgroupWriter {
	fn new(state: State<SubgroupState>, group: Arc<SubgroupInfo>) -> Self {
		Self {
			state,
			info: group,
			next: 0,
		}
	}

	/// Create the next object ID with the given payload.
	pub fn write(&mut self, payload: bytes::Bytes) -> Result<(), ServeError> {
		let mut object = self.create(payload.len())?;
		object.write(payload)?;
		Ok(())
	}

	/// Write an object over multiple writes.
	///
	/// BAD STUFF will happen if the size is wrong; this is an advanced feature.
	pub fn create(&mut self, size: usize) -> Result<SubgroupObjectWriter, ServeError> {
		let (writer, reader) = SubgroupObject {
			group: self.info.clone(),
			object_id: self.next,
			status: ObjectStatus::Object,
			size,
		}
		.produce();

		self.next += 1;

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
	index: usize,
}

impl SubgroupReader {
	fn new(state: State<SubgroupState>, subgroup: Arc<SubgroupInfo>) -> Self {
		Self {
			state,
			info: subgroup,
			index: 0,
		}
	}

	pub fn latest(&self) -> u64 {
		let state = self.state.lock();
		state.objects.last().map(|o| o.object_id).unwrap_or_default()
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

				if self.index < state.objects.len() {
					let object = state.objects[self.index].clone();
					self.index += 1;
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
		self.index
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
