use crate::coding::{Decode, DecodeError, Encode, EncodeError};
use crate::data::ObjectStatus;

#[derive(Clone, Debug)]
pub struct SubgroupHeader {
	// The subscribe ID.
	pub subscribe_id: u64,

	// The track alias.
	pub track_alias: u64,

	// The group sequence number
	pub group_id: u64,

	// The subgroup sequence number
	pub subgroup_id: u64,

	// Publisher priority, where **smaller** values are sent first.
	pub publisher_priority: u8,
}

impl Decode for SubgroupHeader {
	fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
		Ok(Self {
			subscribe_id: u64::decode(r)?,
			track_alias: u64::decode(r)?,
			group_id: u64::decode(r)?,
			subgroup_id: u64::decode(r)?,
			publisher_priority: u8::decode(r)?,
		})
	}
}

impl Encode for SubgroupHeader {
	fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
		self.subscribe_id.encode(w)?;
		self.track_alias.encode(w)?;
		self.group_id.encode(w)?;
		self.subgroup_id.encode(w)?;
		self.publisher_priority.encode(w)?;

		Ok(())
	}
}

#[derive(Clone, Debug)]
pub struct SubgroupObject {
	pub object_id: u64,
	pub size: usize,
	pub status: ObjectStatus,
}

impl Decode for SubgroupObject {
	fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
		let object_id = u64::decode(r)?;
		let size = usize::decode(r)?;

		// If the size is 0, then the status is sent explicitly.
		// Otherwise, the status is assumed to be 0x0 (Object).
		let status = if size == 0 {
			ObjectStatus::decode(r)?
		} else {
			ObjectStatus::Object
		};

		Ok(Self {
			object_id,
			size,
			status,
		})
	}
}

impl Encode for SubgroupObject {
	fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
		self.object_id.encode(w)?;
		self.size.encode(w)?;

		// If the size is 0, then the status is sent explicitly.
		// Otherwise, the status is assumed to be 0x0 (Object).
		if self.size == 0 {
			self.status.encode(w)?;
		}

		Ok(())
	}
}
