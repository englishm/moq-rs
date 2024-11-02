use crate::coding::{Decode, DecodeError, Encode, EncodeError};

/// Subscribe Namespace Error
/// https://www.ietf.org/archive/id/draft-ietf-moq-transport-06.html#name-subscribe_namespace_error
#[derive(Clone, Debug)]
pub struct SubscribeNamespaceError {
	// Echo back the namespace that was reset
	// TODO: convert this to tuple
	pub namespace_prefix: String,

	// An error code.
	pub code: u64,

	// An optional, human-readable reason.
	pub reason: String,
}

impl Decode for SubscribeNamespaceError {
	fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
		let namespace_prefix = String::decode(r)?;
		let code = u64::decode(r)?;
		let reason = String::decode(r)?;

		Ok(Self {
			namespace_prefix,
			code,
			reason,
		})
	}
}

impl Encode for SubscribeNamespaceError {
	fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
		self.namespace_prefix.encode(w)?;
		self.code.encode(w)?;
		self.reason.encode(w)?;

		Ok(())
	}
}
