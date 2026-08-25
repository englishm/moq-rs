// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::net;

use anyhow::Context;
use clap::Parser;
use url::Url;

use moq_native_ietf::quic;
use moq_sub::media::Media;
use moq_transport::{coding::TrackNamespace, serve::Tracks};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing with env filter (respects RUST_LOG environment variable)
    // Default to info level, but suppress quinn's verbose output
    //
    // Logs go to stderr so they can't corrupt the fMP4 byte stream this
    // binary writes to stdout (e.g. `moq-sub ... | ffplay -`).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,quinn=warn")),
        )
        .init();

    let out = tokio::io::stdout();

    let config = Config::parse();
    let tls = config.tls.load()?;
    let quic = quic::Endpoint::new(quic::Config::new(config.bind, None, tls)?)?;

    let (session, connection_id, transport) = quic.client.connect(&config.url, None).await?;

    tracing::info!(
        "connected with CID: {} (use this to look up qlog/mlog on server)",
        connection_id
    );

    let (session, subscriber) = moq_transport::session::Subscriber::connect(
        session,
        moq_transport::session::SessionId::new(connection_id.clone()),
        transport,
    )
    .await
    .context("failed to create MoQ Transport session")?;

    // Associate empty set of Tracks with provided namespace
    let tracks = Tracks::new(TrackNamespace::from_utf8_path(&config.name));

    let mut media = Media::new(subscriber, tracks, out, config.catalog).await?;

    tokio::select! {
        res = session.run() => res.context("session error")?,
        res = media.run() => res.context("media error")?,
    }

    Ok(())
}

#[derive(Parser, Clone)]
pub struct Config {
    /// Listen for UDP packets on the given address.
    #[arg(long, default_value = "[::]:0")]
    pub bind: net::SocketAddr,

    /// Connect to the given URL starting with https://
    #[arg(value_parser = moq_url)]
    pub url: Url,

    /// The name of the broadcast
    #[arg(long)]
    pub name: String,

    /// The TLS configuration.
    #[command(flatten)]
    pub tls: moq_native_ietf::tls::Args,

    /// Request the catalog track (to get other track names)
    ///
    /// First download the track named ".catalog" to find out the
    /// track names to subscribe to.  Other parameters like video
    /// dimension are extracted from the tracks themselves.  Default:
    /// "0.mp4" for the init track, "{track_id}.m4s" for the rest.
    #[arg(long)]
    pub catalog: bool,
}

fn moq_url(s: &str) -> Result<Url, String> {
    let url = Url::try_from(s).map_err(|e| e.to_string())?;

    // Make sure the scheme is moq
    if url.scheme() != "https" && url.scheme() != "moqt" {
        return Err("url scheme must be https:// for WebTransport & moqt:// for QUIC".to_string());
    }

    Ok(url)
}
