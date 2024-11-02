use crate::coding::{Decode, DecodeError, Encode, EncodeError, Params, Tuple};

/// Sent by the publisher to announce the availability of a group of tracks.
#[derive(Clone, Debug)]
pub struct Announce {
	/// The track namespace
	pub namespace: Tuple,

	/// Optional parameters
	pub params: Params,
}

impl Decode for Announce {
	fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
		let namespace = Tuple::decode(r)?;
		let params = Params::decode(r)?;

		Ok(Self { namespace, params })
	}
}

impl Encode for Announce {
	fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
		self.namespace.encode(w)?;
		self.params.encode(w)?;

		Ok(())
	}
}
