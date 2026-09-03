// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use anyhow::Context;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_transport::{
    message::RequestErrorCode,
    serve::{ServeError, Tracks},
    session::{PublishReceived, PublishedNamespace, SessionError, Subscribe, Subscriber},
};
use tokio::sync::Semaphore;

use crate::{
    metrics::GaugeGuard, CacheLease, Coordinator, Locals, Producer, RemoteManager, SessionContext,
    SessionInterface, TrackRequest,
};

const MAX_INBOUND_PUBLISH_TRACKS_PER_SESSION: usize = 1024;

struct CacheSubscriptionCleanup {
    subscribe: Option<Subscribe>,
    lease: Option<CacheLease>,
}

impl CacheSubscriptionCleanup {
    fn new(lease: CacheLease) -> Self {
        Self {
            subscribe: None,
            lease: Some(lease),
        }
    }

    fn attach(&mut self, subscribe: Subscribe) {
        self.subscribe = Some(subscribe);
    }

    fn handles(&self) -> Result<(&Subscribe, &CacheLease), ServeError> {
        match (self.subscribe.as_ref(), self.lease.as_ref()) {
            (Some(subscribe), Some(lease)) => Ok((subscribe, lease)),
            _ => Err(ServeError::Internal(
                "cache subscription cleanup is missing ownership".to_string(),
            )),
        }
    }

    fn finish(mut self) -> Option<tokio::task::JoinHandle<()>> {
        let subscribe = self.subscribe.take();
        let lease = self.lease.take()?;
        lease.begin_release();
        Some(tokio::spawn(async move {
            if let Some(subscribe) = subscribe {
                subscribe.unsubscribe().await;
            }
            lease.finish_release();
        }))
    }
}

impl Drop for CacheSubscriptionCleanup {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        lease.begin_release();
        let subscribe = self.subscribe.take();

        if let Some(subscribe) = subscribe {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                std::mem::drop(runtime.spawn(async move {
                    subscribe.unsubscribe().await;
                    lease.finish_release();
                }));
                return;
            }

            tracing::warn!(
                request_id = subscribe.id,
                "could not wait for cache subscription teardown outside a Tokio runtime"
            );
            drop(subscribe);
        }

        lease.finish_release();
    }
}

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
        }
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

    /// Serve a PUBLISH_NAMESPACE forwarded by a peer relay: advertise it for
    /// discovery, and route nothing through this session.
    ///
    /// The peer that forwarded this is not the origin, it is relaying on the
    /// origin's behalf, so this relay must not present itself as a source. It
    /// registers the namespace the same way the SUBSCRIBE_NAMESPACE pull path
    /// does. The namespace is visible to discovery but absent from local routing,
    /// so a downstream SUBSCRIBE resolves through the coordinator to its owner.
    ///
    /// Deliberately absent, each for its own reason:
    ///
    /// * **No coordinator registration.** Ownership is the origin's. Claiming it
    ///   here races the origin for one key and loses. The losing path then
    ///   unwinds the local registration while the publisher is still live.
    ///
    /// * **No request queue.** With no local route nothing can queue against
    ///   this session, so there is no queue to strand and no need to drain one.
    ///
    /// * **No subscribe reaching this session.** That matters twice over: the
    ///   forwarding peer publishes a placeholder track container whose request
    ///   channel is closed at creation, and that publication takes priority
    ///   over the peer's own unknown-subscribe fall-through. A dialed session
    ///   also has no `Producer` draining
    ///   `subscribed()` at all, so it could not be served even if the
    ///   placeholder were gone.
    ///
    /// * **No onward fan-out.** The coordinator already returns no subscriber
    ///   relays for an internal context; not asking is one fewer way to
    ///   propagate a forwarded announce into a loop.
    async fn serve_proxied(
        self,
        mut published_ns: PublishedNamespace,
    ) -> Result<(), anyhow::Error> {
        let ns = published_ns.namespace.to_utf8_path();

        let _registration = match self
            .locals
            .register_remote_namespace(self.context.scope(), published_ns.namespace.clone())
        {
            Ok(registration) => registration,
            Err(err) => {
                metrics::counter!("moq_relay_announce_errors_total", "phase" => "remote_register")
                    .increment(1);
                return Err(err);
            }
        };
        tracing::debug!(namespace = %ns, "registered proxied namespace for discovery");

        if let Err(err) = published_ns.ok() {
            metrics::counter!("moq_relay_announce_errors_total", "phase" => "send_ok").increment(1);
            return Err(err.into());
        }
        metrics::counter!("moq_relay_announce_ok_total", "kind" => "proxied").increment(1);
        tracing::info!(namespace = %ns, "serving proxied PUBLISH_NAMESPACE from peer relay");

        published_ns.closed().await?;
        tracing::info!(namespace = %ns, "proxied PUBLISH_NAMESPACE closed");

        Ok(())
    }

    /// Serve an inbound PUBLISH_NAMESPACE.
    async fn serve(mut self, mut published_ns: PublishedNamespace) -> Result<(), anyhow::Error> {
        // Track active publishers - decrements when this function returns.
        let _publisher_guard = GaugeGuard::new("moq_relay_active_publishers");

        let mut tasks: FuturesUnordered<
            futures::future::BoxFuture<'static, Result<(), anyhow::Error>>,
        > = FuturesUnordered::new();
        let ns = published_ns.namespace.to_utf8_path();

        // A namespace forwarded by a peer relay is proxied, not ours, so it is
        // advertised for discovery and nothing more.
        if self.context.interface == SessionInterface::Internal {
            return self.serve_proxied(published_ns).await;
        }

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
        metrics::counter!("moq_relay_announce_ok_total", "kind" => "client").increment(1);

        // Forward the namespace upstream, if configured.
        if let Some(mut forward) = self.forward {
            // This advertise-only container cannot route a subscribe back down
            // the dialed session. Supporting that requires the dialed side to
            // run a Producer, which is outside this forwarding path.
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
            // The request half is dropped before this task runs, so this reader
            // can advertise the namespace but cannot serve a subscription. The
            // receiving relay must keep it out of local routing; see
            // `serve_proxied`.
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

                    tasks.push(async move {
                        let mut cleanup = CacheSubscriptionCleanup::new(lease);
                        let info = writer.info.clone();
                        let namespace = info.namespace.to_utf8_path();
                        let track_name = info.name.clone();
                        tracing::info!(
                            namespace = %namespace,
                            track = %track_name,
                            "forwarding subscribe: {:?}", info
                        );

                        // Stop opening the upstream stream when all downstream
                        // interest disappears. Once opening succeeds, cleanup
                        // owns the stream through ordered teardown.
                        let lease = cleanup.lease.as_ref().ok_or_else(|| {
                            ServeError::Internal(
                                "cache subscription cleanup is missing its lease".to_string(),
                            )
                        })?;
                        let subscribe_result = tokio::select! {
                            biased;
                            result = subscriber.subscribe_start(writer) => Some(result),
                            _ = lease.abandoned() => None,
                        };
                        let Some(subscribe_result) = subscribe_result else {
                            tracing::debug!(
                                namespace = %namespace,
                                track = %track_name,
                                "downstream left while opening upstream subscribe"
                            );
                            return Ok(());
                        };
                        let subscribe = match subscribe_result {
                            Ok(subscribe) => {
                                subscribe
                            }
                            Err(err) => {
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
                        };
                        cleanup.attach(subscribe);
                        let (subscribe, lease) = cleanup.handles()?;

                        // Opening wins a tied wake above, so recheck abandonment
                        // before accepting the upstream subscription.
                        let accepted = tokio::select! {
                            result = subscribe.ok() => Some(result),
                            _ = lease.abandoned() => None,
                        };
                        let Some(accepted) = accepted else {
                            tracing::debug!(
                                namespace = %namespace,
                                track = %track_name,
                                "downstream left before upstream accepted subscribe"
                            );
                            if let Some(task) = cleanup.finish() {
                                let _ = task.await;
                            }
                            return Ok(());
                        };
                        if let Err(err) = accepted {
                            tracing::warn!(
                                namespace = %namespace,
                                track = %track_name,
                                error = %err,
                                "failed forwarding subscribe: {:?}", info
                            );
                            upstream.failed(err);
                            if let Some(task) = cleanup.finish() {
                                let _ = task.await;
                            }
                            return Ok(());
                        }
                        upstream.established();
                        let (subscribe, lease) = cleanup.handles()?;

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
                            // evicted, so stop pulling it and complete ordered
                            // request-stream teardown below.
                            _ = lease.released() => {
                                tracing::info!(
                                    namespace = %namespace,
                                    track = %track_name,
                                    "releasing upstream subscription for idle cached track"
                                );
                            }
                        }
                        if let Some(task) = cleanup.finish() {
                            let _ = task.await;
                        }

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
            .register_track_for_session(
                self.context.scope(),
                reader,
                self.context.identity().clone(),
            )
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

#[cfg(test)]
mod tests {
    use moq_transport::serve::FullTrackName;

    use super::CacheSubscriptionCleanup;
    use crate::{Locals, TrackRequest};

    fn ns(path: &str) -> moq_transport::coding::TrackNamespace {
        moq_transport::coding::TrackNamespace::try_from(path).expect("valid namespace")
    }

    #[tokio::test]
    async fn cleanup_before_subscribe_start_removes_the_cache_generation() {
        let namespace = ns("room/123");
        let mut locals = Locals::new();
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("register namespace source");
        let local = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("request missing track");
        let TrackRequest {
            writer,
            lease,
            upstream,
        } = requests.recv().await.expect("receive track request");
        drop(local);
        lease.abandoned().await;

        let full_name = FullTrackName {
            namespace,
            name: "video".into(),
        };
        assert!(locals.retrieve_track(None, &full_name).is_some());

        drop(CacheSubscriptionCleanup::new(lease));

        assert!(locals.retrieve_track(None, &full_name).is_none());
        drop(writer);
        drop(upstream);
    }
}
