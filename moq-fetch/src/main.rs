use anyhow::Context;
use clap::Parser;
use moq_native_ietf::quic;
use std::collections::HashMap;
use std::net;
use url::Url;

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
}

fn moq_url(s: &str) -> Result<Url, String> {
    let url = Url::try_from(s).map_err(|e| e.to_string())?;

    // Make sure the scheme is moq
    if url.scheme() != "https" && url.scheme() != "moqt" {
        return Err("url scheme must be https:// for WebTransport & moqt:// for QUIC".to_string());
    }

    Ok(url)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let config = Config::parse();
    let tls = config.tls.load()?;
    let quic = quic::Endpoint::new(quic::Config {
        bind: config.bind,
        tls,
    })?;

    let session = quic.client.connect(&config.url).await?;

    dbg!();
    println!("Hello, world!");
    dbg!();
    let (session, mut subscriber) = moq_transport::session::Subscriber::connect(session)
        .await
        .context("failed to create MoQ Transport session")?;

    // Spawn a task to run the session
    let handle = tokio::spawn(async move {
        if let Err(e) = session.run().await {
            eprintln!("session error: {:?}", e);
        }
    });

    // Fetch a specific range of data from a known track
    //
    let track_namespace = moq_transport::coding::Tuple::from_utf8_path("bbb");
    let track_name = ".catalog".to_string();

    let fetch_msg = moq_transport::message::Fetch {
        id: 1,
        track_namespace,
        track_name,
        subscriber_priority: 1,
        group_order: moq_transport::message::GroupOrder::Ascending,
        start_group: 0,
        start_object: 0,
        end_group: 3,
        end_object: 0,

        params: moq_transport::coding::Params(HashMap::new()),
    };

    let track = moq_transport::serve::Track::new(fetch_msg.track_namespace, fetch_msg.track_name);
    let (track_writer, track_reader) = track.produce();

    dbg!();
    //Send the fetch message
    tokio::spawn(async move {
        subscriber.fetch(track_writer).await.unwrap_or_else(|err| {
            eprintln!("error: {:?}", err);
        });
    });
    dbg!();

    // This is wonky, but how we'd do it with the current API
    let data = match track_reader.mode().await? {
        moq_transport::serve::TrackReaderMode::Stream(mut stream) => {
            dbg!();
            let mut data = Vec::new();
            dbg!();
            while let Some(mut group) = stream.next().await? {
                dbg!();
                while let Some(bytes) = group.read_next().await? {
                    dbg!();
                    data.extend_from_slice(&bytes);
                    dbg!();
                }
            }
            dbg!();
            data
        }
        _ => panic!("unexpected mode"),
    };

    dbg!("got data!");
    dbg!(data);

    handle.await?;

    Ok(())
}
