// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Messages used for the MoQ Transport handshake.
//!
//! After establishing the WebTransport session, the client creates a bidirectional QUIC stream.
//! The client sends the [Client] message and the server responds with the [Server] message.
//! Both sides negotate the [Version] and [Role].

mod authorization_token;
mod client;
mod param_types;
mod server;
mod version;

pub use authorization_token::*;
pub use client::*;
pub use param_types::*;
pub use server::*;
pub use version::*;

/// ALPN identifier for draft-ietf-moq-transport-16.
/// Used for native QUIC connections; WebTransport uses the WT-Available-Protocols header instead.
pub const ALPN: &[u8] = b"moqt-16";
