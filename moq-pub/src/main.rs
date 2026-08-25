// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use bytes::BytesMut;
use std::net;
use url::Url;

use anyhow::Context;
use clap::Parser;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use moq_native_ietf::quic;
use moq_pub::Media;
use moq_transport::{
    coding::{KeyValuePairs, TrackName, TrackNamespace},
    serve,
    session::{Publisher, SessionId},
};

#[derive(Parser, Clone)]
pub struct Cli {
    /// Listen for UDP packets on the given address.
    #[arg(long, default_value = "[::]:0")]
    pub bind: net::SocketAddr,

    /// Advertise this frame rate in the catalog (informational)
    // TODO auto-detect this from the input when not provided
    #[arg(long, default_value = "24")]
    pub fps: u8,

    /// Advertise this bit rate in the catalog (informational)
    // TODO auto-detect this from the input when not provided
    #[arg(long, default_value = "1500000")]
    pub bitrate: u32,

    /// Connect to the given URL starting with https://
    #[arg()]
    pub url: Url,

    /// The name of the broadcast
    #[arg(long)]
    pub name: String,

    /// The TLS configuration.
    #[command(flatten)]
    pub tls: moq_native_ietf::tls::Args,

    /// Push tracks with PUBLISH after sending PUBLISH_NAMESPACE.
    #[arg(long)]
    pub publish: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing with env filter (respects RUST_LOG environment variable)
    // Default to info level, but suppress quinn's verbose output
    //
    // Logs go to stderr, per convention and to keep stdout clean.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,quinn=warn")),
        )
        .init();

    let cli = Cli::parse();

    let (writer, _, reader) =
        serve::Tracks::new(TrackNamespace::from_utf8_path(&cli.name)).produce();

    let (track_tx, track_rx) = if cli.publish {
        let (tx, rx) = mpsc::unbounded_channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let media = Media::new(writer, track_tx)?;

    let tls = cli.tls.load()?;

    let quic = quic::Endpoint::new(moq_native_ietf::quic::Config::new(
        cli.bind,
        None,
        tls.clone(),
    )?)?;

    tracing::info!("connecting to relay: url={}", cli.url);
    let (session, connection_id, transport) = quic.client.connect(&cli.url, None).await?;

    tracing::info!(
        "connected with CID: {} (use this to look up qlog/mlog on server)",
        connection_id
    );

    let (session, publisher) =
        Publisher::connect(session, SessionId::new(connection_id.clone()), transport)
            .await
            .context("failed to create MoQ Transport publisher")?;

    let mut namespace_publisher = publisher.clone();
    let publish_publisher = publisher;
    let namespace_reader = reader.clone();
    let publish_reader = reader.clone();

    tokio::select! {
        res = session.run() => res.context("session error")?,
        res = run_media(media) => {
            res.context("media error")?
        },
        res = namespace_publisher.publish_namespace(namespace_reader) => res.context("publisher error")?,
        res = async {
            if let Some(track_rx) = track_rx {
                publish_created_tracks(publish_publisher, publish_reader, track_rx).await?;
            }
            Ok::<_, anyhow::Error>(())
        }, if cli.publish => res.context("publish tracks error")?,
    }

    Ok(())
}

async fn publish_created_tracks(
    mut publisher: Publisher,
    mut tracks: serve::TracksReader,
    mut track_rx: mpsc::UnboundedReceiver<TrackName>,
) -> anyhow::Result<()> {
    let mut tasks = JoinSet::new();

    loop {
        tokio::select! {
            Some(track_name) = track_rx.recv() => {
                let namespace = tracks.namespace.clone();
                let Some(track) = tracks.get_track_reader(&namespace, track_name.clone()) else {
                    tracing::warn!(namespace = %namespace, track = %track_name, "created track was not found in TracksReader");
                    continue;
                };

                tracing::info!(namespace = %namespace, track = %track_name, "sending PUBLISH for track");
                let published = publisher
                    .publish(track, KeyValuePairs::default())
                    .await
                    .with_context(|| format!("failed to send PUBLISH for track {}/{}", namespace, track_name))?;

                tasks.spawn(async move {
                    published
                        .serve()
                        .await
                        .context("failed serving PUBLISH track")
                });
            },
            Some(res) = tasks.join_next(), if !tasks.is_empty() => {
                res.context("PUBLISH track task panicked")??;
            },
            else => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_flag_defaults_false() {
        let cli = Cli::try_parse_from(["moq-pub", "https://example.com/watch", "--name", "test"])
            .unwrap();
        assert!(!cli.publish);
    }

    #[test]
    fn publish_flag_sets_true() {
        let cli = Cli::try_parse_from([
            "moq-pub",
            "https://example.com/watch",
            "--name",
            "test",
            "--publish",
        ])
        .unwrap();
        assert!(cli.publish);
    }
}

async fn run_media(mut media: Media) -> anyhow::Result<()> {
    let mut input = tokio::io::stdin();
    let mut buf = BytesMut::new();
    loop {
        input
            .read_buf(&mut buf)
            .await
            .context("failed to read from stdin")?;
        media.parse(&mut buf).context("failed to parse media")?;
    }
}
