use crate::coding::{Decode, DecodeError, Encode, EncodeError};

/// Sent by the publisher to update the max allowed subscription ID for the session.
#[derive(Clone, Debug)]
pub struct MaxSubscribeId {
	/// The max allowed subscription ID
	pub id: u64,
}

impl Decode for MaxSubscribeId {
	fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
		let id = u64::decode(r)?;

		Ok(Self { id })
	}
}

impl Encode for MaxSubscribeId {
	fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
		self.id.encode(w)?;

		Ok(())
	}
}
