// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MoQ Relay library for building Media over QUIC relay servers.
//!
//! This crate provides the core relay functionality that can be embedded
//! into other applications. The relay handles:
//!
//! - Accepting QUIC connections from publishers and subscribers
//! - Routing media between local and remote endpoints
//! - Coordinating namespace/track registration across relay clusters
//!
//! # Example
//!
//! ```rust,ignore
//! use std::sync::Arc;
//! use moq_relay_ietf::{RelayConfig, FileCoordinator, SessionConfig};
//!
//! // Create a coordinator (FileCoordinator for multi-relay deployments)
//! let coordinator = FileCoordinator::new("/path/to/coordination/file", "https://relay.example.com");
//!
//! // Configure and create the relay
//! let relay = RelayConfig {
//!     bind: "[::]:443".parse().unwrap(),
//!     tls: tls_config,
//!     coordinator,
//!     session: SessionConfig::default(),
//!     // ... other options
//! }
//! .build()?;
//!
//! // Run the relay
//! relay.run().await?;
//! ```

mod api;
mod consumer;
mod coordinator;
mod covering_prefix_set;
mod interest;
mod local;
pub mod metrics;
mod producer;
mod relay;
mod remote;
mod session;
mod upstream_namespaces;
mod web;

pub use api::*;
pub use consumer::*;
pub use coordinator::*;
pub use local::*;
pub use moq_transport::session::SessionConfig;
pub use producer::*;
pub use relay::*;
pub use remote::RemoteManager;
pub use session::*;
pub use web::*;
