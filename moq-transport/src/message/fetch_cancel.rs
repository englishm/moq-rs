use crate::coding::{Decode, DecodeError, Encode, EncodeError};

/// A subscriber issues a FETCH_CANCEL message to a publisher indicating it is
/// no longer interested in receiving Objects for the fetch specified by 'Subscribe ID'.
#[derive(Clone, Debug)]
pub struct FetchCancel {
    /// The subscription ID
    pub id: u64,
}

impl Decode for FetchCancel {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let id = u64::decode(r)?;
        Ok(Self { id })
    }
}

impl Encode for FetchCancel {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.id.encode(w)?;

        Ok(())
    }
}
