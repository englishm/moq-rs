use crate::coding::{Decode, DecodeError, Encode, EncodeError, Params};
use crate::message::GroupOrder;

/// Sent by the publisher to accept a Subscribe.
#[derive(Clone, Debug)]
pub struct SubscribeOk {
    /// The request ID for this subscription.
    pub id: u64,

    /// The track alias for this subscription.
    pub track_alias: u64,

    /// The subscription will expire in this many milliseconds.
    pub expires: Option<u64>, // TODO: treat as VarInt

    // Order groups will be delivered in
    pub group_order: GroupOrder,

    pub content_exists: u8, // TODO: enum

    /// The latest group and object for the track.
    pub largest_location: Option<(u64, u64)>,

    pub params: Params,
}

impl Decode for SubscribeOk {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let id = u64::decode(r)?;
        let track_alias = u64::decode(r)?;
        let expires = match u64::decode(r)? {
            0 => None,
            expires => Some(expires),
        };

        let group_order = GroupOrder::decode(r)?;

        let content_exists = u8::decode(r)?;

        Self::decode_remaining(r, 1)?;

        let latest = match r.get_u8() {
            0 => None,
            1 => Some((u64::decode(r)?, u64::decode(r)?)),
            _ => return Err(DecodeError::InvalidValue),
        };

        // TODO: Do stuff with parameters for SubscribeOk
        let params = Params::decode(r)?;

        Ok(Self {
            id,
            track_alias,
            expires,
            group_order,
            content_exists,
            largest_location: latest,
            params,
        })
    }
}

impl Encode for SubscribeOk {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.id.encode(w)?;
        self.expires.unwrap_or(0).encode(w)?;

        self.group_order.encode(w)?;

        Self::encode_remaining(w, 1)?;

        match self.largest_location {
            Some((group, object)) => {
                w.put_u8(1);
                group.encode(w)?;
                object.encode(w)?;
            }
            None => {
                w.put_u8(0);
            }
        }

        // Add 0 for the length of the parameters
        w.put_u8(0);

        Ok(())
    }
}
