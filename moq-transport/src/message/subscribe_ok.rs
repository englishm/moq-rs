use crate::coding::{Decode, DecodeError, Encode, EncodeError};
use crate::message::GroupOrder;

/// Sent by the publisher to accept a Subscribe.
#[derive(Clone, Debug)]
pub struct SubscribeOk {
	/// The ID for this subscription.
	pub id: u64,

	/// The subscription will expire in this many milliseconds.
	pub expires: Option<u64>,

	// Order groups will be delivered in
	pub group_order: GroupOrder,

	/// The latest group and object for the track.
	pub latest: Option<(u64, u64)>,
}

impl Decode for SubscribeOk {
	fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
		let id = u64::decode(r)?;
		let expires = match u64::decode(r)? {
			0 => None,
			expires => Some(expires),
		};

		let group_order = GroupOrder::decode(r)?;

		Self::decode_remaining(r, 1)?;

		let latest = match r.get_u8() {
			0 => None,
			1 => Some((u64::decode(r)?, u64::decode(r)?)),
			_ => return Err(DecodeError::InvalidValue),
		};

		// Skip the parameters.
		// TODO: Implement parameters for SubscribeOk
		let _ = u8::decode(r)?;

		Ok(Self {
			id,
			expires,
			group_order,
			latest,
		})
	}
}

impl Encode for SubscribeOk {
	fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
		self.id.encode(w)?;
		self.expires.unwrap_or(0).encode(w)?;

		self.group_order.encode(w)?;

		Self::encode_remaining(w, 1)?;

		match self.latest {
			Some((group, object)) => {
				w.put_u8(1);
				group.encode(w)?;
				object.encode(w)?;
			}
			None => {
				w.put_u8(0);
			}
		}

		// Add 0 for the length of the parameters
		w.put_u8(0);

		Ok(())
	}
}
