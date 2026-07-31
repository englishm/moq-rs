// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::hash_map;
use std::collections::HashMap;

use std::sync::{Arc, Mutex};

use moq_transport::{
    coding::TrackNamespace,
    serve::{ServeError, TracksReader},
};

use crate::metrics::GaugeGuard;

/// Scope key for the outer level of the two-level registry.
///
/// An empty string (`""`) represents the global/unscoped bucket. All unscoped
/// connections share this bucket — any publisher without a scope can be reached
/// by any subscriber without a scope. This is the default behavior for backward
/// compatibility with pre-scope deployments.
///
/// We use `String` rather than `Option<String>` so that `HashMap::get` can
/// accept a `&str` via the `Borrow` trait, avoiding a heap allocation on
/// every lookup in `retrieve()`.
type ScopeKey = String;

/// The scope key used for unscoped (global) registrations.
const UNSCOPED: &str = "";

/// Registry of local tracks, indexed by (scope, namespace).
///
/// Uses a two-level map so that `retrieve()` only scans namespaces within
/// the matching scope, rather than iterating all namespaces across all scopes.
#[derive(Clone)]
pub struct Locals {
    lookup: Arc<Mutex<HashMap<ScopeKey, HashMap<TrackNamespace, TracksReader>>>>,
}

impl Default for Locals {
    fn default() -> Self {
        Self::new()
    }
}

/// Local tracks registry.
impl Locals {
    pub fn new() -> Self {
        Self {
            lookup: Default::default(),
        }
    }

    /// Register new local tracks.
    ///
    /// `scope` is the resolved scope identity from `Coordinator::resolve_scope()`,
    /// or `None` for unscoped sessions. Registrations are keyed by `(scope, namespace)`,
    /// so the same namespace in different scopes routes independently.
    pub async fn register(
        &mut self,
        scope: Option<&str>,
        tracks: TracksReader,
    ) -> anyhow::Result<Registration> {
        let namespace = tracks.namespace.clone();
        let scope_key = scope.unwrap_or(UNSCOPED).to_string();

        // Insert the tracks into the scope bucket
        let mut lookup = self.lookup.lock().unwrap();
        let bucket = lookup.entry(scope_key.clone()).or_default();
        match bucket.entry(namespace.clone()) {
            hash_map::Entry::Vacant(entry) => entry.insert(tracks),
            hash_map::Entry::Occupied(_) => return Err(ServeError::Duplicate.into()),
        };

        let registration = Registration {
            locals: self.clone(),
            scope_key,
            namespace,
            _gauge_guard: GaugeGuard::new("moq_relay_announced_namespaces"),
        };

        Ok(registration)
    }

    /// Retrieve local tracks by namespace using hierarchical prefix matching.
    /// Returns the TracksReader for the longest matching namespace prefix.
    ///
    /// `scope` is the resolved scope identity from `Coordinator::resolve_scope()`,
    /// or `None` for unscoped sessions. When `scope` is `None`, only tracks
    /// registered without a scope (the global/unscoped bucket) are searched.
    pub fn retrieve(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
    ) -> Option<TracksReader> {
        let lookup = self.lookup.lock().unwrap();

        // Look up the scope bucket directly — O(1), zero allocation.
        // HashMap<String, V>::get accepts &str via Borrow<str>.
        let bucket = lookup.get(scope.unwrap_or(UNSCOPED))?;

        // Find the longest matching prefix within this scope
        let mut best_match: Option<TracksReader> = None;
        let mut best_len = 0;

        for (registered_ns, tracks) in bucket.iter() {
            // Check if registered_ns is a prefix of namespace
            if namespace.fields.len() >= registered_ns.fields.len() {
                let is_prefix = registered_ns
                    .fields
                    .iter()
                    .zip(namespace.fields.iter())
                    .all(|(a, b)| a == b);

                if is_prefix && registered_ns.fields.len() > best_len {
                    best_match = Some(tracks.clone());
                    best_len = registered_ns.fields.len();
                }
            }
        }

        best_match
    }
}

pub struct Registration {
    locals: Locals,
    scope_key: ScopeKey,
    namespace: TrackNamespace,
    /// Gauge guard for tracking announced namespaces - decrements on drop
    _gauge_guard: GaugeGuard,
}

/// Deregister local tracks on drop.
impl Drop for Registration {
    fn drop(&mut self) {
        let scope = if self.scope_key.is_empty() {
            "<unscoped>"
        } else {
            &self.scope_key
        };
        tracing::debug!(namespace = %self.namespace, scope = %scope, "deregistering namespace from locals");

        let mut lookup = self.locals.lookup.lock().unwrap();
        if let Some(bucket) = lookup.get_mut(self.scope_key.as_str()) {
            bucket.remove(&self.namespace);
            // Clean up empty scope buckets to avoid memory leaks
            if bucket.is_empty() {
                lookup.remove(self.scope_key.as_str());
            }
        }
    }
}
