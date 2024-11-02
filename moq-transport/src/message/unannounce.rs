use crate::coding::{Decode, DecodeError, Encode, EncodeError, Tuple};

/// Sent by the publisher to terminate an Announce.
#[derive(Clone, Debug)]
pub struct Unannounce {
	// Echo back the namespace that was reset
	pub namespace: Tuple,
}

impl Decode for Unannounce {
	fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
		let namespace = Tuple::decode(r)?;

		Ok(Self { namespace })
	}
}

impl Encode for Unannounce {
	fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
		self.namespace.encode(w)?;

		Ok(())
	}
}
