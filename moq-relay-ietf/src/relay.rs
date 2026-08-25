// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{future::Future, net, path::PathBuf, pin::Pin, sync::Arc, time::Duration};

use anyhow::Context;

use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_native_ietf::quic::{self, Endpoint};
use moq_transport::session::SessionConfig;
use url::Url;

use crate::upstream_namespaces::{UpstreamNamespaces, UpstreamNamespacesRunner};
use crate::{
    metrics::GaugeGuard, ConnectionMeta, ConnectionTagger, Consumer, Coordinator, Locals, Producer,
    RelayInfo, RemoteManager, Session, SessionContext, DEFAULT_CACHE_IDLE_TIMEOUT,
    DEFAULT_MAX_REMOTE_CONNECTIONS,
};

// A type alias for boxed future
type ServerFuture = Pin<
    Box<
        dyn Future<
            Output = (
                anyhow::Result<(web_transport::Session, quic::ConnInfo)>,
                quic::Server,
            ),
        >,
    >,
>;

/// Configuration for the relay.
pub struct RelayConfig {
    /// Listen on this address
    pub bind: Option<net::SocketAddr>,

    /// Optional list of endpoints if provided, we won't use bind
    pub endpoints: Vec<Endpoint>,

    /// The TLS configuration.
    pub tls: moq_native_ietf::tls::Config,

    /// Directory to write qlog files (one per connection)
    pub qlog_dir: Option<PathBuf>,

    /// Directory to write mlog files (one per connection)
    pub mlog_dir: Option<PathBuf>,

    /// Forward all PUBLISH_NAMESPACE messages to the (optional) upstream URL.
    pub announce: Option<Url>,

    /// Our hostname which we advertise to other origins.
    /// We use QUIC, so the certificate must be valid for this address.
    pub node: Option<Url>,

    /// The coordinator for namespace/track registration and discovery.
    pub coordinator: Arc<dyn Coordinator>,

    /// MoQT session configuration used for inbound and relay-to-relay sessions.
    pub session: SessionConfig,

    /// Classifies inbound connections as public clients or internal relay
    /// peers via connection tags (the well-known `interface` tag). Consulted
    /// once per accepted connection with the peer's socket address and
    /// connection path.
    ///
    /// When `None`, every inbound connection is treated as a public client.
    /// Outbound connections the relay dials itself (`--announce`,
    /// [`RemoteManager`]) are always tagged internal and bypass this.
    pub connection_tagger: Option<Arc<dyn ConnectionTagger>>,
}

impl RelayConfig {
    /// Build a relay from this configuration.
    pub fn build(self) -> anyhow::Result<Relay> {
        Relay::new(self)
    }

    /// Build a relay with a custom pull-through cache idle timeout.
    ///
    /// See [`Relay::new_with_cache_idle_timeout`]. This is a separate
    /// constructor rather than a `RelayConfig` field so that adding it does not
    /// break existing struct-literal construction of [`RelayConfig`].
    pub fn build_with_cache_idle_timeout(
        self,
        cache_idle_timeout: Duration,
    ) -> anyhow::Result<Relay> {
        Relay::new_with_cache_idle_timeout(self, cache_idle_timeout)
    }
}

/// MoQ Relay server.
pub struct Relay {
    config: RelayConfig,
    locals: Locals,
    remotes: RemoteManager,
    upstream_namespaces: UpstreamNamespaces,
    upstream_namespaces_runner: UpstreamNamespacesRunner,
}

impl Relay {
    pub fn new(config: RelayConfig) -> anyhow::Result<Self> {
        Self::new_with_cache_idle_timeout(config, DEFAULT_CACHE_IDLE_TIMEOUT)
    }

    /// Create a relay that releases upstream subscriptions for cached tracks that
    /// have had no downstream subscribers for `cache_idle_timeout`.
    ///
    /// The relay caches tracks it does not publish itself so multiple downstream
    /// subscribers can share one upstream subscription. Without a timeout that
    /// subscription outlives the last subscriber and the relay keeps receiving a
    /// track nobody is watching. A zero timeout restores that behaviour.
    pub fn new_with_cache_idle_timeout(
        config: RelayConfig,
        cache_idle_timeout: Duration,
    ) -> anyhow::Result<Self> {
        Self::new_with_cache_limits(config, cache_idle_timeout, DEFAULT_MAX_REMOTE_CONNECTIONS)
    }

    /// Create a relay with both cache limits set.
    ///
    /// `cache_idle_timeout` is described on [`Relay::new_with_cache_idle_timeout`].
    /// `max_remote_connections` bounds how many peer relay connections are
    /// pooled: see [`RemoteManager::with_max_connections`], which also documents
    /// that connections still serving a subscription are never evicted.
    pub fn new_with_cache_limits(
        mut config: RelayConfig,
        cache_idle_timeout: Duration,
        max_remote_connections: usize,
    ) -> anyhow::Result<Self> {
        if config.bind.is_some() && !config.endpoints.is_empty() {
            anyhow::bail!("cannot specify both bind and endpoints");
        }

        if let Some(bind) = config.bind.take() {
            let endpoint = quic::Endpoint::new(quic::Config::new(
                bind,
                config.qlog_dir.clone(),
                config.tls.clone(),
            )?)?;
            config.endpoints = vec![endpoint];
        }

        if config.endpoints.is_empty() {
            anyhow::bail!("no endpoints available to start the server");
        }

        // Validate mlog directory if provided
        if let Some(mlog_dir) = &config.mlog_dir {
            if !mlog_dir.exists() {
                anyhow::bail!("mlog directory does not exist: {}", mlog_dir.display());
            }
            if !mlog_dir.is_dir() {
                anyhow::bail!("mlog path is not a directory: {}", mlog_dir.display());
            }
            tracing::info!("mlog output enabled: {}", mlog_dir.display());
        }

        let locals = Locals::with_cache_idle_timeout(cache_idle_timeout);

        // FIXME(itzmanish): have a generic filter to find endpoints for forward, remote etc.
        let remote_clients = config
            .endpoints
            .iter()
            .map(|endpoint| endpoint.client.clone())
            .collect::<Vec<_>>();

        // Create remote manager - uses coordinator for namespace lookups
        let remotes = RemoteManager::new_with_session_config(
            config.coordinator.clone(),
            remote_clients,
            config.session,
        )
        .with_cache_idle_timeout(cache_idle_timeout)
        .with_max_connections(max_remote_connections);
        let (upstream_namespaces, upstream_namespaces_runner) =
            UpstreamNamespaces::new(locals.clone(), remotes.clone(), config.coordinator.clone());

        Ok(Self {
            config,
            locals,
            remotes,
            upstream_namespaces,
            upstream_namespaces_runner,
        })
    }

    /// Run the relay server.
    pub async fn run(self) -> anyhow::Result<()> {
        let Self {
            config,
            locals,
            remotes,
            upstream_namespaces,
            upstream_namespaces_runner,
        } = self;

        let RelayConfig {
            endpoints: quic_endpoints,
            announce: announce_url,
            mlog_dir,
            coordinator,
            session: session_config,
            connection_tagger,
            ..
        } = config;

        let run_result = async {
            let mut tasks = FuturesUnordered::new();
            tasks.push(
                async move {
                    upstream_namespaces_runner.run().await;
                    Ok::<(), anyhow::Error>(())
                }
                .boxed(),
            );

            // Use the remote manager for routing to remote relays.
            let remote_manager = remotes.clone();

            // Start the forwarder, if any
            let forward_producer = if let Some(url) = &announce_url {
                tracing::info!("forwarding PUBLISH_NAMESPACE messages to {}", url);

                // Establish a QUIC connection to the forward URL
                let (session, _quic_client_initial_cid, transport) = quic_endpoints[0]
                    .client
                    .connect(url, None)
                    .await
                    .context("failed to establish forward connection")?;

                // Create the MoQ session over the connection
                let (session, publisher, subscriber) = moq_transport::session::Session::connect_with_config(
                    session,
                    None,
                    transport,
                    session_config,
                )
                .await
                .context("failed to establish forward session")?;

                // Use the connection path already validated and stored by Session::connect().
                // The forward session is scoped to whatever path the announce URL specifies.
                //
                // Note: the forward connection intentionally does not call
                // coordinator.resolve_scope(). The announce URL is operator-configured
                // (via --announce), not client-supplied, so it doesn't need the same
                // auth/permission checks that incoming client connections get. The
                // forward session always gets both Producer and Consumer (full
                // read-write) since it's acting as a relay peer, not a client.
                //
                // Limitation: all incoming scopes are forwarded to this single upstream scope.
                // Multi-scope forwarding (routing different incoming scopes to different
                // upstream paths) would require per-scope forward connections.
                let forward_scope = session.connection_path().map(|s| s.to_string());
                let forward_context = SessionContext::internal(
                    forward_scope,
                    Some(RelayInfo::new(url.clone())),
                );

                let forward_coordinator = coordinator.clone();
                let session = Session {
                    session,
                    producer: Some(Producer::new_with_upstream_namespaces(
                        publisher,
                        locals.clone(),
                        remote_manager.clone(),
                        upstream_namespaces.clone(),
                        forward_context.clone(),
                    )),
                    consumer: Some(Consumer::new(
                        subscriber,
                        locals.clone(),
                        forward_coordinator,
                        remote_manager.clone(),
                        None,
                        forward_context,
                    )),
                    // Forward connections are always full read-write relay peers,
                    // so no reject loops needed.
                    reject_publishes: None,
                    reject_subscribes: None,
                };

                let forward_producer = session.producer.clone();

                tasks.push(async move { session.run().await.context("forwarding failed") }.boxed());

                forward_producer
            } else {
                None
            };

            let servers: Vec<quic::Server> = quic_endpoints
                .into_iter()
                .map(|endpoint| endpoint.server.context("missing TLS certificate for server"))
                .collect::<anyhow::Result<_>>()?;

            // This will hold the futures for all our listening servers.
            let mut accepts: FuturesUnordered<ServerFuture> = FuturesUnordered::new();
            for mut server in servers {
                tracing::info!("listening on {}", server.local_addr()?);

                // Create a future, box it, and push it to the collection.
                accepts.push(
                    async move {
                        let conn = server.accept().await.context("accept failed");
                        (conn, server)
                    }
                    .boxed(),
                );
            }

            loop {
                tokio::select! {
                    // This branch polls all the `accept` futures concurrently.
                    Some((conn_result, mut server)) = accepts.next() => {
                        // An accept operation has completed.
                        // First, immediately queue up the next accept() call for this server.
                        accepts.push(
                            async move {
                                let conn = server.accept().await.context("accept failed");
                                (conn, server)
                            }
                            .boxed(),
                        );

                        let (conn, info) = conn_result.context("failed to accept QUIC connection")?;
                        let quic::ConnInfo {
                            id: connection_id,
                            transport,
                            remote_address: remote_addr,
                            // The local IP the connection was accepted on
                            // (destination IP the peer targeted); forwarded to the
                            // connection tagger for inbound-interface classification.
                            local_ip,
                            server_name,
                        } = info;

                        metrics::counter!("moq_relay_connections_total").increment(1);

                        // Construct mlog path from connection ID if mlog directory is configured
                        let mlog_path = mlog_dir.as_ref()
                            .map(|dir| dir.join(format!("{}_server.mlog", connection_id)));

                        let locals = locals.clone();
                        let remotes = remote_manager.clone();
                        let forward = forward_producer.clone();
                        let coordinator = coordinator.clone();
                        let upstream_namespaces = upstream_namespaces.clone();
                        let connection_tagger = connection_tagger.clone();

                        // Spawn a new task to handle the connection
                        tasks.push(async move {
                            // Track active connections - decrements when task completes
                            let _conn_guard = GaugeGuard::new("moq_relay_active_connections");

                            // Clone the raw connection so we can close it with a proper
                            // error code if scope resolution fails after the MoQ handshake.
                            let raw_conn = conn.clone();

                            // Create the MoQ session over the connection (setup handshake etc)
                            let (session, publisher, subscriber) = match moq_transport::session::Session::accept_with_config(conn, mlog_path, transport, session_config).await {
                                Ok(session) => session,
                                Err(err) => {
                                    tracing::warn!(error = %err, "failed to accept MoQ session: {}", err);
                                    metrics::counter!("moq_relay_connection_errors_total", "stage" => "session_accept").increment(1);
                                    // Maintain invariant: connections_total - connections_closed_total == active_connections
                                    metrics::counter!("moq_relay_connections_closed_total").increment(1);
                                    return Ok(());
                                }
                            };

                            // Create our MoQ relay session
                            let moq_session = session;

                            // Resolve the connection path to a scope (identity + permissions).
                            // This translates the raw transport-level path into an application-level
                            // scope_id and determines what the connection is allowed to do.
                            let scope_info = match coordinator.resolve_scope(moq_session.connection_path()).await {
                                Ok(info) => info,
                                Err(err) => {
                                    tracing::warn!(
                                        connection_path = moq_session.connection_path(),
                                        error = %err,
                                        "scope resolution failed, rejecting session"
                                    );
                                    // Close with PROTOCOL_VIOLATION (0x3) so the client
                                    // gets a meaningful error instead of an abrupt reset.
                                    // This is a QUIC APPLICATION_CLOSE, not a MoQT SESSION_CLOSE
                                    // control message. Sending a proper SESSION_CLOSE would require
                                    // running the MoQ session's send loop, which is not warranted
                                    // for a pre-session rejection. The QUIC close code and reason
                                    // string are visible to the client's transport layer.
                                    raw_conn.close(0x3, "scope resolution failed");
                                    metrics::counter!("moq_relay_connection_errors_total", "stage" => "scope_resolve").increment(1);
                                    metrics::counter!("moq_relay_connections_closed_total").increment(1);
                                    return Ok(());
                                }
                            };

                            let can_publish = scope_info.as_ref().is_none_or(|s| s.permissions.can_publish());
                            let can_subscribe = scope_info.as_ref().is_none_or(|s| s.permissions.can_subscribe());

                            // Classify the connection interface (public client vs internal
                            // relay peer). This is deliberately separate from scope
                            // resolution above: resolve_scope() returns identity +
                            // permissions, while the embedder-supplied tagger decides the
                            // transport interface from the peer socket address, TLS SNI, and
                            // connection path. With no tagger configured every inbound
                            // connection is treated as a public client. For connections
                            // classified internal, the peer relay identity is derived from
                            // the inbound socket address (see RelayInfo::from_socket_addr).
                            let scope = scope_info.as_ref().map(|info| info.scope_id.clone());
                            let context = match connection_tagger.as_ref() {
                                Some(tagger) => {
                                    let meta = ConnectionMeta::new(
                                        Some(remote_addr),
                                        server_name,
                                        moq_session.connection_path().map(str::to_string),
                                    )
                                    .with_local_ip(local_ip);
                                    let tags = tagger.tag(&meta);
                                    SessionContext::from_tags(scope, &tags, Some(remote_addr))
                                }
                                None => SessionContext::public(scope),
                            };

                            if let Some(ref info) = scope_info {
                                tracing::debug!(
                                    connection_path = moq_session.connection_path(),
                                    scope_id = %info.scope_id,
                                    permissions = ?info.permissions,
                                    "scope resolved"
                                );
                            }

                            // Gate Producer/Consumer creation on permissions.
                            // Note the intentional inversion:
                            // - Producer serves SUBSCRIBEs → gated on can_subscribe
                            // - Consumer handles PUBLISH_NAMESPACEs → gated on can_publish
                            //
                            // When a half is disabled, we pass its transport counterpart
                            // to the Session's reject fields so unauthorized messages get
                            // an explicit error response instead of being silently ignored.
                            let (producer, reject_subscribes) = if can_subscribe {
                                (publisher.map(|publisher| Producer::new_with_upstream_namespaces(publisher, locals.clone(), remotes.clone(), upstream_namespaces, context.clone())), None)
                            } else {
                                (None, publisher)
                            };

                            let (consumer, reject_publishes) = if can_publish {
                                (subscriber.map(|subscriber| Consumer::new(subscriber, locals, coordinator, remotes.clone(), forward, context)), None)
                            } else {
                                (None, subscriber)
                            };

                            let session = Session {
                                session: moq_session,
                                producer,
                                consumer,
                                reject_publishes,
                                reject_subscribes,
                            };

                            match session.run().await {
                                Ok(()) => {
                                    // Session ended cleanly (uncommon - usually ends via close)
                                    metrics::counter!("moq_relay_connections_closed_total").increment(1);
                                }
                                Err(err) if err.is_graceful_close() => {
                                    // Graceful close - peer sent APPLICATION_CLOSE with code 0
                                    tracing::debug!("MoQ session closed gracefully");
                                    metrics::counter!("moq_relay_connections_closed_total").increment(1);
                                }
                                Err(err) => {
                                    // Actual error - protocol violation, timeout, etc.
                                    tracing::warn!(error = %err, "MoQ session error: {}", err);
                                    metrics::counter!("moq_relay_connection_errors_total", "stage" => "session_run").increment(1);
                                    metrics::counter!("moq_relay_connections_closed_total").increment(1);
                                }
                            }

                            Ok(())
                        }.boxed());
                    },
                    res = tasks.next(), if !tasks.is_empty() => res.unwrap()?,
                }
            }
        }
        .await;

        remotes.shutdown().await;
        run_result
    }
}
