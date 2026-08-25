// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Metrics instrumentation for moq-relay-ietf
//!
//! Metrics are always compiled in via the [`metrics`] crate facade. When no
//! recorder is installed the overhead is negligible (an atomic load + early
//! return per call site), similar to how the `log` crate works when no logger
//! is configured.
//!
//! To actually collect metrics, install a recorder at startup. The optional
//! `metrics-prometheus` feature adds a Prometheus exporter — see the binary
//! in `src/bin/moq-relay-ietf/main.rs` for an example.
//!
//! # Available Metrics
//!
//! All metrics are prefixed with `moq_relay_` to avoid collisions.
//!
//! ## Counters
//!
//! | Name | Labels | Description |
//! |------|--------|-------------|
//! | `moq_relay_connections_total` | - | Total incoming connections accepted |
//! | `moq_relay_connections_closed_total` | - | Total connections that have closed (graceful or error) |
//! | `moq_relay_connection_errors_total` | `stage` | Connection failures (stage: session_accept, session_run) |
//! | `moq_relay_publishers_total` | - | Total publishers (PUBLISH_NAMESPACE requests) received |
//! | `moq_relay_announce_ok_total` | - | Successful REQUEST_OK responses sent for PUBLISH_NAMESPACE |
//! | `moq_relay_announce_errors_total` | `phase` | PUBLISH_NAMESPACE failures (phase: coordinator_register, local_register, send_ok) |
//! | `moq_relay_subscribers_total` | - | Total subscribers (SUBSCRIBE requests) received |
//! | `moq_relay_subscribe_not_found_total` | - | Track not found after checking all sources |
//! | `moq_relay_subscribe_route_errors_total` | - | Infrastructure failure when routing to remote |
//! | `moq_relay_subscribe_upstream_errors_total` | - | Upstream subscription could not be established, so the downstream SUBSCRIBE was rejected |
//! | `moq_relay_upstream_errors_total` | `stage` | Upstream connection failures (stage: connect, session) |
//! | `moq_relay_namespace_transition_timeouts_total` | - | Namespace pull streams reset after graceful transition timeout |
//! | `moq_relay_cache_idle_evictions_total` | `source` | Unwatched cache entries evicted, releasing an upstream subscription (source: local, remote) |
//! | `moq_relay_remote_cache_evictions_total` | - | Peer relay connections closed to keep the connection pool within its limit |
//! | `moq_relay_change_channel_lagged_total` | `channel` | Change notifications skipped by a lagging receiver, forcing a resync (channel: namespace, track) |
//! | `moq_relay_lease_registry_lock_poisoned_total` | `operation` | Upstream namespace lease registry lock found poisoned (operation: acquire, release) |
//!
//! ## Gauges
//!
//! | Name | Description |
//! |------|-------------|
//! | `moq_relay_active_connections` | Current number of active client connections |
//! | `moq_relay_active_publishers` | Current number of active publishers |
//! | `moq_relay_active_subscriptions` | Current number of active subscriptions |
//! | `moq_relay_active_tracks` | Current number of tracks being served |
//! | `moq_relay_announced_namespaces` | Current number of namespaces registered via PUBLISH_NAMESPACE |
//! | `moq_relay_upstream_connections` | Current number of upstream/origin connections |
//!
//! ## Histograms
//!
//! | Name | Labels | Description |
//! |------|--------|-------------|
//! | `moq_relay_subscribe_latency_seconds` | `source` | Time to resolve subscription (source: local, remote, not_found, route_error, upstream_error, downstream_left) |

use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

// ============================================================================
// describe_metrics - Register metric descriptions for Prometheus HELP text
// ============================================================================

/// Register metric descriptions with the metrics recorder.
///
/// Call this once after installing a metrics recorder (e.g., Prometheus exporter).
/// The descriptions appear as `# HELP` comments in Prometheus output.
pub fn describe_metrics() {
    // Counters
    describe_counter!(
        "moq_relay_connections_total",
        "Total incoming connections accepted"
    );
    describe_counter!(
        "moq_relay_connections_closed_total",
        "Total connections that have closed (graceful or error)"
    );
    describe_counter!(
        "moq_relay_connection_errors_total",
        "Connection failures by stage (session_accept, session_run)"
    );
    describe_counter!(
        "moq_relay_publishers_total",
        "Total publishers (PUBLISH_NAMESPACE requests) received"
    );
    describe_counter!(
        "moq_relay_published_tracks_total",
        "Total publisher-initiated PUBLISH track requests received"
    );
    describe_counter!(
        "moq_relay_publish_errors_total",
        "Publisher-initiated PUBLISH failures by phase (take_reader, local_register, coordinator_register, send_ok)"
    );
    describe_counter!(
        "moq_relay_announce_ok_total",
        "Successful REQUEST_OK responses sent for PUBLISH_NAMESPACE"
    );
    describe_counter!(
        "moq_relay_announce_errors_total",
        "PUBLISH_NAMESPACE failures by phase (coordinator_register, local_register, send_ok)"
    );
    describe_counter!(
        "moq_relay_subscribers_total",
        "Total subscribers (SUBSCRIBE requests) received"
    );
    describe_counter!(
        "moq_relay_subscribe_not_found_total",
        "Track not found after checking all sources"
    );
    describe_counter!(
        "moq_relay_subscribe_route_errors_total",
        "Infrastructure failure when routing to remote"
    );
    describe_counter!(
        "moq_relay_subscribe_upstream_errors_total",
        "Upstream subscription could not be established, so the downstream SUBSCRIBE was rejected"
    );
    describe_counter!(
        "moq_relay_upstream_errors_total",
        "Upstream connection failures by stage (connect, session)"
    );
    describe_counter!(
        "moq_relay_namespace_transition_timeouts_total",
        "Namespace pull streams reset after graceful transition timeout"
    );
    describe_counter!(
        "moq_relay_cache_idle_evictions_total",
        "Unwatched cache entries evicted, releasing an upstream subscription, by source (local, remote)"
    );
    describe_counter!(
        "moq_relay_remote_cache_evictions_total",
        "Peer relay connections closed to keep the connection pool within its limit"
    );
    describe_counter!(
        "moq_relay_change_channel_lagged_total",
        "Change notifications skipped by a lagging receiver, forcing a resync, by channel (namespace, track)"
    );
    describe_counter!(
        "moq_relay_lease_registry_lock_poisoned_total",
        "Upstream namespace lease registry lock found poisoned by operation (acquire, release)"
    );

    // Gauges
    describe_gauge!(
        "moq_relay_active_connections",
        "Current number of active client connections"
    );
    describe_gauge!(
        "moq_relay_active_publishers",
        "Current number of active publishers"
    );
    describe_gauge!(
        "moq_relay_active_subscriptions",
        "Current number of active subscriptions"
    );
    describe_gauge!(
        "moq_relay_active_tracks",
        "Current number of tracks being served"
    );
    describe_gauge!(
        "moq_relay_active_published_tracks",
        "Current number of exact tracks registered from PUBLISH"
    );
    describe_gauge!(
        "moq_relay_announced_namespaces",
        "Current number of registered namespaces"
    );
    describe_gauge!(
        "moq_relay_upstream_connections",
        "Current number of upstream/origin connections"
    );

    // Histograms
    describe_histogram!(
        "moq_relay_subscribe_latency_seconds",
        Unit::Seconds,
        "Time to resolve subscription by source (local, remote, not_found, route_error, upstream_error, downstream_left)"
    );
}

// ============================================================================
// GaugeGuard - RAII guard for gauge increment/decrement
// ============================================================================

/// RAII guard that increments a gauge on creation and decrements on drop.
#[must_use = "GaugeGuard must be held for the duration you want the gauge incremented"]
pub struct GaugeGuard {
    name: &'static str,
}

impl GaugeGuard {
    pub fn new(name: &'static str) -> Self {
        metrics::gauge!(name).increment(1.0);
        Self { name }
    }
}

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        metrics::gauge!(self.name).decrement(1.0);
    }
}

// ============================================================================
// TimingGuard - RAII guard for recording duration histograms
// ============================================================================

/// RAII guard that records elapsed time to a histogram on drop.
#[must_use = "TimingGuard must be held for the duration you want to measure"]
pub struct TimingGuard {
    name: &'static str,
    start: std::time::Instant,
    labels: Option<(&'static str, &'static str)>,
}

impl TimingGuard {
    #[allow(dead_code)] // Keep API available for future histograms without labels
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            start: std::time::Instant::now(),
            labels: None,
        }
    }

    pub fn with_label(
        name: &'static str,
        label_key: &'static str,
        label_value: &'static str,
    ) -> Self {
        Self {
            name,
            start: std::time::Instant::now(),
            labels: Some((label_key, label_value)),
        }
    }

    /// Update the label value (useful when outcome determines the label)
    pub fn set_label(&mut self, label_key: &'static str, label_value: &'static str) {
        self.labels = Some((label_key, label_value));
    }
}

impl Drop for TimingGuard {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed().as_secs_f64();
        if let Some((key, value)) = self.labels {
            metrics::histogram!(self.name, key => value).record(elapsed);
        } else {
            metrics::histogram!(self.name).record(elapsed);
        }
    }
}
