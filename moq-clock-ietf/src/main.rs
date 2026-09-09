// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use moq_native_ietf::quic;

use anyhow::Context;

mod cli;
mod clock;

use clap::Parser;
use cli::Cli;

use moq_transport::{
    coding::TrackNamespace,
    serve,
    session::{Publisher, SessionId, Subscriber},
    setup::AuthorizationToken,
};

/// The main entry point for the MoQ Clock IETF example.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing with env filter (respects RUST_LOG environment variable)
    // Default to info level, but suppress quinn's verbose output
    //
    // Logs go to stderr so they stay separate from the clock values this
    // binary prints to stdout.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,quinn=warn")),
        )
        .init();

    let mut config = Cli::parse();
    let authorization_tokens = std::mem::take(&mut config.authorization_tokens)
        .into_iter()
        .map(|token| AuthorizationToken::new(token.token_type, token.value))
        .collect::<Result<Vec<_>, _>>()
        .context("authorization token type exceeds the QUIC varint range")?;
    let tls = config.tls.load()?;

    // Create the QUIC endpoint
    let quic = quic::Endpoint::new(quic::Config::new(config.bind, None, tls)?)?;

    tracing::info!("connecting to server: url={}", config.url);

    // Connect to the server
    let (session, connection_id, transport) = quic.client.connect(&config.url, None).await?;

    tracing::info!(
        "connected with CID: {} (use this to look up qlog/mlog on server)",
        connection_id
    );

    // Depending on whether we are publishing or subscribing, create the appropriate session
    if config.publish {
        // Create the publisher session
        let (session, mut publisher) = Publisher::connect_with_session_id_and_tokens(
            session,
            SessionId::new(connection_id.clone()),
            transport,
            authorization_tokens,
        )
        .await
        .context("failed to create MoQ Transport session")?;

        if config.datagrams {
            tracing::info!("publishing clock via datagrams");

            let (mut tracks_writer, _, tracks_reader) = serve::Tracks {
                namespace: TrackNamespace::from_utf8_path(&config.namespace),
            }
            .produce();

            let track_writer = tracks_writer.create(&config.track).unwrap();
            let clock_publisher = clock::Publisher::new_datagram(track_writer.datagrams()?);

            tokio::select! {
                res = session.run() => res.context("session error")?,
                res = clock_publisher.run() => res.context("clock error")?,
                res = publisher.publish_namespace(tracks_reader) => res.context("failed to serve tracks")?,
            }
        } else {
            tracing::info!("publishing clock via streams");

            let (mut tracks_writer, _, tracks_reader) = serve::Tracks {
                namespace: TrackNamespace::from_utf8_path(&config.namespace),
            }
            .produce();

            let track_writer = tracks_writer.create(&config.track).unwrap();
            let clock_publisher = clock::Publisher::new(track_writer.subgroups()?);

            tokio::select! {
                res = session.run() => res.context("session error")?,
                res = clock_publisher.run() => res.context("clock error")?,
                res = publisher.publish_namespace(tracks_reader) => res.context("failed to serve tracks")?,
            }
        }
    } else {
        // Create the subscriber session
        let (session, mut subscriber) = Subscriber::connect_with_session_id_and_tokens(
            session,
            SessionId::new(connection_id.clone()),
            transport,
            authorization_tokens,
        )
        .await
        .context("failed to create MoQ Transport session")?;

        let track_namespace = TrackNamespace::from_utf8_path(&config.namespace);

        if config.track_status {
            // Request a track_status for the clock track (testing purposes only)
            subscriber.track_status(&track_namespace, &config.track);
        }

        let (track_writer, track_reader) =
            serve::Track::new(track_namespace, config.track).produce();

        let clock_subscriber = clock::Subscriber::new(track_reader);

        tokio::select! {
            res = session.run() => res.context("session error")?,
            res = clock_subscriber.run() => res.context("clock error")?,
            res = subscriber.subscribe(track_writer) => res.context("failed to subscribe to track")?,
        }
    }

    Ok(())
}
