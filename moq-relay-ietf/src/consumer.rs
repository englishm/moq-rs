// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_transport::{
    serve::{TrackRequest, Tracks, DEFAULT_CACHE_IDLE_TIMEOUT},
    session::{Announced, SessionError, Subscriber},
};

use crate::{metrics::GaugeGuard, Coordinator, Locals, Producer};

/// Consumer of tracks from a remote Publisher
#[derive(Clone)]
pub struct Consumer {
    subscriber: Subscriber,
    locals: Locals,
    coordinator: Arc<dyn Coordinator>,
    forward: Option<Producer>, // Forward all announcements to this subscriber
    /// The resolved scope identity for this session, if any.
    /// Produced by `Coordinator::resolve_scope()` from the connection path.
    /// Passed to coordinator register/lookup calls to isolate namespaces.
    scope: Option<String>,
    /// How long a pull-through cache entry with no downstream subscribers is
    /// retained before its upstream subscription is released. Zero disables
    /// eviction.
    cache_idle_timeout: Duration,
}

impl Consumer {
    pub fn new(
        subscriber: Subscriber,
        locals: Locals,
        coordinator: Arc<dyn Coordinator>,
        forward: Option<Producer>,
        scope: Option<String>,
    ) -> Self {
        Self {
            subscriber,
            locals,
            coordinator,
            forward,
            scope,
            cache_idle_timeout: DEFAULT_CACHE_IDLE_TIMEOUT,
        }
    }

    /// Set how long an unwatched cached track is retained before its upstream
    /// subscription is released.
    ///
    /// A builder method rather than a `new()` parameter so that existing callers
    /// of [`Consumer::new`] keep compiling.
    pub fn with_cache_idle_timeout(mut self, cache_idle_timeout: Duration) -> Self {
        self.cache_idle_timeout = cache_idle_timeout;
        self
    }

    /// Run the consumer to serve announce requests.
    pub async fn run(mut self) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();

        loop {
            tokio::select! {
                // Handle a new announce request
                Some(announce) = self.subscriber.announced() => {
                    metrics::counter!("moq_relay_publishers_total").increment(1);

                    let this = self.clone();

                    tasks.push(async move {
                        let info = announce.clone();
                        let namespace = info.namespace.to_utf8_path();
                        tracing::info!(namespace = %namespace, "serving announce: {:?}", info);

                        // Serve the announce request
                        if let Err(err) = this.serve(announce).await {
                            tracing::warn!(namespace = %namespace, error = %err, "failed serving announce: {:?}, error: {}", info, err);
                            // Note: phase-specific error counters are incremented in serve()
                        }
                    });
                },
                _ = tasks.next(), if !tasks.is_empty() => {},
                else => return Ok(()),
            };
        }
    }

    /// Serve an announce request.
    async fn serve(mut self, mut announce: Announced) -> Result<(), anyhow::Error> {
        // Track active publishers - decrements when this function returns
        let _publisher_guard = GaugeGuard::new("moq_relay_active_publishers");

        let mut tasks = FuturesUnordered::new();

        // Produce the tracks for this announce and return the reader
        let (_, mut request, reader) = Tracks::new(announce.namespace.clone())
            .produce_with_cache_idle_timeout(self.cache_idle_timeout);

        // should we allow the same namespace being served from multiple relays??
        // Manish: NO.

        let ns = reader.namespace.to_utf8_path();

        // Register the local tracks, unregister on drop
        tracing::debug!(namespace = %ns, "registering namespace in locals");
        let _register = match self
            .locals
            .register(self.scope.as_deref(), reader.clone())
            .await
        {
            Ok(reg) => reg,
            Err(err) => {
                metrics::counter!("moq_relay_announce_errors_total", "phase" => "local_register")
                    .increment(1);
                return Err(err);
            }
        };
        tracing::debug!(namespace = %ns, "namespace registered in locals");

        // NOTE(mpandit): once the track is pulled from origin, internally it will be relayed
        // from this metal only, because now coordinator will have entry for the namespace.

        // Register namespace with the coordinator
        tracing::debug!(namespace = %ns, "registering namespace with coordinator");
        let _namespace_registration = match self
            .coordinator
            .register_namespace(self.scope.as_deref(), &reader.namespace)
            .await
        {
            Ok(reg) => reg,
            Err(err) => {
                metrics::counter!("moq_relay_announce_errors_total", "phase" => "coordinator_register")
                    .increment(1);
                return Err(err.into());
            }
        };
        tracing::debug!(namespace = %ns, "namespace registered with coordinator");

        // Accept the announce with an OK response
        if let Err(err) = announce.ok() {
            metrics::counter!("moq_relay_announce_errors_total", "phase" => "send_ok").increment(1);
            return Err(err.into());
        }
        tracing::debug!(namespace = %ns, "sent ANNOUNCE_OK");

        // Successfully sent ANNOUNCE_OK
        metrics::counter!("moq_relay_announce_ok_total").increment(1);

        // Forward the announce, if needed
        if let Some(mut forward) = self.forward {
            tasks.push(
                async move {
                    let namespace = reader.namespace.to_utf8_path();
                    tracing::info!(namespace = %namespace, "forwarding announce: {:?}", reader.info);
                    forward
                        .announce(reader)
                        .await
                        .context("failed forwarding announce")
                }
                .boxed(),
            );
        }

        // Serve subscribe requests
        loop {
            tokio::select! {
                // If the announce is closed, return the error
                Err(err) = announce.closed() => {
                    let ns = announce.namespace.to_utf8_path();
                    tracing::info!(namespace = %ns, error = %err, "announce closed");
                    return Err(err.into());
                },

                // Wait for the next subscriber and serve the track.
                Some(TrackRequest { writer, lease }) = request.next() => {
                    let mut subscriber = self.subscriber.clone();

                    // Spawn a new task to handle the subscribe
                    tasks.push(async move {
                        let info = writer.info.clone();
                        let namespace = info.namespace.to_utf8_path();
                        let track_name = info.name.clone();
                        tracing::info!(namespace = %namespace, track = %track_name, "forwarding subscribe: {:?}", info);

                        // Hold the subscription explicitly rather than using
                        // `subscribe()`, so it can be dropped — sending
                        // UNSUBSCRIBE — once downstream interest goes away.
                        let subscribe = match subscriber.subscribe_open(writer).await {
                            Ok(subscribe) => subscribe,
                            Err(err) => {
                                tracing::warn!(namespace = %namespace, track = %track_name, error = %err, "failed forwarding subscribe: {:?}, error: {}", info, err);
                                return Ok(());
                            }
                        };

                        tokio::select! {
                            res = subscribe.closed() => {
                                if let Err(err) = res {
                                    tracing::warn!(namespace = %namespace, track = %track_name, error = %err, "failed forwarding subscribe: {:?}, error: {}", info, err)
                                }
                            }
                            // The cached track went unwatched long enough to be
                            // evicted, so stop pulling it. Dropping `subscribe`
                            // below sends UNSUBSCRIBE upstream.
                            _ = lease.released() => {
                                tracing::info!(namespace = %namespace, track = %track_name, "releasing upstream subscription for idle cached track");
                                metrics::counter!("moq_relay_cache_idle_evictions_total", "source" => "local").increment(1);
                            }
                        }

                        drop(subscribe);

                        Ok(())
                    }.boxed());
                },
                res = tasks.next(), if !tasks.is_empty() => res.unwrap()?,
                else => return Ok(()),
            }
        }
    }
}
