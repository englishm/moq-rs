use crate::coding::{Decode, DecodeError, Encode, EncodeError, Params, Tuple};
use crate::message::FilterType;
use crate::message::GroupOrder;

/// Sent by the subscriber to request all future objects for the given track.
///
/// Objects will use the provided ID instead of the full track name, to save bytes.
#[derive(Clone, Debug)]
pub struct Subscribe {
    /// The subscription ID
    pub id: u64,

    /// Track properties
    pub track_alias: u64, // This alias is useless but part of the spec
    pub track_namespace: Tuple,
    pub track_name: String,

    /// Subscriber Priority
    pub subscriber_priority: u8,

    /// Group Order
    pub group_order: GroupOrder,

    /// Filter type
    pub filter_type: FilterType,

    /// Start and End depending on the Filter Type set
    pub start: Option<SubscribePair>,
    pub end_group: Option<u64>,

    /// Optional parameters
    pub params: Params,
}

impl Decode for Subscribe {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let id = u64::decode(r)?;
        let track_alias = u64::decode(r)?;
        let track_namespace = Tuple::decode(r)?;
        let track_name = String::decode(r)?;

        let subscriber_priority = u8::decode(r)?;
        let group_order = GroupOrder::decode(r)?;

        let filter_type = FilterType::decode(r)?;

        let start: Option<SubscribePair>;
        let end_group: Option<u64>;
        match filter_type {
            FilterType::AbsoluteStart => {
                if r.remaining() < 2 {
                    return Err(DecodeError::MissingField);
                }
                start = Some(SubscribePair::decode(r)?);
                end_group = None;
            }
            FilterType::AbsoluteRange => {
                if r.remaining() < 4 {
                    return Err(DecodeError::MissingField);
                }
                start = Some(SubscribePair::decode(r)?);
                end_group = Some(u64::decode(r)?);
            }
            _ => {
                start = None;
                end_group = None;
            }
        }

        let params = Params::decode(r)?;

        Ok(Self {
            id,
            track_alias,
            track_namespace,
            track_name,
            subscriber_priority,
            group_order,
            filter_type,
            start,
            end_group,
            params,
        })
    }
}

impl Encode for Subscribe {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.id.encode(w)?;
        self.track_alias.encode(w)?;
        self.track_namespace.encode(w)?;
        self.track_name.encode(w)?;

        self.subscriber_priority.encode(w)?;

        self.group_order.encode(w)?;

        self.filter_type.encode(w)?;

        if self.filter_type == FilterType::AbsoluteStart
            || self.filter_type == FilterType::AbsoluteRange
        {
            if let Some(start) = &self.start {
                start.encode(w)?;
            } else {
                return Err(EncodeError::MissingField);
            }
        }

        if self.filter_type == FilterType::AbsoluteRange {
            if let Some(end_group) = &self.end_group {
                end_group.encode(w)?;
            } else {
                return Err(EncodeError::MissingField);
            }
        }

        self.params.encode(w)?;

        Ok(())
    }
}

// Note: When derived on structs, it will produce a lexicographic ordering
//       based on the top-to-bottom declaration order of the struct’s members.
//       Therefore, it will work just as expected.
#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct SubscribePair {
    pub group: u64,
    pub object: u64,
}

impl Decode for SubscribePair {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        Ok(Self {
            group: u64::decode(r)?,
            object: u64::decode(r)?,
        })
    }
}

impl Encode for SubscribePair {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.group.encode(w)?;
        self.object.encode(w)?;
        Ok(())
    }
}
