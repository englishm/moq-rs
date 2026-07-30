// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use moq_transport::{
    serve::{ServeError, TracksReader},
    session::{Publisher, SessionError, Subscribed, TrackStatusRequested},
};

use crate::{
    metrics::{GaugeGuard, TimingGuard},
    Locals, RemoteManager,
};

/// Producer of tracks to a remote Subscriber
#[derive(Clone)]
pub struct Producer {
    publisher: Publisher,
    locals: Locals,
    remotes: RemoteManager,
    /// The resolved scope identity for this session, if any.
    /// Produced by `Coordinator::resolve_scope()` from the connection path.
    /// Passed to locals/remotes to isolate namespace lookups.
    scope: Option<String>,
}

impl Producer {
    pub fn new(
        publisher: Publisher,
        locals: Locals,
        remotes: RemoteManager,
        scope: Option<String>,
    ) -> Self {
        Self {
            publisher,
            locals,
            remotes,
            scope,
        }
    }

    /// Announce new tracks to the remote server.
    pub async fn announce(&mut self, tracks: TracksReader) -> Result<(), SessionError> {
        self.publisher.announce(tracks).await
    }

    /// Run the producer to serve subscribe requests.
    pub async fn run(self) -> Result<(), SessionError> {
        let mut tasks: FuturesUnordered<futures::future::BoxFuture<'static, ()>> =
            FuturesUnordered::new();

        loop {
            let mut publisher_subscribed = self.publisher.clone();
            let mut publisher_track_status = self.publisher.clone();

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
                            tracing::warn!(namespace = %namespace, track = %track_name, error = %err, "failed serving subscribe: {:?}, error: {}", info, err);
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

        // Check local tracks first, and serve from local if possible
        if let Some(mut local) = self.locals.retrieve(self.scope.as_deref(), &namespace) {
            // Pass the full requested namespace, not the announced prefix
            if let Some((track, interest)) = local.subscribe(namespace.clone(), &track_name) {
                let ns = namespace.to_utf8_path();
                tracing::info!(namespace = %ns, track = %track_name, source = "local", "serving subscribe from local: {:?}", track.info);
                // Update label to indicate local source, timing recorded on drop
                timing_guard.set_label("source", "local");
                // Track active tracks - decrements when serve completes
                let _track_guard = GaugeGuard::new("moq_relay_active_tracks");
                // Hold the downstream-interest guard for as long as we are
                // serving, so the cache entry backing this reader is not evicted
                // (and its upstream subscription released) underneath us.
                let _interest = interest;
                return Ok(subscribed.serve(track).await?);
            }
        }

        // Check remote tracks second, and serve from remote if possible
        match self
            .remotes
            .subscribe(self.scope.as_deref(), &namespace, &track_name)
            .await
        {
            Ok(track) => {
                if let Some((track, interest)) = track {
                    let ns = namespace.to_utf8_path();
                    tracing::info!(namespace = %ns, track = %track_name, source = "remote", "serving subscribe from remote: {:?}", track.info);
                    // Update label to indicate remote source, timing recorded on drop
                    timing_guard.set_label("source", "remote");
                    // Track active tracks - decrements when serve completes
                    let _track_guard = GaugeGuard::new("moq_relay_active_tracks");
                    // Hold the downstream-interest guard for as long as we are
                    // serving, so the cross-relay cache entry backing this reader
                    // is not evicted (and its peer subscription released) while
                    // we are still forwarding from it.
                    let _interest = interest;
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

    /// Serve a track_status request.
    async fn serve_track_status(
        self,
        mut track_status_requested: TrackStatusRequested,
    ) -> Result<(), anyhow::Error> {
        // Check local tracks first, and serve from local if possible
        if let Some(mut local_tracks) = self.locals.retrieve(
            self.scope.as_deref(),
            &track_status_requested.request_msg.track_namespace,
        ) {
            if let Some(track) = local_tracks.get_track_reader(
                &track_status_requested.request_msg.track_namespace,
                &track_status_requested.request_msg.track_name,
            ) {
                let namespace = track_status_requested
                    .request_msg
                    .track_namespace
                    .to_utf8_path();
                let track_name = &track_status_requested.request_msg.track_name;
                tracing::info!(namespace = %namespace, track = %track_name, source = "local", "serving track_status from local: {:?}", track.info);
                return Ok(track_status_requested.respond_ok(&track)?);
            }
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

        track_status_requested.respond_error(4, "Track not found")?;

        Err(ServeError::not_found_ctx(format!(
            "track '{}/{}' not found for track_status",
            track_status_requested.request_msg.track_namespace,
            track_status_requested.request_msg.track_name
        ))
        .into())
    }
}
