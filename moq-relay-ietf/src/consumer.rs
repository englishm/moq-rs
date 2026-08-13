// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_transport::{
    message::RequestErrorCode,
    serve::{ServeError, Tracks},
    session::{PublishReceived, PublishedNamespace, SessionError, Subscriber},
};
use tokio::sync::Semaphore;

use crate::{
    metrics::GaugeGuard, Coordinator, Locals, Producer, RemoteManager, SessionContext,
    TrackRequest, DEFAULT_SUBSCRIBE_TIMEOUT,
};

const MAX_INBOUND_PUBLISH_TRACKS_PER_SESSION: usize = 1024;

/// Consumer of tracks from a remote Publisher
#[derive(Clone)]
pub struct Consumer {
    subscriber: Subscriber,
    locals: Locals,
    coordinator: Arc<dyn Coordinator>,
    /// Relay-to-relay session pool, used to forward PUBLISH_NAMESPACE to peer
    /// relays that the coordinator oracle reports as interested subscribers.
    remotes: RemoteManager,
    forward: Option<Producer>, // Forward all announcements to this subscriber
    /// Relay-level context for this MoQT session.
    context: SessionContext,
    publish_track_permits: Arc<Semaphore>,
    /// How long to wait for the upstream to acknowledge a SUBSCRIBE before
    /// giving up. Never zero.
    subscribe_timeout: Duration,
}

impl Consumer {
    pub fn new(
        subscriber: Subscriber,
        locals: Locals,
        coordinator: Arc<dyn Coordinator>,
        remotes: RemoteManager,
        forward: Option<Producer>,
        context: SessionContext,
    ) -> Self {
        Self {
            subscriber,
            locals,
            coordinator,
            remotes,
            forward,
            context,
            publish_track_permits: Arc::new(Semaphore::new(MAX_INBOUND_PUBLISH_TRACKS_PER_SESSION)),
            subscribe_timeout: DEFAULT_SUBSCRIBE_TIMEOUT,
        }
    }

    /// Set how long to wait for the upstream to acknowledge a SUBSCRIBE.
    ///
    /// A builder method rather than a `new()` parameter so that existing callers
    /// of [`Consumer::new`] keep compiling.
    pub fn with_subscribe_timeout(mut self, subscribe_timeout: Duration) -> Self {
        self.subscribe_timeout = subscribe_timeout;
        self
    }

    /// Run the consumer to handle inbound PUBLISH_NAMESPACE and PUBLISH requests.
    pub async fn run(self) -> Result<(), SessionError> {
        let mut tasks: FuturesUnordered<futures::future::BoxFuture<'static, ()>> =
            FuturesUnordered::new();
        let mut namespace_subscriber = self.subscriber.clone();
        let mut publish_subscriber = self.subscriber.clone();

        loop {
            tokio::select! {
                Some(published_ns) = namespace_subscriber.published_namespace() => {
                    metrics::counter!("moq_relay_publishers_total").increment(1);

                    let this = self.clone();

                    tasks.push(async move {
                        let info = published_ns.clone();
                        let namespace = info.namespace.to_utf8_path();
                        tracing::info!(
                            namespace = %namespace,
                            "serving PUBLISH_NAMESPACE: {:?}", info
                        );

                        if let Err(err) = this.serve(published_ns).await {
                            tracing::warn!(
                                namespace = %namespace,
                                error = %err,
                                "failed serving PUBLISH_NAMESPACE: {:?}", info
                            );
                        }
                    }.boxed());
                },
                Some(publish) = publish_subscriber.publish_received() => {
                    metrics::counter!("moq_relay_published_tracks_total").increment(1);

                    let this = self.clone();
                    tasks.push(async move {
                        let namespace = publish.namespace().to_utf8_path();
                        let track_name = publish.name().clone();
                        tracing::info!(namespace = %namespace, track = %track_name, "serving PUBLISH");

                        if let Err(err) = this.serve_track(publish).await {
                            tracing::warn!(namespace = %namespace, track = %track_name, error = %err, "failed serving PUBLISH");
                        }
                    }.boxed());
                },
                _ = tasks.next(), if !tasks.is_empty() => {},
                else => return Ok(()),
            };
        }
    }

    /// Serve an inbound PUBLISH_NAMESPACE.
    async fn serve(mut self, mut published_ns: PublishedNamespace) -> Result<(), anyhow::Error> {
        // Track active publishers - decrements when this function returns.
        let _publisher_guard = GaugeGuard::new("moq_relay_active_publishers");

        let mut tasks: FuturesUnordered<
            futures::future::BoxFuture<'static, Result<(), anyhow::Error>>,
        > = FuturesUnordered::new();
        let ns = published_ns.namespace.to_utf8_path();

        // Register namespace routing metadata locally. This does not register
        // any media tracks; it only creates a request queue used when a
        // downstream SUBSCRIBE asks for a missing track under this namespace.
        tracing::debug!(namespace = %ns, "registering namespace route source in locals");
        let (_register, mut requests) = match self
            .locals
            .register_namespace(self.context.scope(), published_ns.namespace.clone())
            .await
        {
            Ok(reg) => reg,
            Err(err) => {
                metrics::counter!("moq_relay_announce_errors_total", "phase" => "local_register")
                    .increment(1);
                return Err(err);
            }
        };
        tracing::debug!(namespace = %ns, "namespace route source registered in locals");

        // Register namespace with the coordinator so other relay nodes can route to us.
        tracing::debug!(namespace = %ns, "registering namespace with coordinator");
        let coordinator_context = self.context.coordinator_context();
        let _namespace_registration = match self
            .coordinator
            .register_namespace(
                self.context.scope(),
                &published_ns.namespace,
                &coordinator_context,
            )
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

        // Accept the PUBLISH_NAMESPACE with REQUEST_OK.
        if let Err(err) = published_ns.ok() {
            metrics::counter!("moq_relay_announce_errors_total", "phase" => "send_ok").increment(1);
            return Err(err.into());
        }
        tracing::debug!(namespace = %ns, "sent REQUEST_OK for PUBLISH_NAMESPACE");
        metrics::counter!("moq_relay_announce_ok_total").increment(1);

        // Forward the namespace upstream, if configured.
        if let Some(mut forward) = self.forward {
            // The forwarding API still uses the moq-transport Tracks helper.
            // This helper is not the relay-local registry; it is only an
            // adapter for the outgoing publish_namespace API.
            let (_, _forward_request, forward_reader) =
                Tracks::new(published_ns.namespace.clone()).produce();
            tasks.push(
                async move {
                    let namespace = forward_reader.namespace.to_utf8_path();
                    tracing::info!(
                        namespace = %namespace,
                        "forwarding PUBLISH_NAMESPACE: {:?}", forward_reader.info
                    );
                    // Best-effort upstream propagation: a forwarding failure must not
                    // tear down the local PUBLISH_NAMESPACE registration for other peers.
                    if let Err(err) = forward
                        .publish_namespace(forward_reader)
                        .await
                        .context("failed forwarding PUBLISH_NAMESPACE")
                    {
                        metrics::counter!("moq_relay_announce_errors_total", "phase" => "forward")
                            .increment(1);
                        tracing::warn!(namespace = %namespace, error = %err, "failed forwarding PUBLISH_NAMESPACE upstream");
                    }
                    Ok(())
                }
                .boxed(),
            );
        }

        // Fan out the PUBLISH_NAMESPACE to peer relays that the coordinator
        // oracle reports as having matching SUBSCRIBE_NAMESPACE interest. The
        // coordinator already excludes this relay (the caller) and the inbound
        // source peer, so every returned relay is a distinct downstream peer and
        // we can push to all of them without creating a forwarding loop.
        let subscriber_relays = match self
            .coordinator
            .lookup_namespace_subscribers(
                self.context.scope(),
                &published_ns.namespace,
                &coordinator_context,
            )
            .await
        {
            Ok(relays) => relays,
            Err(err) => {
                metrics::counter!("moq_relay_announce_errors_total", "phase" => "coordinator_lookup")
                    .increment(1);
                return Err(err.into());
            }
        };

        for relay in subscriber_relays {
            let remotes = self.remotes.clone();
            // The forwarding API uses the moq-transport Tracks helper as an
            // adapter for the outgoing publish_namespace call; it is not the
            // relay-local registry.
            let (_, _request, reader) = Tracks::new(published_ns.namespace.clone()).produce();
            tasks.push(
                async move {
                    let namespace = reader.namespace.to_utf8_path();
                    tracing::info!(
                        namespace = %namespace,
                        remote = %relay.url,
                        "forwarding PUBLISH_NAMESPACE to peer relay"
                    );
                    // Best-effort fan-out: one downstream relay failing must not tear
                    // down the PUBLISH_NAMESPACE registration for the publisher or the
                    // other peer relays.
                    if let Err(err) = remotes
                        .publish_namespace(&relay, reader)
                        .await
                        .with_context(|| {
                            format!("failed forwarding PUBLISH_NAMESPACE to {}", relay.url)
                        })
                    {
                        metrics::counter!("moq_relay_announce_errors_total", "phase" => "peer_fanout")
                            .increment(1);
                        tracing::warn!(namespace = %namespace, remote = %relay.url, error = %err, "failed forwarding PUBLISH_NAMESPACE to peer relay");
                    }
                    Ok(())
                }
                .boxed(),
            );
        }

        loop {
            tokio::select! {
                res = published_ns.closed() => {
                    let ns = published_ns.namespace.to_utf8_path();
                    res?;
                    tracing::info!(namespace = %ns, "PUBLISH_NAMESPACE closed");
                    return Ok(());
                },
                Some(TrackRequest { writer, lease, upstream }) = requests.recv() => {
                    let mut subscriber = self.subscriber.clone();
                    let subscribe_timeout = self.subscribe_timeout;

                    tasks.push(async move {
                        let info = writer.info.clone();
                        let namespace = info.namespace.to_utf8_path();
                        let track_name = info.name.clone();
                        tracing::info!(
                            namespace = %namespace,
                            track = %track_name,
                            "forwarding subscribe: {:?}", info
                        );

                        // Hold the subscription explicitly rather than using
                        // `subscribe()`, so it can be dropped — sending
                        // UNSUBSCRIBE — once downstream interest goes away.
                        //
                        // `subscribe_open` resolves once the upstream publisher
                        // answers, so its outcome is exactly what downstream
                        // subscribers are waiting on to satisfy draft-16 §8.4.
                        // An upstream is under no obligation to ever answer, so
                        // the handshake is bounded two ways. Left unbounded it
                        // would pin this task, the `TrackWriter` it carries, and
                        // every downstream subscriber waiting on `upstream`, until
                        // the session died.
                        //
                        // - `lease.released()` covers "nobody downstream is waiting
                        //   for this track any more", which is the condition that
                        //   actually matters and needs no arbitrary constant.
                        // - `subscribe_timeout` covers the rest: a subscriber that
                        //   is still waiting cannot wait forever on a peer that
                        //   never answers.
                        //
                        // Either way the in-flight future is dropped, which drops
                        // the `Subscribe`, whose `Drop` sends UNSUBSCRIBE — so
                        // giving up mid-handshake leaves nothing dangling upstream.
                        let subscribe = tokio::select! {
                            result = tokio::time::timeout(subscribe_timeout, subscriber.subscribe_open(writer)) => match result {
                                Ok(Ok(subscribe)) => {
                                    upstream.established();
                                    subscribe
                                }
                                Ok(Err(err)) => {
                                    tracing::warn!(
                                        namespace = %namespace,
                                        track = %track_name,
                                        error = %err,
                                        "failed forwarding subscribe: {:?}", info
                                    );
                                    // Release waiting downstream subscribers with the
                                    // upstream reason so they get a REQUEST_ERROR that
                                    // matches it, rather than a premature accept.
                                    upstream.failed(err);
                                    return Ok(());
                                }
                                Err(_elapsed) => {
                                    tracing::warn!(
                                        namespace = %namespace,
                                        track = %track_name,
                                        timeout = ?subscribe_timeout,
                                        "upstream did not acknowledge SUBSCRIBE in time: {:?}", info
                                    );
                                    metrics::counter!("moq_relay_subscribe_timeouts_total", "source" => "local").increment(1);
                                    upstream.failed(ServeError::Closed(RequestErrorCode::Timeout as u64));
                                    return Ok(());
                                }
                            },
                            // Counted by the eviction itself in `Locals`. Dropping
                            // `upstream` unresolved releases any straggler
                            // downstream subscriber with an error.
                            _ = lease.released() => {
                                tracing::info!(
                                    namespace = %namespace,
                                    track = %track_name,
                                    "abandoning upstream subscribe for idle cached track"
                                );
                                return Ok(());
                            }
                        };

                        tokio::select! {
                            res = subscribe.closed() => {
                                if let Err(err) = res {
                                    tracing::warn!(
                                        namespace = %namespace,
                                        track = %track_name,
                                        error = %err,
                                        "failed forwarding subscribe: {:?}", info
                                    )
                                }
                            }
                            // The cached track went unwatched long enough to be
                            // evicted, so stop pulling it. Dropping `subscribe`
                            // below sends UNSUBSCRIBE upstream.
                            _ = lease.released() => {
                                tracing::info!(
                                    namespace = %namespace,
                                    track = %track_name,
                                    "releasing upstream subscription for idle cached track"
                                );
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

    /// Serve an inbound PUBLISH for one exact track.
    async fn serve_track(mut self, mut publish: PublishReceived) -> Result<(), anyhow::Error> {
        let _publish_permit = match self.publish_track_permits.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                metrics::counter!("moq_relay_publish_errors_total", "phase" => "session_limit")
                    .increment(1);
                publish.close(ServeError::Closed(RequestErrorCode::InternalError as u64));
                return Err(ServeError::Cancel.into());
            }
        };

        let namespace = publish.namespace().clone();
        let track_name = publish.name().clone();

        // Take the reader first, then follow the same order as PUBLISH_NAMESPACE:
        // local registration → coordinator registration → PUBLISH_OK.
        let reader = match publish.take_reader() {
            Ok(reader) => reader,
            Err(err) => {
                metrics::counter!("moq_relay_publish_errors_total", "phase" => "take_reader")
                    .increment(1);
                return Err(err.into());
            }
        };

        let _registration = match self
            .locals
            .register_track(self.context.scope(), reader)
            .await
        {
            Ok(registration) => registration,
            Err(err) => {
                metrics::counter!("moq_relay_publish_errors_total", "phase" => "local_register")
                    .increment(1);
                if err
                    .downcast_ref::<ServeError>()
                    .is_some_and(|err| matches!(err, ServeError::Duplicate))
                {
                    publish.close(ServeError::Duplicate);
                    return Err(ServeError::Duplicate.into());
                }
                return Err(err);
            }
        };

        let track_string = track_name.to_string();
        let _track_registration = match self
            .coordinator
            .register_track(self.context.scope(), &namespace, &track_string)
            .await
        {
            Ok(registration) => registration,
            Err(err) => {
                metrics::counter!("moq_relay_publish_errors_total", "phase" => "coordinator_register")
                    .increment(1);
                publish.close(ServeError::Closed(RequestErrorCode::InternalError as u64));
                return Err(err.into());
            }
        };

        if let Err(err) = publish.accept(true) {
            metrics::counter!("moq_relay_publish_errors_total", "phase" => "send_ok").increment(1);
            return Err(err.into());
        }

        tracing::debug!(
            namespace = %namespace,
            track = %track_name,
            "PUBLISH registered as exact local track"
        );

        publish.closed().await?;
        tracing::info!(namespace = %namespace, track = %track_name, "PUBLISH closed");

        Ok(())
    }
}
