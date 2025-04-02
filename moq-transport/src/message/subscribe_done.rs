use crate::coding::{Decode, DecodeError, Encode, EncodeError};

/// Sent by the publisher to cleanly terminate a Subscribe.
#[derive(Clone, Debug)]
pub struct SubscribeDone {
    /// The ID for this subscription
    pub id: u64,

    /// The error code
    pub code: u64,

    /// Number of DATA streams opened for this subscription
    /// (1 << 62) - 1 if it cannot be determined
    pub count: u64,

    /// An optional error reason
    pub reason: String,
}

impl Decode for SubscribeDone {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let id = u64::decode(r)?;
        let code = u64::decode(r)?;
        let count = u64::decode(r)?;
        let reason = String::decode(r)?;

        Ok(Self {
            id,
            code,
            count,
            reason,
        })
    }
}

impl Encode for SubscribeDone {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.id.encode(w)?;
        self.code.encode(w)?;
        self.count.encode(w)?;
        self.reason.encode(w)?;

        Ok(())
    }
}
