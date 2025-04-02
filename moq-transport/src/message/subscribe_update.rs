use crate::coding::{Decode, DecodeError, Encode, EncodeError, Params};
use crate::message::subscribe::SubscribePair;
use crate::session::{SubscribeFilter, Subscriber};

/// Sent by the subscriber to request all future objects for the given track.
///
/// Objects will use the provided ID instead of the full track name, to save bytes.
#[derive(Clone, Debug)]
pub struct SubscribeUpdate {
    /// The subscription ID
    pub id: u64,

    /// Start and End depending on the Filter Type set
    pub start: SubscribePair,
    pub end_group: u64,

    // Subscriber Priority
    pub subscriber_priority: u8,

    /// Optional parameters
    pub params: Params,
}

impl SubscribeUpdate {
    pub fn new(mut subscriber: Subscriber, id: u64, filter: SubscribeFilter, priority: u8) -> Self {
        let (start, end_group) = match filter {
            SubscribeFilter::AbsoluteStart(start) => (start, 0),
            SubscribeFilter::AbsoluteRange(start, end_group) => (start, end_group),
            SubscribeFilter::LatestObject => (SubscribePair { group: 0, object: 0 }, 0),
        };

        let update = SubscribeUpdate {
            id,
            start,
            end_group,
            subscriber_priority: priority,
            params: Params::new(),
        };

        subscriber.send_message(update.clone());

        update
    }
}

impl Decode for SubscribeUpdate {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let id = u64::decode(r)?;

        let start = SubscribePair::decode(r)?;
        let end_group = u64::decode(r)?;

        let subscriber_priority = u8::decode(r)?;

        let params = Params::decode(r)?;

        Ok(Self {
            id,
            start,
            end_group,
            subscriber_priority,
            params,
        })
    }
}

impl Encode for SubscribeUpdate {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.id.encode(w)?;

        self.start.encode(w)?;
        self.end_group.encode(w)?;

        self.subscriber_priority.encode(w)?;

        self.params.encode(w)?;

        Ok(())
    }
}
