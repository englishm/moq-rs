// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use clap::Parser;

mod server;
use moq_api::ApiError;
use server::{Server, ServerConfig};

#[tokio::main]
async fn main() -> Result<(), ApiError> {
    // Initialize tracing with env filter (respects RUST_LOG environment variable)
    //
    // Logs go to stderr, per convention and to keep stdout clean.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = ServerConfig::parse();
    let server = Server::new(config);
    server.run().await
}
