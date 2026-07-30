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

use crate::{metrics::GaugeGuard, Coordinator, Locals, Producer, DEFAULT_SUBSCRIBE_TIMEOUT};

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
    /// How long to wait for the upstream to acknowledge a SUBSCRIBE before
    /// giving up. Never zero.
    subscribe_timeout: Duration,
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
            subscribe_timeout: DEFAULT_SUBSCRIBE_TIMEOUT,
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

    /// Set how long to wait for the upstream to acknowledge a SUBSCRIBE.
    ///
    /// A builder method rather than a `new()` parameter so that existing callers
    /// of [`Consumer::new`] keep compiling.
    pub fn with_subscribe_timeout(mut self, subscribe_timeout: Duration) -> Self {
        self.subscribe_timeout = subscribe_timeout;
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
                        tracing::info!(namespace = %info.namespace, "serving announce: {:?}", info);

                        // Serve the announce request
                        if let Err(err) = this.serve(announce).await {
                            tracing::warn!(namespace = %info.namespace, error = %err, "failed serving announce: {:?}, error: {}", info, err);
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

        // Register the local tracks, unregister on drop
        tracing::debug!(namespace = %reader.namespace, "registering namespace in locals");
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
        tracing::debug!(namespace = %reader.namespace, "namespace registered in locals");

        // NOTE(mpandit): once the track is pulled from origin, internally it will be relayed
        // from this metal only, because now coordinator will have entry for the namespace.

        // Register namespace with the coordinator
        tracing::debug!(namespace = %reader.namespace, "registering namespace with coordinator");
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
        tracing::debug!(namespace = %reader.namespace, "namespace registered with coordinator");

        // Accept the announce with an OK response
        if let Err(err) = announce.ok() {
            metrics::counter!("moq_relay_announce_errors_total", "phase" => "send_ok").increment(1);
            return Err(err.into());
        }
        tracing::debug!(namespace = %reader.namespace, "sent ANNOUNCE_OK");

        // Successfully sent ANNOUNCE_OK
        metrics::counter!("moq_relay_announce_ok_total").increment(1);

        // Forward the announce, if needed
        if let Some(mut forward) = self.forward {
            tasks.push(
                async move {
                    tracing::info!(namespace = %reader.namespace, "forwarding announce: {:?}", reader.info);
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
                    tracing::info!(namespace = %announce.namespace, error = %err, "announce closed");
                    return Err(err.into());
                },

                // Wait for the next subscriber and serve the track.
                Some(TrackRequest { writer, lease }) = request.next() => {
                    let mut subscriber = self.subscriber.clone();
                    let subscribe_timeout = self.subscribe_timeout;

                    // Spawn a new task to handle the subscribe
                    tasks.push(async move {
                        let info = writer.info.clone();
                        let track_name = info.name.clone();
                        tracing::info!(namespace = %info.namespace, track = %track_name, "forwarding subscribe: {:?}", info);

                        // Hold the subscription explicitly rather than using
                        // `subscribe()`, so it can be dropped — sending
                        // UNSUBSCRIBE — once downstream interest goes away.
                        //
                        // The handshake is bounded two ways, because `subscribe_open`
                        // waits for SUBSCRIBE_OK and an upstream is under no
                        // obligation to ever send it. Left unbounded it would pin this
                        // task, and the `TrackWriter` it carries, until the session
                        // died — the resource leak this path exists to avoid.
                        //
                        // - `lease.released()` covers "nobody downstream is waiting
                        //   for this track any more", which is the condition that
                        //   actually matters and needs no arbitrary constant.
                        // - `subscribe_timeout` covers the rest: a subscriber that is
                        //   still waiting cannot wait forever on a peer that never
                        //   answers.
                        //
                        // Either way the in-flight future is dropped, which drops the
                        // `Subscribe`, whose `Drop` sends UNSUBSCRIBE — so giving up
                        // mid-handshake leaves nothing dangling upstream.
                        let subscribe = tokio::select! {
                            result = tokio::time::timeout(subscribe_timeout, subscriber.subscribe_open(writer)) => match result {
                                Ok(Ok(subscribe)) => subscribe,
                                Ok(Err(err)) => {
                                    tracing::warn!(namespace = %info.namespace, track = %track_name, error = %err, "failed forwarding subscribe: {:?}, error: {}", info, err);
                                    return Ok(());
                                }
                                Err(_elapsed) => {
                                    tracing::warn!(namespace = %info.namespace, track = %track_name, timeout = ?subscribe_timeout, "upstream did not acknowledge SUBSCRIBE in time: {:?}", info);
                                    metrics::counter!("moq_relay_subscribe_timeouts_total", "source" => "local").increment(1);
                                    return Ok(());
                                }
                            },
                            _ = lease.released() => {
                                tracing::info!(namespace = %info.namespace, track = %track_name, "abandoning upstream subscribe for idle cached track");
                                metrics::counter!("moq_relay_cache_idle_evictions_total", "source" => "local").increment(1);
                                return Ok(());
                            }
                        };

                        tokio::select! {
                            res = subscribe.closed() => {
                                if let Err(err) = res {
                                    tracing::warn!(namespace = %info.namespace, track = %track_name, error = %err, "failed forwarding subscribe: {:?}, error: {}", info, err)
                                }
                            }
                            // The cached track went unwatched long enough to be
                            // evicted, so stop pulling it. Dropping `subscribe`
                            // below sends UNSUBSCRIBE upstream.
                            _ = lease.released() => {
                                tracing::info!(namespace = %info.namespace, track = %track_name, "releasing upstream subscription for idle cached track");
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
