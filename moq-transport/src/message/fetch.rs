use crate::coding::{Decode, DecodeError, Encode, EncodeError, Params, Tuple};
use crate::message::GroupOrder;

/// Sent by the subscriber to request to request a range
/// of already published objects within a track.
#[derive(Clone, Debug)]
pub struct Fetch {
    /// The subscription ID
    pub id: u64,

    /// Track properties
    pub track_namespace: Tuple,
    pub track_name: String,

    /// Subscriber Priority
    pub subscriber_priority: u8,

    pub group_order: GroupOrder,

    /// The start/end group/object.
    pub start_group: u64,
    pub start_object: u64,
    pub end_group: u64,
    pub end_object: u64,

    /// Optional parameters
    pub params: Params,
}

impl Decode for Fetch {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let id = u64::decode(r)?;

        let track_namespace = Tuple::decode(r)?;
        let track_name = String::decode(r)?;

        let subscriber_priority = u8::decode(r)?;

        let group_order = GroupOrder::decode(r)?;

        let start_group = u64::decode(r)?;
        let start_object = u64::decode(r)?;
        let end_group = u64::decode(r)?;
        let end_object = u64::decode(r)?;

        let params = Params::decode(r)?;

        Ok(Self {
            id,
            track_namespace,
            track_name,
            subscriber_priority,
            group_order,
            start_group,
            start_object,
            end_group,
            end_object,
            params,
        })
    }
}

impl Encode for Fetch {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.id.encode(w)?;

        self.track_namespace.encode(w)?;
        self.track_name.encode(w)?;

        self.subscriber_priority.encode(w)?;

        self.group_order.encode(w)?;

        self.start_group.encode(w)?;
        self.start_object.encode(w)?;
        self.end_group.encode(w)?;
        self.end_object.encode(w)?;

        self.params.encode(w)?;

        Ok(())
    }
}
