use crate::coding::{Decode, DecodeError, Encode, EncodeError};
use paste::paste;
use std::fmt;

use super::{SubgroupHeader, TrackHeader};

// Use a macro to generate the message types rather than copy-paste.
// This implements a decode/encode method that uses the specified type.
macro_rules! header_types {
    {$($name:ident = $val:expr,)*} => {
		/// All supported message types.
		#[derive(Clone)]
		pub enum Header {
			$($name(paste! { [<$name Header>] })),*
		}

		impl Decode for Header {
			fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
				let t = u64::decode(r)?;

				match t {
					$($val => {
						let msg = <paste! { [<$name Header>] }>::decode(r)?;
						Ok(Self::$name(msg))
					})*
					_ => Err(DecodeError::InvalidMessage(t)),
				}
			}
		}

		impl Encode for Header {
			fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
				match self {
					$(Self::$name(ref m) => {
						self.id().encode(w)?;
						m.encode(w)
					},)*
				}
			}
		}

		impl Header {
			pub fn id(&self) -> u64 {
				match self {
					$(Self::$name(_) => {
						$val
					},)*
				}
			}

			pub fn subscribe_id(&self) -> u64 {
				match self {
					$(Self::$name(o) => o.subscribe_id,)*
				}
			}

			pub fn track_alias(&self) -> u64 {
				match self {
					$(Self::$name(o) => o.track_alias,)*
				}
			}

			pub fn publisher_priority(&self) -> u8 {
				match self {
					$(Self::$name(o) => o.publisher_priority,)*
				}
			}
		}

		$(impl From<paste! { [<$name Header>] }> for Header {
			fn from(m: paste! { [<$name Header>] }) -> Self {
				Self::$name(m)
			}
		})*

		impl fmt::Debug for Header {
			// Delegate to the message formatter
			fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
				match self {
					$(Self::$name(ref m) => m.fmt(f),)*
				}
			}
		}
    }
}

// Each stream type is prefixed with the given VarInt type.
// https://www.ietf.org/archive/id/draft-ietf-moq-transport-06.html#section-7
header_types! {
	//Datagram = 0x1,
	Track = 0x2,
	Subgroup = 0x4,
}
