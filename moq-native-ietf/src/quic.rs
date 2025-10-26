use std::{
    fs::File,
    io::BufWriter,
    net,
    path::PathBuf,
    sync::{Arc, Mutex},
    time,
};

use anyhow::Context;
use clap::Parser;
use url::Url;

use crate::tls;

use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use futures::FutureExt;

/// Build a TransportConfig with our standard settings
///
/// This is used both for the base endpoint config and when creating
/// per-connection configs with qlog enabled.
fn build_transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(time::Duration::from_secs(10).try_into().unwrap()));
    transport.keep_alive_interval(Some(time::Duration::from_secs(4))); // TODO make this smarter
    transport.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    transport.mtu_discovery_config(None); // Disable MTU discovery
    transport
}

#[derive(Parser, Clone)]
pub struct Args {
    /// Listen for UDP packets on the given address.
    #[arg(long, default_value = "[::]:0")]
    pub bind: net::SocketAddr,

    /// Directory to write qlog files (one per connection)
    #[arg(long)]
    pub qlog_dir: Option<PathBuf>,

    #[command(flatten)]
    pub tls: tls::Args,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            bind: "[::]:0".parse().unwrap(),
            qlog_dir: None,
            tls: Default::default(),
        }
    }
}

impl Args {
    pub fn load(&self) -> anyhow::Result<Config> {
        let tls = self.tls.load()?;
        Ok(Config {
            bind: self.bind,
            qlog_dir: self.qlog_dir.clone(),
            tls,
        })
    }
}

pub struct Config {
    pub bind: net::SocketAddr,
    pub qlog_dir: Option<PathBuf>,
    pub tls: tls::Config,
}

pub struct Endpoint {
    pub client: Client,
    pub server: Option<Server>,
}

impl Endpoint {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        // Validate qlog directory if provided
        if let Some(qlog_dir) = &config.qlog_dir {
            if !qlog_dir.exists() {
                anyhow::bail!("qlog directory does not exist: {}", qlog_dir.display());
            }
            if !qlog_dir.is_dir() {
                anyhow::bail!("qlog path is not a directory: {}", qlog_dir.display());
            }
            log::info!("qlog output enabled: {}", qlog_dir.display());
        }

        // Build transport config with our standard settings
        let transport = Arc::new(build_transport_config());

        let mut server_config = None;

        if let Some(mut config) = config.tls.server {
            config.alpn_protocols = vec![
                web_transport_quinn::ALPN.to_vec(),
                moq_transport::setup::ALPN.to_vec(),
            ];
            config.key_log = Arc::new(rustls::KeyLogFile::new());

            let config: quinn::crypto::rustls::QuicServerConfig = config.try_into()?;
            let mut config = quinn::ServerConfig::with_crypto(Arc::new(config));
            config.transport_config(transport.clone());

            server_config = Some(config);
        }

        // There's a bit more boilerplate to make a generic endpoint.
        let runtime = quinn::default_runtime().context("no async runtime")?;
        let endpoint_config = quinn::EndpointConfig::default();
        let socket = std::net::UdpSocket::bind(config.bind).context("failed to bind UDP socket")?;

        // Create the generic QUIC endpoint.
        let quic = quinn::Endpoint::new(endpoint_config, server_config.clone(), socket, runtime)
            .context("failed to create QUIC endpoint")?;

        let server = server_config.clone().map(|base_server_config| Server {
            quic: quic.clone(),
            accept: Default::default(),
            qlog_dir: config.qlog_dir.map(Arc::new),
            base_server_config: Arc::new(base_server_config),
        });

        let client = Client {
            quic,
            config: config.tls.client,
            transport,
        };

        Ok(Self { client, server })
    }
}

pub struct Server {
    quic: quinn::Endpoint,
    accept: FuturesUnordered<BoxFuture<'static, anyhow::Result<(web_transport::Session, String)>>>,
    qlog_dir: Option<Arc<PathBuf>>,
    base_server_config: Arc<quinn::ServerConfig>,
}

impl Server {
    pub async fn accept(&mut self) -> Option<(web_transport::Session, String)> {
        loop {
            tokio::select! {
                res = self.quic.accept() => {
                    let conn = res?;
                    let qlog_dir = self.qlog_dir.clone();
                    let base_server_config = self.base_server_config.clone();
                    self.accept.push(Self::accept_session(conn, qlog_dir, base_server_config).boxed());
                },
                res = self.accept.next(), if !self.accept.is_empty() => {
                    match res? {
                        Ok(result) => return Some(result),
                        Err(err) => log::warn!("failed to accept QUIC connection: {}", err),
                    }
                }
            }
        }
    }

    async fn accept_session(
        conn: quinn::Incoming,
        qlog_dir: Option<Arc<PathBuf>>,
        base_server_config: Arc<quinn::ServerConfig>,
    ) -> anyhow::Result<(web_transport::Session, String)> {
        // Capture the original destination connection ID BEFORE accepting
        // This is the actual QUIC CID that can be used for qlog/mlog correlation
        let orig_dst_cid = conn.orig_dst_cid();
        let connection_id_hex = orig_dst_cid.to_string();

        // Configure per-connection qlog if enabled
        let mut conn = if let Some(qlog_dir) = qlog_dir {
            // Create qlog file path using connection ID
            let qlog_path = qlog_dir.join(format!("{}_server.qlog", connection_id_hex));

            // Create transport config with our standard settings plus qlog
            let mut transport = build_transport_config();

            let file = File::create(&qlog_path).context("failed to create qlog file")?;
            let writer = BufWriter::new(file);

            let mut qlog = quinn::QlogConfig::default();
            qlog.writer(Box::new(writer))
                .title(Some("moq-relay".into()));
            transport.qlog_stream(qlog.into_stream());

            // Create custom server config with qlog-enabled transport
            let mut server_config = (*base_server_config).clone();
            server_config.transport_config(Arc::new(transport));

            log::debug!(
                "qlog enabled: cid={} path={}",
                connection_id_hex,
                qlog_path.display()
            );

            // Accept with custom config
            conn.accept_with(Arc::new(server_config))?
        } else {
            // No qlog - use default config
            conn.accept()?
        };

        let handshake = conn
            .handshake_data()
            .await?
            .downcast::<quinn::crypto::rustls::HandshakeData>()
            .unwrap();

        let alpn = handshake.protocol.context("missing ALPN")?;
        let alpn = String::from_utf8_lossy(&alpn);
        let server_name = handshake.server_name.unwrap_or_default();

        log::debug!(
            "received QUIC handshake: cid={} ip={} alpn={} server={}",
            connection_id_hex,
            conn.remote_address(),
            alpn,
            server_name,
        );

        // Wait for the QUIC connection to be established.
        let conn = conn.await.context("failed to establish QUIC connection")?;

        log::debug!(
            "established QUIC connection: cid={} stable_id={} ip={} alpn={} server={}",
            connection_id_hex,
            conn.stable_id(),
            conn.remote_address(),
            alpn,
            server_name,
        );

        let session = match alpn.as_bytes() {
            web_transport_quinn::ALPN => {
                // Wait for the CONNECT request.
                let request = web_transport_quinn::accept(conn)
                    .await
                    .context("failed to receive WebTransport request")?;

                // Accept the CONNECT request.
                request
                    .ok()
                    .await
                    .context("failed to respond to WebTransport request")?
            }
            // A bit of a hack to pretend like we're a WebTransport session
            moq_transport::setup::ALPN => conn.into(),
            _ => anyhow::bail!("unsupported ALPN: {}", alpn),
        };

        Ok((session.into(), connection_id_hex))
    }

    pub fn local_addr(&self) -> anyhow::Result<net::SocketAddr> {
        self.quic
            .local_addr()
            .context("failed to get local address")
    }
}

#[derive(Clone)]
pub struct Client {
    quic: quinn::Endpoint,
    config: rustls::ClientConfig,
    transport: Arc<quinn::TransportConfig>,
}

impl Client {
    pub async fn connect(&self, url: &Url) -> anyhow::Result<(web_transport::Session, String)> {
        let mut config = self.config.clone();

        // TODO support connecting to both ALPNs at the same time
        config.alpn_protocols = vec![match url.scheme() {
            "https" => web_transport_quinn::ALPN.to_vec(),
            "moqt" => moq_transport::setup::ALPN.to_vec(),
            _ => anyhow::bail!("url scheme must be 'https' or 'moqt'"),
        }];

        config.key_log = Arc::new(rustls::KeyLogFile::new());

        let config: quinn::crypto::rustls::QuicClientConfig = config.try_into()?;
        let mut config = quinn::ClientConfig::new(Arc::new(config));
        config.transport_config(self.transport.clone());

        // Capture the initial destination CID that will be sent to the server
        // This is the CID used for qlog/mlog correlation on the server side
        let cid_capture: Arc<Mutex<Option<quinn::ConnectionId>>> = Arc::new(Mutex::new(None));
        let cid_capture_clone = cid_capture.clone();
        config.initial_dst_cid_provider(Arc::new(move || {
            // Generate a random CID (Quinn's default behavior)
            use rand::Rng;
            let mut rng = rand::thread_rng();
            let random_bytes: [u8; 16] = rng.gen();
            let cid = quinn::ConnectionId::new(&random_bytes);
            *cid_capture_clone.lock().unwrap() = Some(cid);
            cid
        }));

        let host = url.host().context("invalid DNS name")?.to_string();
        let port = url.port().unwrap_or(443);

        // Look up the DNS entry.
        let addr = tokio::net::lookup_host((host.clone(), port))
            .await
            .context("failed DNS lookup")?
            .next()
            .context("no DNS entries")?;

        let connection = self.quic.connect_with(config, addr, &host)?.await?;

        // Extract the CID that was used
        let connection_id_hex = cid_capture
            .lock()
            .unwrap()
            .as_ref()
            .context("CID not captured")?
            .to_string();

        let session = match url.scheme() {
            "https" => web_transport_quinn::connect_with(connection, url).await?,
            "moqt" => connection.into(),
            _ => unreachable!(),
        };

        Ok((session.into(), connection_id_hex))
    }
}
