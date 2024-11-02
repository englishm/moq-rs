use crate::coding::{Decode, DecodeError, Encode, EncodeError};

/// Unsubscribe Namespace
/// https://www.ietf.org/archive/id/draft-ietf-moq-transport-06.html#name-unsubscribe_namespace
#[derive(Clone, Debug)]
pub struct UnsubscribeNamespace {
	// Echo back the namespace that was reset
	// TODO: convert this to tuple
	pub namespace_prefix: String,
}

impl Decode for UnsubscribeNamespace {
	fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
		let namespace_prefix = String::decode(r)?;
		Ok(Self { namespace_prefix })
	}
}

impl Encode for UnsubscribeNamespace {
	fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
		self.namespace_prefix.encode(w)?;
		Ok(())
	}
}
