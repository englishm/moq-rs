// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_transport::{
    coding::{KeyValuePairs, TrackName, TrackNamespace},
    message::{RequestErrorCode, SubscribeOptions},
    serve::{FullTrackName, ServeError, TrackReader, TracksReader},
    session::{
        Publisher, ServeWithDeadlineError, SessionError, Subscribed, SubscribedNamespace,
        TrackStatusRequested,
    },
};
use tokio::sync::{broadcast, OwnedSemaphorePermit, Semaphore};

use crate::{
    local::MAX_CONCURRENT_RENDEZVOUS_HOLDS,
    metrics::{GaugeGuard, TimingGuard},
    remote::RemoteSubscribeError,
    upstream_namespaces::UpstreamNamespaces,
    Coordinator, Locals, NamespaceChange, RemoteManager, SessionContext, TrackChange,
    UpstreamReady,
};

/// Ceiling on how long a SUBSCRIBE is held waiting for a publisher to appear.
///
/// draft-18 §10.2.6 lets a relay use a shorter window than the subscriber asked
/// for, without telling it. Each held subscribe pins a bidi stream slot and its
/// relay-side state for the whole window. [`Locals`] separately caps concurrent
/// holds across all sessions on the relay.
///
/// Hardcoded for now. The eventual source is per-scope configuration, which is
/// the same knob the pre-spec "lingering subscribe" config carried before it was
/// removed; 30s was its default there too.
const MAX_RENDEZVOUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Keep one session from consuming the relay's entire rendezvous budget. Half
/// the relay-wide capacity remains available to other sessions.
const MAX_CONCURRENT_RENDEZVOUS_HOLDS_PER_SESSION: usize = MAX_CONCURRENT_RENDEZVOUS_HOLDS / 2;

/// How often a held SUBSCRIBE asks the coordinator again whether a publisher has
/// turned up.
///
/// The broadcasts that wake a hold only carry publishers on this relay, but
/// several relay instances serve one scope, so the publisher a subscriber is
/// waiting for routinely lands on a different one. Its namespace goes into
/// shared coordinator state and nothing tells this relay to look again, so
/// without a second trigger the hold expires while the routing information sits
/// there. Asking on an interval limits timer-driven rechecks to fifteen at the
/// 30s ceiling. Publisher events and lagged broadcast receivers can trigger
/// additional lookups.
///
/// Coordinator lookups can require network calls, so a tight interval would let
/// a handful of holds consume routing capacity needed by other subscriptions.
///
/// The two costs of choosing it this way: a cross-relay publisher goes unnoticed
/// for up to one interval after it arrives, and a window shorter than the
/// interval expires without asking again, so those stay same-relay only.
const RENDEZVOUS_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// How long to hold a SUBSCRIBE for, given what the subscriber asked for.
///
/// `None` means do not hold: answer immediately with DOES_NOT_EXIST, which is
/// what draft-18 requires for both an absent parameter and an explicit 0.
fn rendezvous_hold(
    requested_ms: Option<u64>,
    max: std::time::Duration,
) -> Option<std::time::Duration> {
    match requested_ms {
        None | Some(0) => None,
        Some(ms) => Some(std::time::Duration::from_millis(ms).min(max)),
    }
}

/// Whether a newly announced namespace could serve a track in `target`.
///
/// `Locals` routes a track to the longest registered namespace that prefixes it,
/// so an announcement is relevant when it prefixes the track's namespace, not
/// only when it matches exactly.
fn namespace_covers(announced: &TrackNamespace, target: &TrackNamespace) -> bool {
    announced.fields.len() <= target.fields.len()
        && announced
            .fields
            .iter()
            .zip(target.fields.iter())
            .all(|(a, b)| a == b)
}

/// How a rendezvous hold ended.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Rendezvous {
    /// Something that could publish this track arrived. The caller repeats the
    /// lookup rather than assuming the track is now servable.
    PublisherAppeared,
    /// No local event, but the re-check interval came round, so the caller looks
    /// again in case a publisher appeared on another relay.
    RecheckDue,
    /// The window closed before routing produced a usable publisher. The caller
    /// distinguishes a publisher timeout from a routing outage.
    TimedOut,
    /// The subscriber withdrew. Nothing to answer.
    DownstreamLeft,
}

/// Result of running routing work during a rendezvous hold.
enum RendezvousWork<T> {
    Completed(T),
    TimedOut,
    DownstreamLeft,
}

/// Producer of tracks to a remote Subscriber
#[derive(Clone)]
pub struct Producer {
    publisher: Publisher,
    locals: Locals,
    remotes: RemoteManager,
    upstream_namespaces: UpstreamNamespaces,
    rendezvous_hold_permits: Arc<Semaphore>,
    /// Relay-level context for this MoQT session.
    context: SessionContext,
}

/// Why the wait for upstream readiness ended without the subscription being
/// established.
///
/// An upstream rejection and a departing subscriber can both produce `Cancel`
/// or `Done`, so the direction has to come from the branch that resolved.
/// Otherwise publisher failures appear as subscriber cancellations in the
/// subscribe metrics.
enum UpstreamWait {
    /// The upstream subscription could not be established.
    UpstreamFailed(ServeError),

    /// The downstream subscriber went away before it was established.
    DownstreamLeft(ServeError),

    /// The rendezvous window closed before the upstream subscription was
    /// established.
    TimedOut,
}

impl Producer {
    pub fn new(
        publisher: Publisher,
        locals: Locals,
        remotes: RemoteManager,
        coordinator: Arc<dyn Coordinator>,
        context: SessionContext,
    ) -> Self {
        let (upstream_namespaces, runner) =
            UpstreamNamespaces::new(locals.clone(), remotes.clone(), coordinator);
        tokio::spawn(runner.run());
        Self::new_with_upstream_namespaces(publisher, locals, remotes, upstream_namespaces, context)
    }

    pub(crate) fn new_with_upstream_namespaces(
        publisher: Publisher,
        locals: Locals,
        remotes: RemoteManager,
        upstream_namespaces: UpstreamNamespaces,
        context: SessionContext,
    ) -> Self {
        Self {
            publisher,
            locals,
            remotes,
            upstream_namespaces,
            rendezvous_hold_permits: Arc::new(Semaphore::new(
                MAX_CONCURRENT_RENDEZVOUS_HOLDS_PER_SESSION,
            )),
            context,
        }
    }

    fn try_acquire_rendezvous_hold(&self) -> Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
        let session = self
            .rendezvous_hold_permits
            .clone()
            .try_acquire_owned()
            .ok()?;
        let relay = self.locals.try_acquire_rendezvous_hold()?;
        Some((session, relay))
    }

    fn try_reserve_unresolved_capacity(
        &self,
        permit: &mut Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)>,
        gauge: &mut Option<GaugeGuard>,
    ) -> bool {
        if permit.is_some() {
            return true;
        }

        let Some(acquired) = self.try_acquire_rendezvous_hold() else {
            return false;
        };
        *permit = Some(acquired);
        *gauge = Some(GaugeGuard::new("moq_relay_active_rendezvous_holds"));
        true
    }

    fn reject_unresolved_overload(
        &self,
        subscribed: Subscribed,
        timing_guard: &mut TimingGuard,
    ) -> Result<(), anyhow::Error> {
        metrics::counter!("moq_relay_rendezvous_rejections_total").increment(1);
        timing_guard.set_label("source", "rendezvous_overloaded");
        let err = ServeError::Closed(RequestErrorCode::ExcessiveLoad as u64);
        let retry_interval = self.locals.rendezvous_overload_retry_interval();
        let _ = subscribed.close_with_retry(err.clone(), retry_interval);
        Err(err.into())
    }

    /// Send PUBLISH_NAMESPACE for a set of tracks to the remote peer.
    pub async fn publish_namespace(&mut self, tracks: TracksReader) -> Result<(), SessionError> {
        self.publisher.publish_namespace(tracks).await
    }

    /// Run the producer to serve subscribe requests.
    pub async fn run(self) -> Result<(), SessionError> {
        let mut tasks: FuturesUnordered<futures::future::BoxFuture<'static, ()>> =
            FuturesUnordered::new();

        loop {
            let mut publisher_subscribed = self.publisher.clone();
            let mut publisher_track_status = self.publisher.clone();
            let mut publisher_subscribed_namespace = self.publisher.clone();

            tokio::select! {
                // Handle a new subscribe request
                Some(subscribed) = publisher_subscribed.subscribed() => {
                    metrics::counter!("moq_relay_subscribers_total").increment(1);

                    let this = self.clone();

                    // Spawn a new task to handle the subscribe
                    tasks.push(async move {
                        let info = subscribed.clone();
                        let namespace = info.track_namespace.to_utf8_path();
                        let track_name = info.track_name.clone();
                        tracing::info!(namespace = %namespace, track = %track_name, "serving subscribe: {:?}", info);

                        // Serve the subscribe request
                        if let Err(err) = this.serve_subscribe(subscribed).await {
                            if Self::is_expected_serve_shutdown(&err) {
                                tracing::debug!(namespace = %namespace, track = %track_name, subscribe_info = ?info, error = %err, "stopped serving subscribe");
                            } else {
                                tracing::warn!(namespace = %namespace, track = %track_name, subscribe_info = ?info, error = %err, "failed serving subscribe");
                            }
                        }
                    }.boxed())
                },
                // Handle a new track_status request
                Some(track_status_requested) = publisher_track_status.track_status_requested() => {
                    let this = self.clone();

                    // Spawn a new task to handle the track_status request
                    tasks.push(async move {
                        let info = track_status_requested.request_msg.clone();
                        let namespace = info.track_namespace.to_utf8_path();
                        let track_name = info.track_name.clone();
                        tracing::info!(namespace = %namespace, track = %track_name, "serving track_status: {:?}", info);

                        // Serve the track_status request
                        if let Err(err) = this.serve_track_status(track_status_requested).await {
                            tracing::warn!(namespace = %namespace, track = %track_name, error = %err, "failed serving track_status: {:?}, error: {}", info, err)
                        }
                    }.boxed())
                },
                // Handle a new namespace subscription request.
                Some(subscribed_namespace) = publisher_subscribed_namespace.subscribed_namespace() => {
                    let this = self.clone();

                    tasks.push(async move {
                        let prefix = subscribed_namespace.namespace_prefix.to_utf8_path();
                        tracing::info!(namespace_prefix = %prefix, "serving subscribe namespace");

                        if let Err(err) = this.serve_subscribe_namespace(subscribed_namespace).await {
                            if Self::is_expected_serve_shutdown(&err) {
                                tracing::debug!(namespace_prefix = %prefix, error = %err, "stopped serving subscribe namespace");
                            } else {
                                tracing::warn!(namespace_prefix = %prefix, error = %err, "failed serving subscribe namespace");
                            }
                        }
                    }.boxed())
                },
                _= tasks.next(), if !tasks.is_empty() => {},
                else => return Ok(()),
            };
        }
    }

    /// Serve a subscribe request.
    async fn serve_subscribe(self, subscribed: Subscribed) -> Result<(), anyhow::Error> {
        // Track subscribe latency from request to track resolution (records on drop)
        let mut timing_guard =
            TimingGuard::with_label("moq_relay_subscribe_latency_seconds", "source", "not_found");
        // Track active subscriptions - decrements when this function returns
        let _sub_guard = GaugeGuard::new("moq_relay_active_subscriptions");

        let namespace = subscribed.track_namespace.clone();
        let track_name = subscribed.track_name.clone();

        let requested_timeout = match subscribed.params.rendezvous_timeout() {
            Ok(timeout) => timeout,
            Err(err) => {
                let serve_err = ServeError::internal_ctx(format!(
                    "validated rendezvous timeout became invalid: {err}"
                ));
                let _ = subscribed.close(serve_err.clone());
                return Err(serve_err.into());
            }
        };
        let hold = rendezvous_hold(requested_timeout, MAX_RENDEZVOUS_TIMEOUT);
        // Subscribing before the first lookup closes the race where a publisher
        // arrives between the lookup missing and the wait starting.
        let mut rendezvous = hold.map(|hold| {
            (
                tokio::time::Instant::now() + hold,
                self.locals.subscribe_track_changes(),
                self.locals.subscribe_namespace_changes(),
            )
        });
        let deadline = rendezvous.as_ref().map(|(deadline, _, _)| *deadline);
        let mut coordinator_lookup_succeeded = false;
        let mut rendezvous_permit = None;
        let mut rendezvous_guard = None;
        let full_name = FullTrackName {
            namespace: namespace.clone(),
            name: track_name.clone(),
        };

        // Re-entered after a publisher appears. The lookup is repeated rather
        // than trusting the wake, because `Consumer::serve_track` emits
        // TrackChange::Added before the coordinator registration behind it has
        // succeeded, so an event does not guarantee a servable track.
        loop {
            let mut locals = self.locals.clone();
            let mut local = locals.retrieve_track_with_interest(self.context.scope(), &full_name);

            // A namespace route creates an upstream request and cache generation,
            // so admission must precede that work. Exact established tracks above
            // stay on the unmetered fast path.
            if local.is_none() && locals.has_namespace_route(self.context.scope(), &namespace) {
                if !self
                    .try_reserve_unresolved_capacity(&mut rendezvous_permit, &mut rendezvous_guard)
                {
                    return self.reject_unresolved_overload(subscribed, &mut timing_guard);
                }

                local = if let Some(deadline) = deadline {
                    match Self::await_rendezvous_work(
                        &subscribed,
                        deadline,
                        locals.get_or_request_track(
                            self.context.scope(),
                            namespace.clone(),
                            &track_name,
                        ),
                    )
                    .await
                    {
                        RendezvousWork::Completed(result) => result,
                        RendezvousWork::TimedOut => {
                            return Self::finish_unresolved_rendezvous(
                                subscribed,
                                &mut timing_guard,
                                coordinator_lookup_succeeded,
                                &namespace,
                                &track_name,
                            );
                        }
                        RendezvousWork::DownstreamLeft => {
                            tracing::debug!(
                                namespace = %namespace.to_utf8_path(),
                                track = %track_name,
                                "subscriber left during a rendezvous local lookup"
                            );
                            timing_guard.set_label("source", "downstream_left");
                            return Ok(());
                        }
                    }
                } else {
                    locals
                        .get_or_request_track(self.context.scope(), namespace.clone(), &track_name)
                        .await
                };

                if local.is_none() {
                    drop(rendezvous_guard.take());
                    drop(rendezvous_permit.take());
                }
            }

            if let Some(local) = local {
                let largest_location = local.largest_location;
                let ns = namespace.to_utf8_path();
                tracing::info!(namespace = %ns, track = %track_name, source = "local", "serving subscribe from local: {:?}", local.reader.info);
                timing_guard.set_label("source", "local");
                let _track_guard = GaugeGuard::new("moq_relay_active_tracks");
                // Held until serving finishes. Once the last guard for a cached track
                // drops, its upstream subscription becomes eligible for release.
                let _interest_guard = local.interest;

                // Draft-18 §9.4: "The relay MUST have an Established upstream
                // subscription before sending SUBSCRIBE_OK in response to a
                // downstream SUBSCRIBE." A pull-through cache entry exists before
                // its upstream subscription does, so wait for it.
                if let Some(upstream) = local.upstream {
                    if upstream.is_pending()
                        && !self.try_reserve_unresolved_capacity(
                            &mut rendezvous_permit,
                            &mut rendezvous_guard,
                        )
                    {
                        // Recheck after failed admission because the upstream may
                        // have completed while permits were inspected.
                        if upstream.is_pending() {
                            return self.reject_unresolved_overload(subscribed, &mut timing_guard);
                        }
                    }

                    if let Err(outcome) =
                        Self::await_upstream(&subscribed, &upstream, deadline).await
                    {
                        // Which side ended the wait is decided by the branch that
                        // resolved, not by the error variant: an upstream failure can
                        // itself be Cancel or Done, so sniffing the variant would
                        // report a publisher-side failure as a subscriber cancellation
                        // and skip the upstream-error counter.
                        let err = match outcome {
                            UpstreamWait::DownstreamLeft(err) => {
                                tracing::debug!(namespace = %ns, track = %track_name, error = %err, "downstream subscriber left before the upstream subscription was established");
                                timing_guard.set_label("source", "downstream_left");
                                err
                            }
                            UpstreamWait::UpstreamFailed(err) => {
                                tracing::warn!(namespace = %ns, track = %track_name, error = %err, "upstream subscription could not be established");
                                metrics::counter!("moq_relay_subscribe_upstream_errors_total")
                                    .increment(1);
                                timing_guard.set_label("source", "upstream_error");
                                err
                            }
                            UpstreamWait::TimedOut => {
                                return Self::finish_rendezvous_timeout(
                                    subscribed,
                                    &mut timing_guard,
                                );
                            }
                        };

                        // Rejects when the subscription is already closed (the
                        // downstream-left case), which is fine: the error below is
                        // still the reason we stopped.
                        let _ = subscribed.close(err.clone());
                        return Err(err.into());
                    }
                }

                drop(rendezvous_guard.take());
                drop(rendezvous_permit.take());
                return Self::serve_resolved_track(
                    subscribed,
                    local.reader,
                    largest_location,
                    deadline,
                    &mut timing_guard,
                )
                .await;
            }

            // Check remote tracks after local exact tracks and namespace route sources.
            if !self.try_reserve_unresolved_capacity(&mut rendezvous_permit, &mut rendezvous_guard)
            {
                return self.reject_unresolved_overload(subscribed, &mut timing_guard);
            }

            let remote = if let Some(deadline) = deadline {
                match Self::await_rendezvous_work(
                    &subscribed,
                    deadline,
                    self.remotes.subscribe_with_lookup_status(
                        self.context.scope(),
                        &namespace,
                        &track_name,
                        &mut coordinator_lookup_succeeded,
                    ),
                )
                .await
                {
                    RendezvousWork::Completed(result) => result,
                    RendezvousWork::TimedOut => {
                        return Self::finish_unresolved_rendezvous(
                            subscribed,
                            &mut timing_guard,
                            coordinator_lookup_succeeded,
                            &namespace,
                            &track_name,
                        );
                    }
                    RendezvousWork::DownstreamLeft => {
                        tracing::debug!(
                            namespace = %namespace.to_utf8_path(),
                            track = %track_name,
                            "subscriber left during a rendezvous route lookup"
                        );
                        timing_guard.set_label("source", "downstream_left");
                        return Ok(());
                    }
                }
            } else {
                self.remotes
                    .subscribe_with_lookup_status(
                        self.context.scope(),
                        &namespace,
                        &track_name,
                        &mut coordinator_lookup_succeeded,
                    )
                    .await
            };

            match remote {
                Ok(Some((track, interest_guard))) => {
                    let largest_location = track.largest_location();
                    let ns = namespace.to_utf8_path();
                    tracing::info!(namespace = %ns, track = %track_name, source = "remote", "serving subscribe from remote: {:?}", track.info);
                    // Update label to indicate remote source, timing recorded on drop
                    timing_guard.set_label("source", "remote");
                    // Track active tracks - decrements when serve completes
                    let _track_guard = GaugeGuard::new("moq_relay_active_tracks");
                    // Held until serving finishes; the cross-relay subscription is
                    // released once the last guard for this track drops.
                    let _interest_guard = interest_guard;
                    drop(rendezvous_guard.take());
                    drop(rendezvous_permit.take());
                    return Self::serve_resolved_track(
                        subscribed,
                        track,
                        largest_location,
                        deadline,
                        &mut timing_guard,
                    )
                    .await;
                }
                Ok(None) => coordinator_lookup_succeeded = true,
                Err(RemoteSubscribeError::Peer(err)) => {
                    let ns = namespace.to_utf8_path();
                    tracing::debug!(namespace = %ns, track = %track_name, error = %err, "upstream relay rejected subscribe");
                    metrics::counter!("moq_relay_subscribe_upstream_errors_total").increment(1);
                    timing_guard.set_label("source", "upstream_error");
                    let _ = subscribed.close(err.clone());
                    return Err(err.into());
                }
                Err(RemoteSubscribeError::Route(e)) => {
                    // Route error = infrastructure failure (couldn't reach coordinator/upstream)
                    // This is different from "not found" - we don't know if the track exists
                    let ns = namespace.to_utf8_path();
                    tracing::error!(namespace = %ns, track = %track_name, error = %e, "failed to route to remote: {}", e);
                    metrics::counter!("moq_relay_subscribe_route_errors_total").increment(1);

                    // A hold turns this from an answer into a retry. The subscriber
                    // asked to be kept waiting, a lookup that failed says nothing
                    // about whether the track exists, and the next re-check may well
                    // succeed, so failing the subscribe here would make one slow
                    // coordinator call the end of every rendezvous crossing it.
                    if deadline.is_none() {
                        timing_guard.set_label("source", "route_error");

                        // Return an internal error rather than "not found" since we couldn't check
                        // TODO: Consider returning a more specific error to the subscriber
                        let err = ServeError::internal_ctx(format!(
                            "route error for namespace '{}': {}",
                            namespace, e
                        ));
                        subscribed.close(err.clone())?;
                        return Err(err.into());
                    }
                }
            }

            // Nothing is publishing this track. Without a rendezvous window that is
            // the end of it; with one, hold the subscription open and re-check when
            // a publisher shows up.
            let Some((deadline, track_changes, namespace_changes)) = rendezvous.as_mut() else {
                // timing_guard label already set to "not_found", will record on drop
                metrics::counter!("moq_relay_subscribe_not_found_total").increment(1);

                let err = ServeError::not_found_ctx(format!(
                    "track '{}/{}' not found in local or remote tracks",
                    namespace, track_name
                ));
                subscribed.close(err.clone())?;
                return Err(err.into());
            };

            // Capacity limits held requests, not ordinary resolution work. An
            // available publisher must be served even when every hold slot is in
            // use, so only acquire a slot after the initial lookup misses.
            if rendezvous_permit.is_none()
                && !self
                    .try_reserve_unresolved_capacity(&mut rendezvous_permit, &mut rendezvous_guard)
            {
                return self.reject_unresolved_overload(subscribed, &mut timing_guard);
            }

            match Self::await_rendezvous(
                &subscribed,
                self.context.scope(),
                &namespace,
                &track_name,
                *deadline,
                track_changes,
                namespace_changes,
            )
            .await
            {
                // A publisher may exist now, either because one arrived here or
                // because enough time passed to be worth asking elsewhere. Either
                // way the answer comes from repeating the lookup.
                Rendezvous::PublisherAppeared | Rendezvous::RecheckDue => continue,
                Rendezvous::TimedOut => {
                    return Self::finish_unresolved_rendezvous(
                        subscribed,
                        &mut timing_guard,
                        coordinator_lookup_succeeded,
                        &namespace,
                        &track_name,
                    );
                }
                Rendezvous::DownstreamLeft => {
                    timing_guard.set_label("source", "downstream_left");
                    return Ok(());
                }
            }
        }
    }

    /// Wait for the upstream subscription behind a cached track to be established.
    ///
    /// Also completes when the rendezvous deadline passes or the downstream
    /// subscriber goes away. Upstream and downstream failures are reported
    /// separately because the error alone does not identify their direction.
    async fn await_upstream(
        subscribed: &Subscribed,
        upstream: &UpstreamReady,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<(), UpstreamWait> {
        let Some(deadline) = deadline else {
            return tokio::select! {
                res = upstream.established() => res.map_err(UpstreamWait::UpstreamFailed),
                res = subscribed.closed() => Err(UpstreamWait::DownstreamLeft(
                    res.err().unwrap_or(ServeError::Done),
                )),
            };
        };

        tokio::select! {
            biased;
            res = subscribed.closed() => Err(UpstreamWait::DownstreamLeft(
                res.err().unwrap_or(ServeError::Done),
            )),
            () = tokio::time::sleep_until(deadline) => Err(UpstreamWait::TimedOut),
            res = upstream.established() => {
                if tokio::time::Instant::now() >= deadline {
                    Err(UpstreamWait::TimedOut)
                } else {
                    res.map_err(UpstreamWait::UpstreamFailed)
                }
            },
        }
    }

    /// Run routing work under the rendezvous deadline and downstream cancellation
    /// signal.
    async fn await_rendezvous_work<T>(
        subscribed: &Subscribed,
        deadline: tokio::time::Instant,
        lookup: impl Future<Output = T>,
    ) -> RendezvousWork<T> {
        tokio::select! {
            biased;
            _ = subscribed.closed() => RendezvousWork::DownstreamLeft,
            () = tokio::time::sleep_until(deadline) => RendezvousWork::TimedOut,
            result = lookup => {
                if tokio::time::Instant::now() >= deadline {
                    RendezvousWork::TimedOut
                } else {
                    RendezvousWork::Completed(result)
                }
            },
        }
    }

    fn finish_rendezvous_timeout(
        subscribed: Subscribed,
        timing_guard: &mut TimingGuard,
    ) -> Result<(), anyhow::Error> {
        Self::record_rendezvous_timeout(timing_guard);

        // TIMEOUT records that the relay completed routing work but no usable
        // publisher became ready within the requested window.
        let err = ServeError::Timeout;
        subscribed.close(err.clone())?;
        Err(err.into())
    }

    fn record_rendezvous_timeout(timing_guard: &mut TimingGuard) {
        metrics::counter!("moq_relay_rendezvous_timeouts_total").increment(1);
        timing_guard.set_label("source", "rendezvous_timeout");
    }

    async fn serve_resolved_track(
        subscribed: Subscribed,
        track: TrackReader,
        largest_location: Option<moq_transport::coding::Location>,
        deadline: Option<tokio::time::Instant>,
        timing_guard: &mut TimingGuard,
    ) -> Result<(), anyhow::Error> {
        let Some(deadline) = deadline else {
            return Ok(subscribed
                .serve_with_largest_location(track, largest_location)
                .await?);
        };

        match subscribed
            .serve_with_deadline_and_largest_location(track, deadline, largest_location)
            .await
        {
            Ok(()) => Ok(()),
            Err(ServeWithDeadlineError::DeadlineExpired) => {
                Self::record_rendezvous_timeout(timing_guard);
                Err(ServeError::Timeout.into())
            }
            Err(ServeWithDeadlineError::Session(err)) => Err(err.into()),
        }
    }

    fn finish_unresolved_rendezvous(
        subscribed: Subscribed,
        timing_guard: &mut TimingGuard,
        coordinator_lookup_succeeded: bool,
        namespace: &TrackNamespace,
        track_name: &TrackName,
    ) -> Result<(), anyhow::Error> {
        if coordinator_lookup_succeeded {
            return Self::finish_rendezvous_timeout(subscribed, timing_guard);
        }

        // No successful lookup means the relay never established that the track
        // was absent. A publisher timeout would hide the routing outage.
        metrics::counter!("moq_relay_subscribe_route_errors_total").increment(1);
        timing_guard.set_label("source", "route_error");
        let err = ServeError::internal_ctx(format!(
            "no coordinator lookup completed during rendezvous hold for '{}/{}'",
            namespace, track_name
        ));
        subscribed.close(err.clone())?;
        Err(err.into())
    }

    /// Hold a SUBSCRIBE open until a publisher for the track shows up, the
    /// deadline passes, or the subscriber gives up (draft-18 §10.2.6).
    ///
    /// Both a new track and a newly announced namespace count as a publisher
    /// arriving: a track can be served either from an exact PUBLISH or by a
    /// PUBLISH_NAMESPACE route source that triggers an upstream SUBSCRIBE, and
    /// the caller re-runs the full lookup either way.
    ///
    /// Those events are in-process, so they miss a publisher on another relay
    /// serving the same scope. The recheck interval covers that by ending the wait
    /// periodically for the caller to ask the coordinator again.
    ///
    /// Waking the subscriber promptly matters beyond tidiness. A held
    /// subscription keeps its entry in the publisher's per-session name map, and
    /// a second SUBSCRIBE for the same track on that session is rejected with
    /// DUPLICATE_SUBSCRIPTION, so a subscriber that gave up and retried would be
    /// turned away by its own abandoned hold.
    async fn await_rendezvous(
        subscribed: &Subscribed,
        scope: Option<&str>,
        namespace: &TrackNamespace,
        track_name: &TrackName,
        deadline: tokio::time::Instant,
        track_changes: &mut broadcast::Receiver<TrackChange>,
        namespace_changes: &mut broadcast::Receiver<NamespaceChange>,
    ) -> Rendezvous {
        let sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(sleep);
        let recheck = tokio::time::sleep(RENDEZVOUS_RECHECK_INTERVAL);
        tokio::pin!(recheck);

        loop {
            tokio::select! {
                biased;

                res = subscribed.closed() => {
                    tracing::debug!(
                        namespace = %namespace.to_utf8_path(),
                        track = %track_name,
                        reason = ?res.err(),
                        "subscriber left while waiting for a publisher"
                    );
                    return Rendezvous::DownstreamLeft;
                }

                () = &mut sleep => return Rendezvous::TimedOut,

                () = &mut recheck => {
                    tracing::debug!(
                        namespace = %namespace.to_utf8_path(),
                        track = %track_name,
                        "asking the coordinator again for a publisher during a rendezvous hold"
                    );
                    return Rendezvous::RecheckDue;
                }

                change = track_changes.recv() => {
                    match change {
                        Ok(TrackChange::Added { scope: change_scope, track })
                            if change_scope.as_deref() == scope
                                && track.info.namespace == *namespace
                                && track.info.name == *track_name =>
                        {
                            return Rendezvous::PublisherAppeared
                        }
                        // Some other track, or a removal: keep waiting.
                        Ok(_) => continue,
                        // Lagged means events were missed, one of which could have
                        // been ours, so re-check rather than keep waiting blind.
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            return Rendezvous::PublisherAppeared
                        }
                        Err(broadcast::error::RecvError::Closed) => return Rendezvous::TimedOut,
                    }
                }

                change = namespace_changes.recv() => {
                    match change {
                        Ok(NamespaceChange { scope: change_scope, namespace: ns, added: true })
                            if change_scope.as_deref() == scope
                                && namespace_covers(&ns, namespace) =>
                        {
                            return Rendezvous::PublisherAppeared
                        }
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            return Rendezvous::PublisherAppeared
                        }
                        Err(broadcast::error::RecvError::Closed) => return Rendezvous::TimedOut,
                    }
                }
            }
        }
    }

    /// Serve a SUBSCRIBE_NAMESPACE request using relay-local namespace state.
    async fn serve_subscribe_namespace(
        self,
        mut subscribed_namespace: SubscribedNamespace,
    ) -> Result<(), anyhow::Error> {
        let wants_namespace = wants_namespace(subscribed_namespace.subscribe_options);
        let wants_publish = wants_publish(subscribed_namespace.subscribe_options);
        let namespace_changes = self.locals.subscribe_namespace_changes();
        let track_changes = self.locals.subscribe_track_changes();
        let mut publish_tasks: FuturesUnordered<futures::future::BoxFuture<'static, ()>> =
            FuturesUnordered::new();

        let _upstream_lease = if wants_namespace {
            match self
                .upstream_namespaces
                .subscribe(&self.context, subscribed_namespace.namespace_prefix.clone())
            {
                Ok(lease) => Some(lease),
                Err(error) => {
                    tracing::error!(
                        prefix = %subscribed_namespace.namespace_prefix,
                        error = %error,
                        "failed to acquire shared upstream namespace lease; serving local state only"
                    );
                    None
                }
            }
        } else {
            None
        };

        subscribed_namespace.ok()?;

        let mut known_namespaces = HashSet::new();

        if wants_namespace {
            self.send_namespace_snapshot(&mut subscribed_namespace, &mut known_namespaces)?;
        }

        let mut known_tracks = HashSet::new();
        if wants_publish {
            self.send_publish_snapshot(
                &subscribed_namespace,
                &mut known_tracks,
                &mut publish_tasks,
            )
            .await?;
        }

        self.serve_subscribe_namespace_loop(
            subscribed_namespace,
            wants_namespace,
            wants_publish,
            namespace_changes,
            track_changes,
            publish_tasks,
            known_namespaces,
            known_tracks,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn serve_subscribe_namespace_loop(
        self,
        subscribed_namespace: SubscribedNamespace,
        wants_namespace: bool,
        wants_publish: bool,
        mut namespace_changes: tokio::sync::broadcast::Receiver<NamespaceChange>,
        mut track_changes: tokio::sync::broadcast::Receiver<TrackChange>,
        mut publish_tasks: FuturesUnordered<futures::future::BoxFuture<'static, ()>>,
        mut known_namespaces: HashSet<TrackNamespace>,
        mut known_tracks: HashSet<FullTrackName>,
    ) -> Result<(), anyhow::Error> {
        let mut subscribed_namespace = subscribed_namespace;
        loop {
            tokio::select! {
                res = subscribed_namespace.closed() => {
                    res?;
                    return Ok(());
                }
                change = namespace_changes.recv(), if wants_namespace => {
                    match change {
                        Ok(change) => {
                            self.apply_namespace_change(&mut subscribed_namespace, &mut known_namespaces, change)?;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            // Recoverable: a full resync reconstructs the state the
                            // skipped events would have produced. Counted so that
                            // sustained churn outgrowing the channel capacity is
                            // visible before it shows up as latency.
                            metrics::counter!("moq_relay_change_channel_lagged_total", "channel" => "namespace")
                                .increment(skipped);
                            self.resync_namespaces(&mut subscribed_namespace, &mut known_namespaces)?;
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
                change = track_changes.recv(), if wants_publish => {
                    match change {
                        Ok(change) => {
                            self.apply_track_change(&subscribed_namespace, &mut known_tracks, &mut publish_tasks, change).await?;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            metrics::counter!("moq_relay_change_channel_lagged_total", "channel" => "track")
                                .increment(skipped);
                            self.resync_publish_tracks(&subscribed_namespace, &mut known_tracks, &mut publish_tasks).await?;
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    }
                }
                _ = publish_tasks.next(), if !publish_tasks.is_empty() => {},
            }
        }
    }

    fn send_namespace_snapshot(
        &self,
        subscribed_namespace: &mut SubscribedNamespace,
        known: &mut HashSet<TrackNamespace>,
    ) -> Result<(), ServeError> {
        for namespace in self
            .locals
            .list_namespaces_matching(self.context.scope(), &subscribed_namespace.namespace_prefix)
        {
            if known.insert(namespace.clone()) {
                subscribed_namespace.namespace(&namespace)?;
            }
        }

        Ok(())
    }

    fn apply_namespace_change(
        &self,
        subscribed_namespace: &mut SubscribedNamespace,
        known: &mut HashSet<TrackNamespace>,
        change: NamespaceChange,
    ) -> Result<(), ServeError> {
        if change.scope.as_deref() != self.context.scope() {
            return Ok(());
        }

        if !subscribed_namespace
            .namespace_prefix
            .is_prefix_of(&change.namespace)
        {
            return Ok(());
        }

        if change.added {
            if known.insert(change.namespace.clone()) {
                subscribed_namespace.namespace(&change.namespace)?;
            }
        } else if known.remove(&change.namespace) {
            subscribed_namespace.namespace_done(&change.namespace)?;
        }

        Ok(())
    }

    fn resync_namespaces(
        &self,
        subscribed_namespace: &mut SubscribedNamespace,
        known: &mut HashSet<TrackNamespace>,
    ) -> Result<(), ServeError> {
        let current: HashSet<_> = self
            .locals
            .list_namespaces_matching(self.context.scope(), &subscribed_namespace.namespace_prefix)
            .into_iter()
            .collect();

        for namespace in current.difference(known) {
            subscribed_namespace.namespace(namespace)?;
        }

        for namespace in known.difference(&current) {
            subscribed_namespace.namespace_done(namespace)?;
        }

        *known = current;
        Ok(())
    }

    async fn send_publish_snapshot(
        &self,
        subscribed_namespace: &SubscribedNamespace,
        known: &mut HashSet<FullTrackName>,
        publish_tasks: &mut FuturesUnordered<futures::future::BoxFuture<'static, ()>>,
    ) -> Result<(), anyhow::Error> {
        for track in self
            .locals
            .list_tracks_matching(self.context.scope(), &subscribed_namespace.namespace_prefix)
        {
            self.publish_track_for_namespace(subscribed_namespace, known, publish_tasks, track)
                .await?;
        }

        Ok(())
    }

    async fn apply_track_change(
        &self,
        subscribed_namespace: &SubscribedNamespace,
        known: &mut HashSet<FullTrackName>,
        publish_tasks: &mut FuturesUnordered<futures::future::BoxFuture<'static, ()>>,
        change: TrackChange,
    ) -> Result<(), anyhow::Error> {
        match change {
            TrackChange::Added { scope, track } => {
                if scope.as_deref() != self.context.scope()
                    || !subscribed_namespace
                        .namespace_prefix
                        .is_prefix_of(&track.namespace)
                {
                    return Ok(());
                }

                self.publish_track_for_namespace(subscribed_namespace, known, publish_tasks, track)
                    .await
            }
            TrackChange::Removed { scope, full_name } => {
                if scope.as_deref() == self.context.scope() {
                    known.remove(&full_name);
                }
                Ok(())
            }
        }
    }

    async fn resync_publish_tracks(
        &self,
        subscribed_namespace: &SubscribedNamespace,
        known: &mut HashSet<FullTrackName>,
        publish_tasks: &mut FuturesUnordered<futures::future::BoxFuture<'static, ()>>,
    ) -> Result<(), anyhow::Error> {
        // Single pass: build only the `current` set while publishing new tracks,
        // instead of materializing an intermediate Vec of (name, reader) pairs.
        let mut current = HashSet::new();
        for track in self
            .locals
            .list_tracks_matching(self.context.scope(), &subscribed_namespace.namespace_prefix)
        {
            let full_name = full_name_for_track(&track);
            if !known.contains(&full_name) {
                self.publish_track_for_namespace(subscribed_namespace, known, publish_tasks, track)
                    .await?;
            }
            current.insert(full_name);
        }

        known.retain(|full_name| current.contains(full_name));
        Ok(())
    }

    async fn publish_track_for_namespace(
        &self,
        subscribed_namespace: &SubscribedNamespace,
        known: &mut HashSet<FullTrackName>,
        publish_tasks: &mut FuturesUnordered<futures::future::BoxFuture<'static, ()>>,
        track: TrackReader,
    ) -> Result<(), anyhow::Error> {
        let full_name = full_name_for_track(&track);
        if known.contains(&full_name) {
            return Ok(());
        }

        let mut params = KeyValuePairs::default();
        if !subscribed_namespace.forward {
            params.set_forward(false);
        }

        let namespace = full_name.namespace.to_utf8_path();
        let track_name = full_name.name.to_string();
        let mut publisher = self.publisher.clone();
        let published = match publisher.publish(track, params).await {
            Ok(published) => published,
            Err(SessionError::Serve(ServeError::Duplicate)) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        known.insert(full_name);
        publish_tasks.push(
            async move {
                if let Err(err) = published.serve().await {
                    tracing::warn!(namespace = %namespace, track = %track_name, error = %err, "failed serving PUBLISH for SUBSCRIBE_NAMESPACE");
                }
            }
            .boxed(),
        );

        Ok(())
    }

    fn is_expected_serve_shutdown(err: &anyhow::Error) -> bool {
        let serve = match err.downcast_ref::<SessionError>() {
            Some(SessionError::Serve(err)) => Some(err),
            _ => err.downcast_ref::<ServeError>(),
        };

        serve.is_some_and(Self::is_expected_serve_shutdown_err)
    }

    /// True for the errors that mean nobody is waiting for the subscription any
    /// more, rather than a failure worth warning about.
    fn is_expected_serve_shutdown_err(err: &ServeError) -> bool {
        matches!(err, ServeError::Cancel | ServeError::Done)
    }

    /// Serve a track_status request.
    async fn serve_track_status(
        self,
        mut track_status_requested: TrackStatusRequested,
    ) -> Result<(), anyhow::Error> {
        let full_name = FullTrackName {
            namespace: track_status_requested.request_msg.track_namespace.clone(),
            name: track_status_requested.request_msg.track_name.clone(),
        };

        // Check actual local tracks first.
        if let Some(track) = self.locals.retrieve_track(self.context.scope(), &full_name) {
            let namespace = full_name.namespace.to_utf8_path();
            let track_name = &full_name.name;
            tracing::info!(namespace = %namespace, track = %track_name, source = "local", "serving track_status from local: {:?}", track.info);
            return Ok(track_status_requested.respond_ok(&track)?);
        }

        // TODO - forward track status to remotes?
        // Check remote tracks second, and serve from remote if possible
        /*
        if let Some(remotes) = &self.remotes {
            // Try to route to a remote for this namespace
            if let Some(remote) = remotes.route(&subscribe.track_namespace).await? {
                if let Some(track) =
                    remote.subscribe(subscribe.track_namespace.clone(), subscribe.track_name.clone())?
                {
                    tracing::info!("serving from remote: {:?} {:?}", remote.info, track.info);

                    // NOTE: Depends on drop(track) being called afterwards
                    return Ok(subscribe.serve(track.reader).await?);
                }
            }
        }*/

        track_status_requested.respond_error(
            moq_transport::message::RequestErrorCode::DoesNotExist as u64,
            "track not found",
        )?;

        Err(ServeError::not_found_ctx(format!(
            "track '{}/{}' not found for track_status",
            track_status_requested.request_msg.track_namespace,
            track_status_requested.request_msg.track_name
        ))
        .into())
    }
}

fn wants_namespace(options: SubscribeOptions) -> bool {
    matches!(
        options,
        SubscribeOptions::Namespace | SubscribeOptions::Both
    )
}

fn wants_publish(options: SubscribeOptions) -> bool {
    matches!(options, SubscribeOptions::Publish | SubscribeOptions::Both)
}

fn full_name_for_track(track: &TrackReader) -> FullTrackName {
    FullTrackName {
        namespace: track.namespace.clone(),
        name: track.name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use moq_transport::{
        coding::{KeyValuePairs, TrackNamespace},
        message::RequestErrorCode,
        serve::{ServeError, Track},
        session::{Session as TransportSession, SessionError, Subscriber, Transport},
    };

    use super::{
        namespace_covers, rendezvous_hold, Producer, MAX_CONCURRENT_RENDEZVOUS_HOLDS_PER_SESSION,
    };
    use crate::{
        Coordinator, CoordinatorContext, CoordinatorError, CoordinatorResult, Locals,
        NamespaceOrigin, NamespaceRegistration, RemoteManager, SessionContext, TrackRequest,
    };

    #[derive(Default)]
    struct CountingCoordinator {
        track_lookups: Mutex<Vec<String>>,
    }

    struct RemoteCoordinator {
        url: url::Url,
    }

    impl CountingCoordinator {
        fn looked_up(&self, track: &str) -> bool {
            self.track_lookups
                .lock()
                .expect("track lookup lock poisoned")
                .iter()
                .any(|lookup| lookup == track)
        }

        fn distinct_lookup_count(&self, prefix: &str) -> usize {
            self.track_lookups
                .lock()
                .expect("track lookup lock poisoned")
                .iter()
                .filter(|lookup| lookup.starts_with(prefix))
                .collect::<std::collections::HashSet<_>>()
                .len()
        }
    }

    #[async_trait]
    impl Coordinator for CountingCoordinator {
        async fn register_namespace(
            &self,
            _scope: Option<&str>,
            _namespace: &TrackNamespace,
            _context: &CoordinatorContext,
        ) -> CoordinatorResult<NamespaceRegistration> {
            Ok(NamespaceRegistration::new(()))
        }

        async fn unregister_namespace(
            &self,
            _scope: Option<&str>,
            _namespace: &TrackNamespace,
        ) -> CoordinatorResult<()> {
            Ok(())
        }

        async fn lookup(
            &self,
            _scope: Option<&str>,
            _namespace: &TrackNamespace,
        ) -> CoordinatorResult<(NamespaceOrigin, Option<moq_native_ietf::quic::Client>)> {
            Err(CoordinatorError::NamespaceNotFound)
        }

        async fn lookup_track(
            &self,
            _scope: Option<&str>,
            _namespace: &TrackNamespace,
            track: &str,
        ) -> CoordinatorResult<(NamespaceOrigin, Option<moq_native_ietf::quic::Client>)> {
            self.track_lookups
                .lock()
                .map_err(|_| anyhow::anyhow!("track lookup lock poisoned"))?
                .push(track.to_string());
            Err(CoordinatorError::NamespaceNotFound)
        }
    }

    #[async_trait]
    impl Coordinator for RemoteCoordinator {
        async fn register_namespace(
            &self,
            _scope: Option<&str>,
            _namespace: &TrackNamespace,
            _context: &CoordinatorContext,
        ) -> CoordinatorResult<NamespaceRegistration> {
            Ok(NamespaceRegistration::new(()))
        }

        async fn unregister_namespace(
            &self,
            _scope: Option<&str>,
            _namespace: &TrackNamespace,
        ) -> CoordinatorResult<()> {
            Ok(())
        }

        async fn lookup(
            &self,
            _scope: Option<&str>,
            namespace: &TrackNamespace,
        ) -> CoordinatorResult<(NamespaceOrigin, Option<moq_native_ietf::quic::Client>)> {
            Ok((
                NamespaceOrigin::new(namespace.clone(), self.url.clone(), None),
                None,
            ))
        }
    }

    async fn loopback_webtransport_pair() -> (web_transport::Session, web_transport::Session) {
        use web_transport::quinn::{ClientBuilder, ServerBuilder};

        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("generate self-signed certificate");
        let cert_der = cert.cert.der().clone();
        let key = cert
            .key_pair
            .serialize_der()
            .try_into()
            .expect("serialize private key");
        let mut server = ServerBuilder::new()
            .with_addr("127.0.0.1:0".parse().expect("server addr"))
            .with_certificate(vec![cert_der], key)
            .expect("build loopback server");
        let addr = server.local_addr().expect("server local addr");

        let accepted = tokio::spawn(async move {
            server
                .accept()
                .await
                .expect("accept loopback request")
                .ok()
                .await
                .expect("accept loopback session")
        });
        let client = ClientBuilder::new()
            .dangerous()
            .with_no_certificate_verification()
            .expect("build loopback client");
        let url = url::Url::parse(&format!("https://127.0.0.1:{}/", addr.port()))
            .expect("parse loopback URL");
        let connected = tokio::time::timeout(Duration::from_secs(10), client.connect(url))
            .await
            .expect("loopback connect timed out")
            .expect("connect loopback session");

        (
            tokio::time::timeout(Duration::from_secs(10), accepted)
                .await
                .expect("loopback accept timed out")
                .expect("join loopback accept")
                .into(),
            connected.into(),
        )
    }

    async fn spawn_relay_session(
        locals: Locals,
        remotes: RemoteManager,
        coordinator: Arc<dyn Coordinator>,
    ) -> Subscriber {
        let (server_transport, client_transport) = loopback_webtransport_pair().await;
        let (server, client) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                TransportSession::accept(server_transport, None, Transport::WebTransport),
                TransportSession::connect(client_transport, None, Transport::WebTransport),
            )
        })
        .await
        .expect("MoQT setup timed out");
        let (server, publisher, _server_subscriber) = server.expect("accept MoQT session");
        let (client, _client_publisher, subscriber) = client.expect("connect MoQT session");
        let producer = Producer::new(
            publisher.expect("server publisher"),
            locals,
            remotes,
            coordinator,
            SessionContext::public(None),
        );

        tokio::spawn(async move { server.run().await });
        tokio::spawn(async move { client.run().await });
        tokio::spawn(async move { producer.run().await });

        subscriber
    }

    async fn subscribe_result(
        subscriber: &mut Subscriber,
        track_name: &str,
        rendezvous_timeout: Option<u64>,
    ) -> Result<moq_transport::session::Subscribe, ServeError> {
        let namespace = TrackNamespace::from_utf8_path("admission");
        let (writer, _reader) = Track::new(namespace, track_name).produce();
        let mut params = KeyValuePairs::default();
        if let Some(timeout) = rendezvous_timeout {
            params.set_rendezvous_timeout(timeout);
        }
        subscriber.subscribe_open_with_params(writer, params).await
    }

    async fn rejected_subscribe(
        subscriber: &mut Subscriber,
        track_name: &str,
        rendezvous_timeout: Option<u64>,
    ) -> ServeError {
        match subscribe_result(subscriber, track_name, rendezvous_timeout).await {
            Ok(_) => panic!("expected {track_name} subscription to be rejected"),
            Err(err) => err,
        }
    }

    /// draft-18 §10.2.6: absent and 0 both mean answer immediately, so neither
    /// produces a hold.
    #[test]
    fn no_hold_without_a_rendezvous_request() {
        let max = Duration::from_secs(30);

        assert_eq!(rendezvous_hold(None, max), None);
        assert_eq!(rendezvous_hold(Some(0), max), None);
    }

    #[test]
    fn a_request_within_the_ceiling_is_honoured() {
        assert_eq!(
            rendezvous_hold(Some(5_000), Duration::from_secs(30)),
            Some(Duration::from_secs(5))
        );
    }

    /// The spec lets a relay shorten the window without telling the subscriber,
    /// which is what stops one client pinning state for as long as it likes.
    #[test]
    fn a_request_above_the_ceiling_is_clamped() {
        assert_eq!(
            rendezvous_hold(Some(600_000), Duration::from_secs(30)),
            Some(Duration::from_secs(30))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn relay_admission_serves_available_tracks_before_rejecting_new_holds() {
        let locals = Locals::with_rendezvous_capacity(1);
        let coordinator = Arc::new(CountingCoordinator::default());
        let remotes = RemoteManager::new(coordinator.clone(), Vec::new());
        let mut first_subscriber =
            spawn_relay_session(locals.clone(), remotes.clone(), coordinator.clone()).await;
        let mut second_subscriber =
            spawn_relay_session(locals.clone(), remotes, coordinator.clone()).await;

        let first = tokio::spawn(async move {
            subscribe_result(&mut first_subscriber, "held", Some(10_000)).await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !coordinator.looked_up("held") || locals.available_rendezvous_holds() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first request did not enter rendezvous routing");

        let namespace = TrackNamespace::from_utf8_path("admission");
        let (_available_writer, available_reader) =
            Track::new(namespace.clone(), "available").produce();
        let mut registering_locals = locals.clone();
        let _available_registration = registering_locals
            .register_track(None, available_reader)
            .await
            .expect("register available track");
        let available = subscribe_result(&mut second_subscriber, "available", Some(10_000))
            .await
            .expect("capacity must not reject an available track");
        drop(available);

        let absent = rejected_subscribe(&mut second_subscriber, "absent", None).await;
        assert_eq!(
            absent,
            ServeError::Closed(RequestErrorCode::ExcessiveLoad as u64)
        );
        assert!(!coordinator.looked_up("absent"));

        let zero = rejected_subscribe(&mut second_subscriber, "zero", Some(0)).await;
        assert_eq!(
            zero,
            ServeError::Closed(RequestErrorCode::ExcessiveLoad as u64)
        );
        assert!(!coordinator.looked_up("zero"));

        let overloaded =
            rejected_subscribe(&mut second_subscriber, "overloaded", Some(10_000)).await;
        assert_eq!(
            overloaded,
            ServeError::Closed(RequestErrorCode::ExcessiveLoad as u64)
        );
        assert!(!coordinator.looked_up("overloaded"));

        let (_held_writer, held_reader) = Track::new(namespace.clone(), "held").produce();
        let _held_registration = registering_locals
            .register_track(None, held_reader)
            .await
            .expect("register held track");
        let first_subscription = tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .expect("held request did not resolve")
            .expect("join held request")
            .expect("held request was rejected");

        let after_release =
            rejected_subscribe(&mut second_subscriber, "after-release", Some(25)).await;
        assert_eq!(
            after_release,
            ServeError::Timeout,
            "resolved hold should release capacity for another rendezvous"
        );
        drop(first_subscription);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unresolved_pull_through_obeys_capacity_and_releases_on_timeout() {
        let locals = Locals::with_rendezvous_capacity(1);
        let namespace = TrackNamespace::from_utf8_path("admission");
        let (registration, mut requests) = locals
            .clone()
            .register_namespace(None, namespace)
            .await
            .expect("register namespace source");
        let coordinator = Arc::new(CountingCoordinator::default());
        let remotes = RemoteManager::new(coordinator.clone(), Vec::new());
        let mut first_subscriber =
            spawn_relay_session(locals.clone(), remotes.clone(), coordinator.clone()).await;
        let mut second_subscriber = spawn_relay_session(locals.clone(), remotes, coordinator).await;

        let requests_task = tokio::spawn(async move {
            while let Some(TrackRequest {
                writer,
                lease,
                upstream,
            }) = requests.recv().await
            {
                tokio::spawn(async move {
                    lease.abandoned().await;
                    drop(writer);
                    drop(upstream);
                });
            }
        });

        let first = tokio::spawn(async move {
            rejected_subscribe(&mut first_subscriber, "first-pending", Some(100)).await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while locals.available_rendezvous_holds() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first pull-through did not acquire rendezvous capacity");

        let overloaded =
            rejected_subscribe(&mut second_subscriber, "overloaded-pending", Some(1_000)).await;
        assert_eq!(
            overloaded,
            ServeError::Closed(RequestErrorCode::ExcessiveLoad as u64)
        );

        assert_eq!(
            first.await.expect("join first pull-through"),
            ServeError::Timeout
        );
        let after_timeout =
            rejected_subscribe(&mut second_subscriber, "after-timeout", Some(25)).await;
        assert_eq!(after_timeout, ServeError::Timeout);

        drop(registration);
        requests_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_peer_rejection_is_forwarded_with_and_without_rendezvous() {
        let (upstream_server_transport, upstream_client_transport) =
            loopback_webtransport_pair().await;
        let (upstream_server, upstream_client) = tokio::join!(
            TransportSession::accept(upstream_server_transport, None, Transport::WebTransport),
            TransportSession::connect(upstream_client_transport, None, Transport::WebTransport),
        );
        let (upstream_server, mut upstream_publisher, _) =
            upstream_server.expect("accept upstream MoQT session");
        let (upstream_client, remote_publisher, remote_subscriber) =
            upstream_client.expect("connect upstream MoQT session");
        let remote_url = url::Url::parse("https://relay.example.com/live").unwrap();
        let coordinator = Arc::new(RemoteCoordinator {
            url: remote_url.clone(),
        });
        let remotes = RemoteManager::new(coordinator.clone(), Vec::new());
        remotes
            .insert_test_remote(remote_url, remote_publisher, remote_subscriber)
            .await;

        let upstream_server_task = tokio::spawn(upstream_server.run());
        let upstream_client_task = tokio::spawn(upstream_client.run());
        let reject = tokio::spawn(async move {
            let publisher = upstream_publisher
                .as_mut()
                .expect("upstream server publisher");
            for _ in 0..2 {
                let subscribed = publisher
                    .subscribed()
                    .await
                    .expect("receive upstream subscribe");
                subscribed
                    .close(ServeError::Closed(RequestErrorCode::Unauthorized as u64))
                    .expect("reject upstream subscribe");
            }
        });

        let locals = Locals::new();
        let mut subscriber =
            spawn_relay_session(locals, remotes, coordinator as Arc<dyn Coordinator>).await;

        for rendezvous_timeout in [None, Some(1_000)] {
            assert_eq!(
                rejected_subscribe(&mut subscriber, "unauthorized", rendezvous_timeout).await,
                ServeError::Closed(RequestErrorCode::Unauthorized as u64)
            );
        }

        reject.await.expect("join upstream rejection task");
        upstream_server_task.abort();
        upstream_client_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_remote_subscribe_tears_down_before_same_track_retry() {
        let (upstream_server_transport, upstream_client_transport) =
            loopback_webtransport_pair().await;
        let (upstream_server, upstream_client) = tokio::join!(
            TransportSession::accept(upstream_server_transport, None, Transport::WebTransport),
            TransportSession::connect(upstream_client_transport, None, Transport::WebTransport),
        );
        let (upstream_server, mut upstream_publisher, _) =
            upstream_server.expect("accept upstream MoQT session");
        let (upstream_client, remote_publisher, remote_subscriber) =
            upstream_client.expect("connect upstream MoQT session");
        let remote_url = url::Url::parse("https://relay.example.com/live").unwrap();
        let coordinator = Arc::new(RemoteCoordinator {
            url: remote_url.clone(),
        });
        let remotes = RemoteManager::new(coordinator.clone(), Vec::new());
        remotes
            .insert_test_remote(remote_url, remote_publisher, remote_subscriber)
            .await;

        let upstream_server_task = tokio::spawn(upstream_server.run());
        let upstream_client_task = tokio::spawn(upstream_client.run());
        let locals = Locals::new();
        let subscriber =
            spawn_relay_session(locals, remotes, coordinator as Arc<dyn Coordinator>).await;
        let namespace = TrackNamespace::from_utf8_path("admission");

        let (first_writer, _first_reader) = Track::new(namespace.clone(), "retry").produce();
        let mut first_subscriber = subscriber.clone();
        let first = tokio::spawn(async move {
            let mut params = KeyValuePairs::default();
            params.set_rendezvous_timeout(30_000);
            first_subscriber
                .subscribe_open_with_params(first_writer, params)
                .await
        });
        let first_upstream = tokio::time::timeout(
            Duration::from_secs(2),
            upstream_publisher
                .as_mut()
                .expect("upstream server publisher")
                .subscribed(),
        )
        .await
        .expect("first upstream subscribe timed out")
        .expect("first upstream subscribe");

        first.abort();
        match first.await {
            Err(err) => assert!(err.is_cancelled()),
            Ok(_) => panic!("first subscribe was not cancelled"),
        }

        let mut retry_subscriber = subscriber.clone();
        let retry_namespace = namespace.clone();
        let retry = tokio::spawn(async move {
            let (writer, _reader) = Track::new(retry_namespace, "retry").produce();
            let mut params = KeyValuePairs::default();
            params.set_rendezvous_timeout(30_000);
            retry_subscriber
                .subscribe_open_with_params(writer, params)
                .await
        });

        let publisher = upstream_publisher
            .as_mut()
            .expect("upstream server publisher");
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                biased;
                _ = first_upstream.closed() => {}
                _ = publisher.subscribed() => {
                    panic!("same-track retry overtook the first upstream teardown")
                }
            }
        })
        .await
        .expect("first upstream teardown did not complete");
        drop(first_upstream);

        let second_upstream = tokio::time::timeout(Duration::from_secs(2), publisher.subscribed())
            .await
            .expect("second upstream subscribe timed out")
            .expect("second upstream subscribe");
        second_upstream
            .close(ServeError::Closed(RequestErrorCode::Uninterested as u64))
            .expect("reject second upstream subscribe");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), retry)
                .await
                .expect("downstream retry timed out")
                .expect("join downstream retry"),
            Err(ServeError::Closed(code)) if code == RequestErrorCode::Uninterested as u64
        ));

        upstream_server_task.abort();
        upstream_client_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_session_cannot_monopolize_relay_capacity() {
        let locals = Locals::new();
        let coordinator = Arc::new(CountingCoordinator::default());
        let remotes = RemoteManager::new(coordinator.clone(), Vec::new());
        let mut saturated_session =
            spawn_relay_session(locals.clone(), remotes.clone(), coordinator.clone()).await;
        let mut other_session = spawn_relay_session(locals, remotes, coordinator.clone()).await;

        let mut holds = Vec::new();
        for index in 0..MAX_CONCURRENT_RENDEZVOUS_HOLDS_PER_SESSION {
            let mut subscriber = saturated_session.clone();
            let track = format!("session-hold-{index}");
            holds.push(tokio::spawn(async move {
                subscribe_result(&mut subscriber, &track, Some(10_000)).await
            }));
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while coordinator.distinct_lookup_count("session-hold-")
                < MAX_CONCURRENT_RENDEZVOUS_HOLDS_PER_SESSION
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("session did not fill its rendezvous capacity");

        let overloaded = rejected_subscribe(
            &mut saturated_session,
            "same-session-overloaded",
            Some(10_000),
        )
        .await;
        assert_eq!(
            overloaded,
            ServeError::Closed(RequestErrorCode::ExcessiveLoad as u64)
        );
        assert!(!coordinator.looked_up("same-session-overloaded"));

        let other_hold = tokio::spawn(async move {
            subscribe_result(&mut other_session, "other-session-hold", Some(10_000)).await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !coordinator.looked_up("other-session-hold") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("saturated session blocked another session");

        other_hold.abort();
        for hold in holds {
            hold.abort();
        }
    }

    /// Locals routes a track to the longest registered namespace that prefixes
    /// it, so a shorter announcement still makes the track servable and has to
    /// wake the waiter.
    #[test]
    fn a_prefix_announcement_covers_the_track_namespace() {
        let target = TrackNamespace::from_utf8_path("meeting/room/42");

        assert!(namespace_covers(
            &TrackNamespace::from_utf8_path("meeting"),
            &target
        ));
        assert!(namespace_covers(
            &TrackNamespace::from_utf8_path("meeting/room/42"),
            &target
        ));
    }

    #[test]
    fn an_unrelated_or_longer_announcement_does_not_cover_it() {
        let target = TrackNamespace::from_utf8_path("meeting/room/42");

        assert!(!namespace_covers(
            &TrackNamespace::from_utf8_path("other/room"),
            &target
        ));
        // Longer than the target cannot be a prefix of it.
        assert!(!namespace_covers(
            &TrackNamespace::from_utf8_path("meeting/room/42/sub"),
            &target
        ));
    }

    #[test]
    fn expected_serve_shutdown_accepts_wrapped_session_errors() {
        assert!(Producer::is_expected_serve_shutdown(&anyhow::Error::new(
            SessionError::Serve(ServeError::Cancel)
        )));
        assert!(Producer::is_expected_serve_shutdown(&anyhow::Error::new(
            SessionError::Serve(ServeError::Done)
        )));
        assert!(!Producer::is_expected_serve_shutdown(&anyhow::Error::new(
            SessionError::Serve(ServeError::NotFound)
        )));
    }

    #[test]
    fn expected_serve_shutdown_accepts_direct_serve_errors() {
        assert!(Producer::is_expected_serve_shutdown(&anyhow::Error::new(
            ServeError::Cancel
        )));
        assert!(Producer::is_expected_serve_shutdown(&anyhow::Error::new(
            ServeError::Done
        )));
        assert!(!Producer::is_expected_serve_shutdown(&anyhow::Error::new(
            ServeError::NotFound
        )));
    }
}
