// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;

use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_transport::{
    coding::{KeyValuePairs, TrackNamespace, TrackNamespacePrefix},
    message::SubscribeOptions,
    serve::{FullTrackName, ServeError, TrackReader, TracksReader},
    session::{Publisher, SessionError, Subscribed, SubscribedNamespace, TrackStatusRequested},
};
use tokio::sync::broadcast;

use crate::auth::{authorize, AuthzOperation, DenyReason, SessionAuth};
use crate::{
    metrics::{GaugeGuard, TimingGuard},
    upstream_namespaces::UpstreamNamespaces,
    Locals, NamespaceChange, RemoteManager, SessionContext, TrackChange, UpstreamReady,
};

/// Producer of tracks to a remote Subscriber
#[derive(Clone)]
pub struct Producer {
    publisher: Publisher,
    locals: Locals,
    remotes: RemoteManager,
    upstream_namespaces: UpstreamNamespaces,
    /// Relay-level context for this MoQT session.
    context: SessionContext,
    /// Authorization state for this session. `None` when the scope has no
    /// authorization policy, in which case every request is permitted.
    auth: Option<SessionAuth>,
}

/// Why the wait for upstream readiness ended without the subscription being
/// established.
///
/// The two cases carry the same [`ServeError`] variants — an upstream rejection
/// can be `Cancel` or `Done` just as a departing subscriber can — so the
/// direction has to come from which branch resolved rather than from the error.
/// Getting it wrong misreports publisher failures as subscriber cancellations in
/// the subscribe metrics.
enum UpstreamWait {
    /// The upstream subscription could not be established.
    UpstreamFailed(ServeError),

    /// The downstream subscriber went away before it was established.
    DownstreamLeft(ServeError),
}

impl Producer {
    /// Create a producer for a session.
    ///
    /// `auth` is the session's authorization state, and is deliberately a
    /// required argument rather than something attached afterwards: a producer
    /// with no authorization serves every request, so forgetting to supply it
    /// must be a compile error rather than a silent grant. `None` states that
    /// the session needs no authorization — its scope has no policy, or the
    /// relay dialled the peer itself.
    pub(crate) fn new_with_upstream_namespaces(
        publisher: Publisher,
        locals: Locals,
        remotes: RemoteManager,
        upstream_namespaces: UpstreamNamespaces,
        context: SessionContext,
        auth: Option<SessionAuth>,
    ) -> Self {
        Self {
            publisher,
            locals,
            remotes,
            upstream_namespaces,
            context,
            auth,
        }
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

        // Authorize before any lookup: deciding after would let response
        // timing reveal whether a track the peer may not access exists.
        if let Err(reason) = authorize(
            self.auth.as_ref(),
            &self.context,
            AuthzOperation::Subscribe {
                namespace: &namespace,
                track: &track_name,
            },
            Some(subscribed.id),
        )
        .await
        {
            metrics::counter!("moq_relay_subscribe_errors_total", "phase" => "auth").increment(1);
            timing_guard.set_label("source", "unauthorized");
            let err = ServeError::Closed(reason.request_error_code());
            subscribed.close(err.clone())?;
            return Err(err.into());
        }

        // Local lookup order inside Locals:
        // 1. actual FullTrackName -> TrackReader media cache
        // 2. PUBLISH_NAMESPACE route source, which triggers upstream SUBSCRIBE
        let mut locals = self.locals.clone();
        if let Some(local) = locals
            .get_or_request_track(self.context.scope(), namespace.clone(), &track_name)
            .await
        {
            let ns = namespace.to_utf8_path();
            tracing::info!(namespace = %ns, track = %track_name, source = "local", "serving subscribe from local: {:?}", local.reader.info);
            timing_guard.set_label("source", "local");
            let _track_guard = GaugeGuard::new("moq_relay_active_tracks");
            // Held until serving finishes. Once the last guard for a cached track
            // drops, its upstream subscription becomes eligible for release.
            let _interest_guard = local.interest;

            // Draft-16 §8.4: a relay MUST have an Established upstream
            // subscription before it sends SUBSCRIBE_OK. A pull-through cache
            // entry exists before its upstream subscription does, so wait for it.
            if let Some(upstream) = local.upstream {
                if let Err(outcome) = Self::await_upstream(&subscribed, &upstream).await {
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
                    };

                    // Rejects when the subscription is already closed (the
                    // downstream-left case), which is fine: the error below is
                    // still the reason we stopped.
                    let _ = subscribed.close(err.clone());
                    return Err(err.into());
                }
            }

            return Ok(subscribed.serve(local.reader).await?);
        }

        // Check remote tracks after local exact tracks and namespace route sources.
        match self
            .remotes
            .subscribe(self.context.scope(), &namespace, &track_name)
            .await
        {
            Ok(track) => {
                if let Some((track, interest_guard)) = track {
                    let ns = namespace.to_utf8_path();
                    tracing::info!(namespace = %ns, track = %track_name, source = "remote", "serving subscribe from remote: {:?}", track.info);
                    // Update label to indicate remote source, timing recorded on drop
                    timing_guard.set_label("source", "remote");
                    // Track active tracks - decrements when serve completes
                    let _track_guard = GaugeGuard::new("moq_relay_active_tracks");
                    // Held until serving finishes; the cross-relay subscription is
                    // released once the last guard for this track drops.
                    let _interest_guard = interest_guard;
                    return Ok(subscribed.serve(track).await?);
                }
            }
            Err(e) => {
                // Route error = infrastructure failure (couldn't reach coordinator/upstream)
                // This is different from "not found" - we don't know if the track exists
                let ns = namespace.to_utf8_path();
                tracing::error!(namespace = %ns, track = %track_name, error = %e, "failed to route to remote: {}", e);
                timing_guard.set_label("source", "route_error");
                metrics::counter!("moq_relay_subscribe_route_errors_total").increment(1);

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

        // Track not found - we checked all sources and the track doesn't exist
        // timing_guard label already set to "not_found", will record on drop
        metrics::counter!("moq_relay_subscribe_not_found_total").increment(1);

        let err = ServeError::not_found_ctx(format!(
            "track '{}/{}' not found in local or remote tracks",
            namespace, track_name
        ));
        subscribed.close(err.clone())?;
        Err(err.into())
    }

    /// Wait for the upstream subscription behind a cached track to be established.
    ///
    /// Also completes when the downstream subscriber goes away first, so a
    /// cancelled SUBSCRIBE is not held here for the full upstream response
    /// timeout. The two cases are reported separately because they cannot be
    /// told apart from the error alone — see [`UpstreamWait`].
    async fn await_upstream(
        subscribed: &Subscribed,
        upstream: &UpstreamReady,
    ) -> Result<(), UpstreamWait> {
        tokio::select! {
            res = upstream.established() => res.map_err(UpstreamWait::UpstreamFailed),
            res = subscribed.closed() => Err(UpstreamWait::DownstreamLeft(
                res.err().unwrap_or(ServeError::Done),
            )),
        }
    }

    /// Serve a SUBSCRIBE_NAMESPACE request using relay-local namespace state.
    async fn serve_subscribe_namespace(
        self,
        mut subscribed_namespace: SubscribedNamespace,
    ) -> Result<(), anyhow::Error> {
        // Authorize before taking the upstream lease: an unauthorized prefix
        // must not cause the relay to open upstream subscriptions.
        if let Err(reason) = authorize(
            self.auth.as_ref(),
            &self.context,
            AuthzOperation::SubscribeNamespace {
                prefix: &subscribed_namespace.namespace_prefix,
            },
            Some(subscribed_namespace.info.request_id),
        )
        .await
        {
            metrics::counter!("moq_relay_subscribe_namespace_errors_total", "phase" => "auth")
                .increment(1);
            subscribed_namespace.reject(reason.request_error_code(), "unauthorized")?;
            return Err(anyhow::anyhow!("unauthorized subscribe_namespace"));
        }

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
            self.send_namespace_snapshot(&mut subscribed_namespace, &mut known_namespaces)
                .await?;
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
                            self.apply_namespace_change(&mut subscribed_namespace, &mut known_namespaces, change).await?;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            // Recoverable: a full resync reconstructs the state the
                            // skipped events would have produced. Counted so that
                            // sustained churn outgrowing the channel capacity is
                            // visible before it shows up as latency.
                            metrics::counter!("moq_relay_change_channel_lagged_total", "channel" => "namespace")
                                .increment(skipped);
                            self.resync_namespaces(&mut subscribed_namespace, &mut known_namespaces).await?;
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

    /// Whether the peer may be told that `namespace` exists.
    async fn may_announce(&self, namespace: &TrackNamespace) -> bool {
        may_announce_namespace(self.auth.as_ref(), &self.context, namespace).await
    }

    async fn send_namespace_snapshot(
        &self,
        subscribed_namespace: &mut SubscribedNamespace,
        known: &mut HashSet<TrackNamespace>,
    ) -> Result<(), ServeError> {
        for namespace in self
            .locals
            .list_namespaces_matching(self.context.scope(), &subscribed_namespace.namespace_prefix)
        {
            if !self.may_announce(&namespace).await {
                continue;
            }
            if known.insert(namespace.clone()) {
                subscribed_namespace.namespace(&namespace)?;
            }
        }

        Ok(())
    }

    async fn apply_namespace_change(
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
            if !self.may_announce(&change.namespace).await {
                return Ok(());
            }
            if known.insert(change.namespace.clone()) {
                subscribed_namespace.namespace(&change.namespace)?;
            }
        } else if known.remove(&change.namespace) {
            subscribed_namespace.namespace_done(&change.namespace)?;
        }

        Ok(())
    }

    async fn resync_namespaces(
        &self,
        subscribed_namespace: &mut SubscribedNamespace,
        known: &mut HashSet<TrackNamespace>,
    ) -> Result<(), ServeError> {
        let mut current = HashSet::new();
        for namespace in self
            .locals
            .list_namespaces_matching(self.context.scope(), &subscribed_namespace.namespace_prefix)
        {
            if self.may_announce(&namespace).await {
                current.insert(namespace);
            }
        }

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

        // SUBSCRIBE_NAMESPACE grants discovery of a prefix, not delivery of
        // everything under it. This path pushes PUBLISH and then streams the
        // track's objects, so it needs the same authorization a SUBSCRIBE for
        // that track would: a token scoped to a prefix for discovery, or to a
        // subset of track names, must not receive media outside that grant.
        //
        // A denial skips the track rather than failing the subscription: the
        // peer asked for a prefix, not for this track, so the rest of the
        // prefix is still legitimately theirs.
        if let Err(reason) = may_serve_track_in_fanout(
            self.auth.as_ref(),
            &self.context,
            &full_name,
            subscribed_namespace.info.request_id,
        )
        .await
        {
            tracing::debug!(
                namespace = %full_name.namespace,
                track = %full_name.name,
                reason = %reason,
                "withholding track from SUBSCRIBE_NAMESPACE fan-out"
            );
            metrics::counter!("moq_relay_publish_errors_total", "phase" => "auth_fanout")
                .increment(1);

            // A policy denial is stable for the session, so record it and stop
            // re-deciding on every change event. A hook *fault* is not: it says
            // nothing about this track, so leave it out of `known` and let the
            // next event retry rather than withholding the track for the life
            // of the subscription over one transient failure.
            if reason.is_policy_denial() {
                known.insert(full_name);
            }
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

        // Authorize before the lookup: TRACK_STATUS is an existence oracle, so
        // answering it for an unauthorized track leaks exactly what the token
        // scope is meant to hide.
        if let Err(reason) = authorize(
            self.auth.as_ref(),
            &self.context,
            AuthzOperation::TrackStatus {
                namespace: &full_name.namespace,
                track: &full_name.name,
            },
            Some(track_status_requested.request_msg.id),
        )
        .await
        {
            metrics::counter!("moq_relay_track_status_errors_total", "phase" => "auth")
                .increment(1);
            track_status_requested.respond_error(reason.request_error_code(), "unauthorized")?;
            return Err(anyhow::anyhow!("unauthorized track_status"));
        }

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

/// Whether a track may be delivered through the SUBSCRIBE_NAMESPACE fan-out.
///
/// That path pushes PUBLISH and then streams the track's objects, so it needs
/// the authorization a SUBSCRIBE for the same track would: a prefix
/// subscription grants discovery, not delivery of everything beneath it. A
/// token scoped to a subset of track names must not receive the rest merely
/// because it subscribed to the enclosing prefix.
///
/// A free function so the decision is testable without a live session, which
/// `Producer` requires: a `Publisher` cannot be constructed outside
/// `moq-transport`, so the gate could not otherwise be covered at all.
///
/// The tests therefore pin this decision and the operation it asks about, but
/// not that the caller still consults it — removing the call site produces a
/// dead-code warning rather than a failing test. Closing that would need an
/// end-to-end session harness.
async fn may_serve_track_in_fanout(
    auth: Option<&SessionAuth>,
    context: &SessionContext,
    full_name: &FullTrackName,
    request_id: u64,
) -> Result<(), DenyReason> {
    authorize(
        auth,
        context,
        AuthzOperation::Subscribe {
            namespace: &full_name.namespace,
            track: &full_name.name,
        },
        Some(request_id),
    )
    .await
}

/// Whether the peer may be told that `namespace` exists.
///
/// A prefix subscription is not a grant over everything beneath it. The `nil`
/// terminator makes the distinction concrete: a scope of
/// `[exact("sports"), nil]` authorizes SUBSCRIBE_NAMESPACE for `sports` while
/// denying every operation under `sports/football`, so announcing that
/// namespace would disclose the existence of something the token cannot touch.
/// The reasoning that gates the media fan-out applies to the metadata too.
async fn may_announce_namespace(
    auth: Option<&SessionAuth>,
    context: &SessionContext,
    namespace: &TrackNamespace,
) -> bool {
    // A concrete namespace viewed as the prefix naming exactly it.
    let prefix = TrackNamespacePrefix {
        fields: namespace.fields.clone(),
    };

    authorize(
        auth,
        context,
        AuthzOperation::SubscribeNamespace { prefix: &prefix },
        None,
    )
    .await
    .is_ok()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use moq_transport::{serve::ServeError, session::SessionError};

    use super::*;
    use crate::auth::{AuthDecision, AuthError, AuthHook, AuthRequest, AuthToken, Principal};

    /// Records every operation it is asked about, and answers from a script.
    ///
    /// Lets the fan-out gates be tested for the operation they construct, not
    /// merely for their yes/no answer: authorizing the wrong thing would be as
    /// much a bypass as authorizing nothing.
    struct RecordingHook {
        seen: Mutex<Vec<String>>,
        decision: fn() -> Result<AuthDecision, AuthError>,
    }

    impl RecordingHook {
        fn new(decision: fn() -> Result<AuthDecision, AuthError>) -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                decision,
            })
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AuthHook for RecordingHook {
        async fn on_setup(
            &self,
            _session: &SessionContext,
            _tokens: &[AuthToken],
        ) -> Result<AuthDecision, AuthError> {
            (self.decision)()
        }

        async fn on_request(&self, request: &AuthRequest<'_>) -> Result<AuthDecision, AuthError> {
            let detail = match &request.operation {
                AuthzOperation::Subscribe { namespace, track } => format!(
                    "subscribe {} {}",
                    namespace.to_utf8_path(),
                    track.to_string_lossy()
                ),
                AuthzOperation::SubscribeNamespace { prefix } => {
                    format!("subscribe_namespace {}", prefix.to_utf8_path())
                }
                other => format!("other {}", other.label()),
            };
            self.seen.lock().unwrap().push(detail);
            (self.decision)()
        }
    }

    fn allow() -> Result<AuthDecision, AuthError> {
        Ok(AuthDecision::allow(Principal::anonymous()))
    }

    fn deny() -> Result<AuthDecision, AuthError> {
        Ok(AuthDecision::deny(DenyReason::ScopeMismatch))
    }

    fn fault() -> Result<AuthDecision, AuthError> {
        Err(AuthError::Backend("backend unavailable".to_string()))
    }

    fn context() -> SessionContext {
        SessionContext::public(Some("tenant".to_string()))
    }

    fn session_auth(hook: Arc<RecordingHook>) -> SessionAuth {
        SessionAuth::new(hook, Principal::anonymous())
    }

    fn full_name() -> FullTrackName {
        FullTrackName {
            namespace: TrackNamespace::from_utf8_path("sports/football"),
            name: "premium-4k".into(),
        }
    }

    /// The fan-out streams media, so it must ask for SUBSCRIBE on the exact
    /// track — not for the enclosing prefix, and not nothing at all.
    #[tokio::test]
    async fn fanout_authorizes_each_track_as_a_subscribe() {
        let hook = RecordingHook::new(allow);
        let auth = session_auth(hook.clone());

        may_serve_track_in_fanout(Some(&auth), &context(), &full_name(), 7)
            .await
            .expect("allowed");

        assert_eq!(hook.seen(), vec!["subscribe /sports/football premium-4k"]);
    }

    #[tokio::test]
    async fn fanout_withholds_a_track_the_token_does_not_cover() {
        let hook = RecordingHook::new(deny);
        let auth = session_auth(hook.clone());

        let result = may_serve_track_in_fanout(Some(&auth), &context(), &full_name(), 7).await;

        assert!(result.is_err(), "a denied track must not be served");
        assert_eq!(hook.seen().len(), 1, "the decision must actually be sought");
    }

    /// A hook fault is not a statement about this track, so it must not be
    /// remembered as one; `publish_track_for_namespace` only caches denials
    /// that are policy decisions.
    #[tokio::test]
    async fn fanout_distinguishes_a_fault_from_a_denial() {
        let denied = may_serve_track_in_fanout(
            Some(&session_auth(RecordingHook::new(deny))),
            &context(),
            &full_name(),
            7,
        )
        .await
        .expect_err("denied");
        assert!(denied.is_policy_denial());

        let faulted = may_serve_track_in_fanout(
            Some(&session_auth(RecordingHook::new(fault))),
            &context(),
            &full_name(),
            7,
        )
        .await
        .expect_err("faulted");
        assert!(
            !faulted.is_policy_denial(),
            "a fault must not be cached as a denial"
        );
    }

    /// A session whose scope has no policy is unaffected.
    #[tokio::test]
    async fn fanout_permits_everything_when_no_policy_applies() {
        assert!(may_serve_track_in_fanout(None, &context(), &full_name(), 7)
            .await
            .is_ok());
        assert!(
            may_announce_namespace(
                None,
                &context(),
                &TrackNamespace::from_utf8_path("sports/football")
            )
            .await
        );
    }

    /// Announcements disclose existence, so each concrete namespace is
    /// authorized as the prefix naming exactly it.
    #[tokio::test]
    async fn announcements_are_authorized_per_namespace() {
        let hook = RecordingHook::new(allow);
        let auth = session_auth(hook.clone());
        let namespace = TrackNamespace::from_utf8_path("sports/football");

        assert!(may_announce_namespace(Some(&auth), &context(), &namespace).await);
        assert_eq!(hook.seen(), vec!["subscribe_namespace /sports/football"]);
    }

    #[tokio::test]
    async fn announcements_are_withheld_when_denied() {
        let hook = RecordingHook::new(deny);
        let auth = session_auth(hook.clone());
        let namespace = TrackNamespace::from_utf8_path("sports/football");

        assert!(
            !may_announce_namespace(Some(&auth), &context(), &namespace).await,
            "a namespace the token cannot touch must not be announced"
        );
        assert_eq!(hook.seen().len(), 1);
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
