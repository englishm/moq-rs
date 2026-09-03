// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::message::{self, Message};
use std::fmt;

macro_rules! publisher_msgs {
    {$($name:ident,)*} => {
		#[derive(Clone)]
		pub enum Publisher {
			$($name(message::$name)),*
		}

		$(impl From<message::$name> for Publisher {
			fn from(msg: message::$name) -> Self {
				Publisher::$name(msg)
			}
		})*

		impl From<Publisher> for Message {
			fn from(p: Publisher) -> Self {
				match p {
					$(Publisher::$name(m) => Message::$name(m),)*
				}
			}
		}

		impl TryFrom<Message> for Publisher {
			type Error = Message;

			fn try_from(m: Message) -> Result<Self, Self::Error> {
				match m {
					$(Message::$name(m) => Ok(Publisher::$name(m)),)*
					_ => Err(m),
				}
			}
		}

		impl fmt::Debug for Publisher {
			// Delegate to the message formatter
			fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
				match self {
					$(Self::$name(ref m) => m.fmt(f),)*
				}
			}
		}
    }
}

// Defines messages that a PUBLISHER would send, or that a SUBSCRIBER would handle.
// RequestOk and RequestError are shared responses (draft-16 §9.7 / §9.8).
publisher_msgs! {
    // Namespace advertisement and termination.
    PublishNamespace,
    PublishNamespaceDone,
    // Publisher-initiated subscriptions.
    Publish,
    PublishDone,
    PublishBlocked,
    // Responses to subscriber-initiated requests.
    SubscribeOk,
    RequestOk,
    RequestError,
    // FETCH response; FETCH itself is still unsupported by the session layer.
    FetchOk,
}
