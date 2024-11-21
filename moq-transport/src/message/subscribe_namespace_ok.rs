use crate::coding::{Decode, DecodeError, Encode, EncodeError, Tuple};

/// Subscribe Namespace Ok
/// https://www.ietf.org/archive/id/draft-ietf-moq-transport-06.html#name-subscribe_namespace_ok
#[derive(Clone, Debug)]
pub struct SubscribeNamespaceOk {
	// Echo back the namespace that was announced.
	pub namespace_prefix: Tuple,
}

impl Decode for SubscribeNamespaceOk {
	fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
		let namespace_prefix = Tuple::decode(r)?;
		Ok(Self { namespace_prefix })
	}
}

impl Encode for SubscribeNamespaceOk {
	fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
		self.namespace_prefix.encode(w)
	}
}
