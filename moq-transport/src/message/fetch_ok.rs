use crate::coding::{Decode, DecodeError, Encode, EncodeError, Params};

/// A publisher sends a FETCH_OK control message in response to successful fetches.
#[derive(Clone, Debug)]
pub struct FetchOk {
    /// The subscription ID
    pub id: u64,

    /// Order groups will be delivered in
    pub group_order: u8,

    /// True if all objects have been published on this track
    pub end_of_track: u8,

    /// The largest Group ID available for this track (last if end_of_track)
    pub largest_group_id: u64,
    /// The largest Object ID available within the largest Group ID for this track (last if end_of_track)
    pub largest_object_id: u64,

    /// Optional parameters
    pub params: Params,
}

impl Decode for FetchOk {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let id = u64::decode(r)?;

        let group_order = u8::decode(r)?;

        let end_of_track = u8::decode(r)?;

        let largest_group_id = u64::decode(r)?;
        let largest_object_id = u64::decode(r)?;

        let params = Params::decode(r)?;

        Ok(Self {
            id,
            group_order,
            end_of_track,
            largest_group_id,
            largest_object_id,
            params,
        })
    }
}

impl Encode for FetchOk {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.id.encode(w)?;

        self.group_order.encode(w)?;

        self.end_of_track.encode(w)?;

        self.largest_group_id.encode(w)?;
        self.largest_object_id.encode(w)?;

        self.params.encode(w)?;

        Ok(())
    }
}
