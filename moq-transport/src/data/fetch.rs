use crate::coding::{Decode, DecodeError, Encode, EncodeError};

use super::ObjectStatus;

#[derive(Clone, Debug)]
pub struct FetchHeader {
    // The subscribe ID.
    pub subscribe_id: u64,

    // TODO remove these fields (only here to satisfy the macro)
    pub track_alias: u64,
    pub publisher_priority: u8,
}

impl Decode for FetchHeader {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        Ok(Self {
            subscribe_id: u64::decode(r)?,
            track_alias: 0,
            publisher_priority: 0,
        })
    }
}
impl Encode for FetchHeader {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.subscribe_id.encode(w)?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FetchObject {
    pub group_id: u64,
    pub subgroup_id: u64,
    pub object_id: u64,
    pub publisher_priority: u8,
    pub size: u64,
    pub status: ObjectStatus,
}

impl Decode for FetchObject {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let group_id = u64::decode(r)?;
        let subgroup_id = u64::decode(r)?;
        let object_id = u64::decode(r)?;
        let publisher_priority = u8::decode(r)?;
        let size = u64::decode(r)?;

        // If the size is 0, then the status is sent explicitly.
        // Otherwise, the status is assumed to be 0x0 (Object).
        let status = if size == 0 {
            ObjectStatus::decode(r)?
        } else {
            ObjectStatus::Object
        };

        Ok(Self {
            group_id,
            subgroup_id,
            object_id,
            publisher_priority,
            size,
            status,
        })
    }
}

impl Encode for FetchObject {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.group_id.encode(w)?;
        self.subgroup_id.encode(w)?;
        self.object_id.encode(w)?;
        self.publisher_priority.encode(w)?;
        self.size.encode(w)?;

        // If the size is 0, then the status is sent explicitly.
        // Otherwise, the status is assumed to be 0x0 (Object).
        if self.size == 0 {
            self.status.encode(w)?;
        }

        Ok(())
    }
}
