use crate::coding::{Decode, DecodeError, Encode, EncodeError};

/// Sent by the server to indicate that the client should connect to a different server.
#[derive(Clone, Debug)]
pub struct FetchError {
    /// The ID for this subscription.
    pub id: u64,

    /// An error code.
    pub code: u64,

    /// An optional, human-readable reason.
    pub reason: String,
}

impl Decode for FetchError {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let id = u64::decode(r)?;

        let code = u64::decode(r)?;
        let reason = String::decode(r)?;

        Ok(Self { id, code, reason })
    }
}

impl Encode for FetchError {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.id.encode(w)?;

        self.code.encode(w)?;
        self.reason.encode(w)?;

        Ok(())
    }
}
