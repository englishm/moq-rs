// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::hash_map;
use std::collections::HashMap;
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

use moq_transport::{
    coding::{TrackNamespace, TrackNamespacePrefix},
    serve::{FullTrackName, ServeError, Track, TrackReader, TrackWriter},
};
use tokio::sync::{broadcast, mpsc};

use crate::interest::{TrackInterest, TrackInterestGuard};
use crate::metrics::GaugeGuard;

/// Scope key for the outer level of the two-level registry.
///
/// An empty string (`""`) represents the global/unscoped bucket. All unscoped
/// connections share this bucket — any publisher without a scope can be reached
/// by any subscriber without a scope. This is the default behavior for backward
/// compatibility with pre-scope deployments.
type ScopeKey = String;

/// The scope key used for unscoped (global) registrations.
const UNSCOPED: &str = "";

const NAMESPACE_REQUEST_CHANNEL_CAPACITY: usize = 1024;

/// How long a pull-through cache entry with no downstream subscribers is kept
/// before the upstream subscription is released.
///
/// The grace period keeps the common case cheap: a subscriber reconnecting, or a
/// player switching between renditions, reuses the warm cache entry instead of
/// paying a fresh upstream SUBSCRIBE round trip. Past that we stop paying to
/// receive a track nobody is watching.
pub const DEFAULT_CACHE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Capacity of the namespace add/remove notification broadcast channel used by
/// SUBSCRIBE_NAMESPACE handlers.
const NAMESPACE_CHANGE_CHANNEL_CAPACITY: usize = 1024;

/// Capacity of the PUBLISH track add/remove notification broadcast channel used
/// by Publish/Both fan-out.
const TRACK_CHANGE_CHANNEL_CAPACITY: usize = 1024;

#[derive(Clone)]
struct NamespaceSource {
    requests: mpsc::Sender<TrackRequest>,
}

/// A request for a track that is not cached yet, sent to the session that
/// advertised the namespace so it can SUBSCRIBE upstream.
pub struct TrackRequest {
    /// Writer the upstream subscription should fill.
    pub writer: TrackWriter,

    /// Lease on the pull-through cache entry backing this request.
    ///
    /// The requester should hold the upstream subscription open until
    /// [`CacheLease::released`] resolves, then drop it to send UNSUBSCRIBE.
    pub lease: CacheLease,
}

/// Ties an upstream subscription to downstream interest in its cache entry.
///
/// Handed to whichever session owns the upstream subscription so it can tell when
/// the cached track has gone unwatched and stop paying for it.
pub struct CacheLease {
    locals: Locals,
    scope_key: ScopeKey,
    full_name: FullTrackName,
    interest: TrackInterest,
}

impl CacheLease {
    /// Resolve once the cache entry has been evicted for lack of interest.
    ///
    /// The caller should drop its upstream subscription when this returns, which
    /// sends UNSUBSCRIBE. Eviction happens first so that a subscriber arriving
    /// afterwards misses the cache and triggers a fresh upstream SUBSCRIBE,
    /// rather than attaching to a reader that is about to stop being fed.
    ///
    /// Never resolves when the idle timeout is disabled, preserving the previous
    /// behaviour of holding the upstream subscription for the session's lifetime.
    pub async fn released(&self) {
        let timeout = self.locals.cache_idle_timeout;
        if timeout.is_zero() {
            std::future::pending::<()>().await;
        }

        loop {
            self.interest.idle_for(timeout).await;

            if self
                .locals
                .evict_idle_cache_entry(&self.scope_key, &self.full_name, &self.interest)
            {
                return;
            }

            // A subscriber arrived between the timer firing and the eviction
            // taking the lock, so the entry is busy again. Go back to waiting.
            tracing::trace!(
                namespace = %self.full_name.namespace,
                track = %self.full_name.name,
                "cache eviction abandoned; downstream interest returned"
            );
        }
    }
}

struct NamespaceEntry {
    local: Option<NamespaceSource>,
    remote: Weak<RemoteNamespaceSource>,
}

struct RemoteNamespaceSource {
    locals: Locals,
    scope_key: ScopeKey,
    namespace: TrackNamespace,
}

struct TrackEntry {
    reader: TrackReader,
    source: TrackSource,

    /// Downstream interest in this entry, for [`TrackSource::Cache`] entries only.
    ///
    /// Locally published tracks have no upstream subscription to release, so
    /// there is nothing to count.
    interest: Option<TrackInterest>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TrackSource {
    Published,
    Cache,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamespaceChange {
    pub scope: Option<String>,
    pub namespace: TrackNamespace,
    pub added: bool,
}

#[derive(Clone)]
pub enum TrackChange {
    Added {
        scope: Option<String>,
        track: TrackReader,
    },
    Removed {
        scope: Option<String>,
        full_name: FullTrackName,
    },
}

/// Relay-local registry.
///
/// Actual media tracks are always stored by exact Full Track Name. Namespace
/// entries combine discovery metadata with an optional local PUBLISH_NAMESPACE
/// route source that can be asked for a missing track.
#[derive(Clone)]
pub struct Locals {
    /// Actual media tracks, indexed by (scope, full track name).
    tracks: Arc<RwLock<HashMap<ScopeKey, HashMap<FullTrackName, TrackEntry>>>>,

    /// Namespace sources, indexed by (scope, namespace) and matched by prefix.
    /// Each entry can have one local PUBLISH_NAMESPACE route source and shared
    /// ownership by any number of remote discovery registrations.
    namespaces: Arc<RwLock<HashMap<ScopeKey, HashMap<TrackNamespace, NamespaceEntry>>>>,

    /// Namespace add/remove notifications for SUBSCRIBE_NAMESPACE handlers.
    namespace_changes: broadcast::Sender<NamespaceChange>,

    /// Actual PUBLISH track add/remove notifications for Publish/Both fan-out.
    track_changes: broadcast::Sender<TrackChange>,

    /// How long an unwatched pull-through cache entry is retained before its
    /// upstream subscription is released. Zero disables eviction.
    cache_idle_timeout: Duration,
}

impl Default for Locals {
    fn default() -> Self {
        Self::new()
    }
}

impl Locals {
    pub fn new() -> Self {
        Self::with_cache_idle_timeout(DEFAULT_CACHE_IDLE_TIMEOUT)
    }

    /// Build a registry with a custom pull-through cache idle timeout.
    ///
    /// A zero timeout disables idle eviction, holding upstream subscriptions for
    /// as long as the upstream session lives.
    pub fn with_cache_idle_timeout(cache_idle_timeout: Duration) -> Self {
        let (namespace_changes, _) = broadcast::channel(NAMESPACE_CHANGE_CHANNEL_CAPACITY);
        let (track_changes, _) = broadcast::channel(TRACK_CHANGE_CHANNEL_CAPACITY);
        Self {
            tracks: Default::default(),
            namespaces: Default::default(),
            namespace_changes,
            track_changes,
            cache_idle_timeout,
        }
    }

    pub fn subscribe_namespace_changes(&self) -> broadcast::Receiver<NamespaceChange> {
        self.namespace_changes.subscribe()
    }

    pub fn subscribe_track_changes(&self) -> broadcast::Receiver<TrackChange> {
        self.track_changes.subscribe()
    }

    pub fn list_namespaces_matching(
        &self,
        scope: Option<&str>,
        prefix: &TrackNamespacePrefix,
    ) -> Vec<TrackNamespace> {
        let Ok(namespaces) = self.namespaces.read() else {
            return Vec::new();
        };
        let Some(bucket) = namespaces.get(scope.unwrap_or(UNSCOPED)) else {
            return Vec::new();
        };

        bucket
            .keys()
            .filter(|namespace| prefix.is_prefix_of(namespace))
            .cloned()
            .collect()
    }

    pub fn list_tracks_matching(
        &self,
        scope: Option<&str>,
        prefix: &TrackNamespacePrefix,
    ) -> Vec<TrackReader> {
        let scope_key = scope.unwrap_or(UNSCOPED);

        // Collect matching readers under a shared read lock so concurrent
        // SUBSCRIBE_NAMESPACE fan-outs don't serialize against each other. Note any
        // closed entries and prune them afterwards under a brief write lock, keeping
        // the common (no-stale) path read-only.
        let mut matches = Vec::new();
        let mut stale = Vec::new();
        {
            let Ok(tracks) = self.tracks.read() else {
                return Vec::new();
            };
            let Some(bucket) = tracks.get(scope_key) else {
                return Vec::new();
            };
            for (full_name, entry) in bucket.iter() {
                if entry.reader.is_closed() {
                    stale.push(full_name.clone());
                } else if entry.source == TrackSource::Published
                    && prefix.is_prefix_of(&full_name.namespace)
                {
                    matches.push(entry.reader.clone());
                }
            }
        }

        if !stale.is_empty() {
            self.prune_closed_tracks(scope_key, &stale);
        }

        matches
    }

    /// Remove the given tracks from a scope bucket if they are still closed.
    ///
    /// Invoked off the read path (e.g. by [`Locals::list_tracks_matching`]) so that
    /// closed-entry cleanup takes the write lock only briefly and only when a closed
    /// entry was actually observed. The re-check guards against removing an entry
    /// that a concurrent writer replaced with a live reader.
    fn prune_closed_tracks(&self, scope_key: &str, keys: &[FullTrackName]) {
        let Ok(mut tracks) = self.tracks.write() else {
            return;
        };
        let Some(bucket) = tracks.get_mut(scope_key) else {
            return;
        };
        for key in keys {
            if bucket
                .get(key)
                .is_some_and(|entry| entry.reader.is_closed())
            {
                bucket.remove(key);
            }
        }
        if bucket.is_empty() {
            tracks.remove(scope_key);
        }
    }

    /// Register namespace routing metadata from PUBLISH_NAMESPACE.
    ///
    /// This does not register any media tracks. It only creates a request queue
    /// used when a downstream SUBSCRIBE asks for a missing track under this
    /// namespace.
    pub async fn register_namespace(
        &mut self,
        scope: Option<&str>,
        namespace: TrackNamespace,
    ) -> anyhow::Result<(LocalNamespaceRegistration, mpsc::Receiver<TrackRequest>)> {
        let scope_key = scope.unwrap_or(UNSCOPED).to_string();
        let (tx, rx) = mpsc::channel(NAMESPACE_REQUEST_CHANNEL_CAPACITY);

        let added = {
            let mut namespaces = self
                .namespaces
                .write()
                .map_err(|_| ServeError::internal_ctx("locals namespace registry lock poisoned"))?;
            let bucket = namespaces.entry(scope_key.clone()).or_default();
            match bucket.entry(namespace.clone()) {
                hash_map::Entry::Vacant(entry) => {
                    entry.insert(NamespaceEntry {
                        local: Some(NamespaceSource { requests: tx }),
                        remote: Weak::new(),
                    });
                    true
                }
                hash_map::Entry::Occupied(mut entry) => {
                    if entry.get().local.is_some() {
                        return Err(ServeError::Duplicate.into());
                    }
                    entry.get_mut().local = Some(NamespaceSource { requests: tx });
                    false
                }
            }
        };

        let registration = LocalNamespaceRegistration {
            locals: self.clone(),
            scope_key,
            namespace: namespace.clone(),
            _gauge_guard: GaugeGuard::new("moq_relay_announced_namespaces"),
        };

        if added {
            let _ = self.namespace_changes.send(NamespaceChange {
                scope: scope_key_to_option(&registration.scope_key),
                namespace,
                added: true,
            });
        }

        Ok((registration, rx))
    }

    /// Register remote discovery metadata for one exact namespace.
    pub fn register_remote_namespace(
        &self,
        scope: Option<&str>,
        namespace: TrackNamespace,
    ) -> anyhow::Result<RemoteNamespaceRegistration> {
        let scope_key = scope.unwrap_or(UNSCOPED).to_string();
        let (source, added) = {
            let mut namespaces = self
                .namespaces
                .write()
                .map_err(|_| ServeError::internal_ctx("locals namespace registry lock poisoned"))?;
            let bucket = namespaces.entry(scope_key.clone()).or_default();
            match bucket.entry(namespace.clone()) {
                hash_map::Entry::Vacant(entry) => {
                    let source = Arc::new(RemoteNamespaceSource {
                        locals: self.clone(),
                        scope_key: scope_key.clone(),
                        namespace: namespace.clone(),
                    });
                    entry.insert(NamespaceEntry {
                        local: None,
                        remote: Arc::downgrade(&source),
                    });
                    (source, true)
                }
                hash_map::Entry::Occupied(mut entry) => {
                    if let Some(source) = entry.get().remote.upgrade() {
                        (source, false)
                    } else {
                        let source = Arc::new(RemoteNamespaceSource {
                            locals: self.clone(),
                            scope_key: scope_key.clone(),
                            namespace: namespace.clone(),
                        });
                        entry.get_mut().remote = Arc::downgrade(&source);
                        (source, false)
                    }
                }
            }
        };

        let registration = RemoteNamespaceRegistration { _source: source };

        if added {
            let _ = self.namespace_changes.send(NamespaceChange {
                scope: scope_key_to_option(&scope_key),
                namespace,
                added: true,
            });
        }

        Ok(registration)
    }

    /// Register one exact track received via PUBLISH.
    pub async fn register_track(
        &mut self,
        scope: Option<&str>,
        track: TrackReader,
    ) -> anyhow::Result<LocalTrackRegistration> {
        let full_name = FullTrackName {
            namespace: track.namespace.clone(),
            name: track.name.clone(),
        };
        self.insert_track_with_registration(scope, full_name, track)
            .await
    }

    async fn insert_track_with_registration(
        &mut self,
        scope: Option<&str>,
        full_name: FullTrackName,
        track: TrackReader,
    ) -> anyhow::Result<LocalTrackRegistration> {
        let scope_key = scope.unwrap_or(UNSCOPED).to_string();

        let mut tracks = self
            .tracks
            .write()
            .map_err(|_| ServeError::internal_ctx("locals track registry lock poisoned"))?;
        let bucket = tracks.entry(scope_key.clone()).or_default();
        match bucket.entry(full_name.clone()) {
            hash_map::Entry::Vacant(entry) => entry.insert(TrackEntry {
                reader: track.clone(),
                source: TrackSource::Published,
                // A locally published track has no upstream subscription to
                // release, so there is nothing to count interest against.
                interest: None,
            }),
            hash_map::Entry::Occupied(_) => return Err(ServeError::Duplicate.into()),
        };

        let _ = self.track_changes.send(TrackChange::Added {
            scope: scope_key_to_option(&scope_key),
            track,
        });

        Ok(LocalTrackRegistration {
            locals: self.clone(),
            scope_key,
            full_name,
            _gauge_guard: GaugeGuard::new("moq_relay_active_published_tracks"),
        })
    }

    /// Retrieve one actual media track by exact Full Track Name.
    pub fn retrieve_track(
        &self,
        scope: Option<&str>,
        full_name: &FullTrackName,
    ) -> Option<TrackReader> {
        let scope_key = scope.unwrap_or(UNSCOPED);

        // Fast path: look up under a shared read lock.
        {
            let tracks = self.tracks.read().ok()?;
            let bucket = tracks.get(scope_key)?;
            match bucket.get(full_name) {
                Some(entry) if !entry.reader.is_closed() => return Some(entry.reader.clone()),
                Some(_) => {} // closed: fall through to prune under the write lock
                None => return None,
            }
        }

        // The entry was closed; remove it under a brief write lock (re-checking in
        // case a concurrent writer replaced it with a live reader).
        self.prune_closed_tracks(scope_key, std::slice::from_ref(full_name));
        None
    }

    /// Remove an unwatched pull-through cache entry.
    ///
    /// Returns false, leaving the entry in place, if interest returned before the
    /// write lock was acquired, or if the entry has since been replaced by a
    /// different generation or by a locally published track.
    ///
    /// The idle check happens under the same lock that guards are created under,
    /// so a subscriber racing eviction either gets counted here (and eviction is
    /// abandoned) or misses the cache entirely and requests a fresh one.
    fn evict_idle_cache_entry(
        &self,
        scope_key: &str,
        full_name: &FullTrackName,
        interest: &TrackInterest,
    ) -> bool {
        let Ok(mut tracks) = self.tracks.write() else {
            // A poisoned registry means we can no longer reason about the entry;
            // release the upstream subscription rather than pinning it forever.
            return true;
        };

        let Some(bucket) = tracks.get_mut(scope_key) else {
            return true;
        };

        match bucket.get(full_name) {
            Some(entry) => {
                // Only evict the generation this lease belongs to. A replacement
                // entry has its own lease and its own idle timer.
                let ours = entry
                    .interest
                    .as_ref()
                    .is_some_and(|current| current.same_generation(interest));

                if !ours {
                    return true;
                }

                if !interest.is_idle() {
                    return false;
                }

                bucket.remove(full_name);
            }
            // Already gone (e.g. pruned as closed), so the upstream subscription
            // is no longer serving anything.
            None => return true,
        }

        if bucket.is_empty() {
            tracks.remove(scope_key);
        }

        tracing::debug!(
            namespace = %full_name.namespace,
            track = %full_name.name,
            "evicting idle cached track and releasing upstream subscription"
        );
        metrics::counter!("moq_relay_cache_idle_evictions_total", "source" => "local").increment(1);

        true
    }

    /// Return the best namespace route source for a requested namespace.
    fn route_namespace(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
    ) -> Option<NamespaceSource> {
        let namespaces = self.namespaces.read().ok()?;
        let bucket = namespaces.get(scope.unwrap_or(UNSCOPED))?;

        let mut best_match: Option<NamespaceSource> = None;
        let mut best_len = 0;

        for (registered_ns, entry) in bucket.iter() {
            let Some(source) = entry.local.as_ref() else {
                continue;
            };

            if namespace.fields.len() >= registered_ns.fields.len() {
                let is_prefix = registered_ns
                    .fields
                    .iter()
                    .zip(namespace.fields.iter())
                    .all(|(a, b)| a == b);

                if is_prefix && registered_ns.fields.len() > best_len {
                    best_match = Some(source.clone());
                    best_len = registered_ns.fields.len();
                }
            }
        }

        best_match
    }

    /// Get an existing exact track or request it from a matching namespace source.
    ///
    /// This replaces the old `TracksReader::subscribe` relay registry behavior:
    /// the actual track reader is stored in `tracks`, while PUBLISH_NAMESPACE is
    /// only a source to ask when a track is missing.
    /// Returns the reader plus, for pull-through cache entries, a guard the caller
    /// must hold for as long as it is serving that reader. Dropping the guard is
    /// what eventually lets the upstream subscription be released.
    pub async fn get_or_request_track(
        &mut self,
        scope: Option<&str>,
        namespace: TrackNamespace,
        track_name: impl Into<moq_transport::coding::TrackName>,
    ) -> Option<(TrackReader, Option<TrackInterestGuard>)> {
        let track_name = track_name.into();
        let full_name = FullTrackName {
            namespace: namespace.clone(),
            name: track_name.clone(),
        };
        let scope_key = scope.unwrap_or(UNSCOPED).to_string();

        if let Some(hit) = self.retrieve_track_with_interest(&scope_key, &full_name) {
            return Some(hit);
        }

        let source = self.route_namespace(scope, &namespace)?;

        // Reserve the pull-through cache slot under a single lock so concurrent
        // misses for the same track collapse onto one produced reader. The caller
        // that finds no live entry claims the slot with a freshly produced track and
        // owns requesting it from the source; concurrent callers share the reserved
        // reader instead of racing to insert and failing with a spurious `None`.
        let (writer, reader, interest, guard) = {
            let mut tracks = self.tracks.write().ok()?;
            let bucket = tracks.entry(scope_key.clone()).or_default();

            if let Some(entry) = bucket.get(&full_name) {
                if !entry.reader.is_closed() {
                    let guard = entry.interest.as_ref().map(TrackInterest::guard);
                    return Some((entry.reader.clone(), guard));
                }
            }

            // Vacant slot (or a stale, closed reader): claim it with a fresh track.
            let (writer, reader) = Track::new(namespace, track_name).produce();
            let interest = TrackInterest::new();

            // Take this caller's guard before the entry is visible to anyone
            // else, so the entry can never be seen as idle while we are still
            // setting up the upstream subscription.
            let guard = interest.guard();

            bucket.insert(
                full_name.clone(),
                TrackEntry {
                    reader: reader.clone(),
                    source: TrackSource::Cache,
                    interest: Some(interest.clone()),
                },
            );
            (writer, reader, interest, guard)
        };

        let lease = CacheLease {
            locals: self.clone(),
            scope_key: scope_key.clone(),
            full_name: full_name.clone(),
            interest: interest.clone(),
        };

        if source
            .requests
            .send(TrackRequest { writer, lease })
            .await
            .is_err()
        {
            // The source is gone, so nothing will ever fill this reader. Remove
            // the slot we claimed, but only if it is still ours: a concurrent
            // caller may already have replaced it with a live generation.
            self.remove_cache_generation(&scope_key, &full_name, &interest);
            return None;
        }

        Some((reader, Some(guard)))
    }

    /// Remove a cache entry if it is still the given generation, regardless of
    /// whether anyone is currently interested in it.
    fn remove_cache_generation(
        &self,
        scope_key: &str,
        full_name: &FullTrackName,
        interest: &TrackInterest,
    ) {
        let Ok(mut tracks) = self.tracks.write() else {
            return;
        };
        let Some(bucket) = tracks.get_mut(scope_key) else {
            return;
        };

        let ours = bucket.get(full_name).is_some_and(|entry| {
            entry
                .interest
                .as_ref()
                .is_some_and(|current| current.same_generation(interest))
        });

        if ours {
            bucket.remove(full_name);
        }

        if bucket.is_empty() {
            tracks.remove(scope_key);
        }
    }

    /// Cache lookup that also registers interest, under a single lock.
    ///
    /// Interest must be registered while the lock is held: taking the guard after
    /// releasing it would leave a window where eviction sees an idle entry and
    /// removes the reader this caller is about to serve.
    fn retrieve_track_with_interest(
        &self,
        scope_key: &str,
        full_name: &FullTrackName,
    ) -> Option<(TrackReader, Option<TrackInterestGuard>)> {
        {
            let tracks = self.tracks.read().ok()?;
            let bucket = tracks.get(scope_key)?;
            match bucket.get(full_name) {
                Some(entry) if !entry.reader.is_closed() => {
                    let guard = entry.interest.as_ref().map(TrackInterest::guard);
                    return Some((entry.reader.clone(), guard));
                }
                Some(_) => {} // closed: fall through to prune under the write lock
                None => return None,
            }
        }

        self.prune_closed_tracks(scope_key, std::slice::from_ref(full_name));
        None
    }
}

pub struct LocalNamespaceRegistration {
    locals: Locals,
    scope_key: ScopeKey,
    namespace: TrackNamespace,
    _gauge_guard: GaugeGuard,
}

impl Drop for LocalNamespaceRegistration {
    fn drop(&mut self) {
        let ns = self.namespace.to_utf8_path();
        let scope = if self.scope_key.is_empty() {
            "<unscoped>"
        } else {
            &self.scope_key
        };
        tracing::debug!(namespace = %ns, scope = %scope, "deregistering namespace route source from locals");

        let mut removed = false;
        if let Ok(mut namespaces) = self.locals.namespaces.write() {
            if let Some(bucket) = namespaces.get_mut(self.scope_key.as_str()) {
                let remove_namespace = if let Some(entry) = bucket.get_mut(&self.namespace) {
                    entry.local.take().is_some() && entry.remote.upgrade().is_none()
                } else {
                    false
                };
                if remove_namespace {
                    removed = bucket.remove(&self.namespace).is_some();
                }
                if bucket.is_empty() {
                    namespaces.remove(self.scope_key.as_str());
                }
            }
        }

        if removed {
            let _ = self.locals.namespace_changes.send(NamespaceChange {
                scope: scope_key_to_option(&self.scope_key),
                namespace: self.namespace.clone(),
                added: false,
            });
        }
    }
}

pub struct RemoteNamespaceRegistration {
    _source: Arc<RemoteNamespaceSource>,
}

impl Drop for RemoteNamespaceSource {
    fn drop(&mut self) {
        let mut removed = false;
        if let Ok(mut namespaces) = self.locals.namespaces.write() {
            if let Some(bucket) = namespaces.get_mut(self.scope_key.as_str()) {
                let remove_namespace = if let Some(entry) = bucket.get_mut(&self.namespace) {
                    if std::ptr::eq(entry.remote.as_ptr(), self) {
                        entry.remote = Weak::new();
                        entry.local.is_none()
                    } else {
                        false
                    }
                } else {
                    false
                };
                if remove_namespace {
                    removed = bucket.remove(&self.namespace).is_some();
                }
                if bucket.is_empty() {
                    namespaces.remove(self.scope_key.as_str());
                }
            }
        }

        if removed {
            let _ = self.locals.namespace_changes.send(NamespaceChange {
                scope: scope_key_to_option(&self.scope_key),
                namespace: self.namespace.clone(),
                added: false,
            });
        }
    }
}

fn scope_key_to_option(scope_key: &str) -> Option<String> {
    if scope_key.is_empty() {
        None
    } else {
        Some(scope_key.to_string())
    }
}

pub struct LocalTrackRegistration {
    locals: Locals,
    scope_key: ScopeKey,
    full_name: FullTrackName,
    _gauge_guard: GaugeGuard,
}

impl Drop for LocalTrackRegistration {
    fn drop(&mut self) {
        let namespace = self.full_name.namespace.to_utf8_path();
        let track = self.full_name.name.to_string();
        let scope = if self.scope_key.is_empty() {
            "<unscoped>"
        } else {
            &self.scope_key
        };
        tracing::debug!(namespace = %namespace, track = %track, scope = %scope, "deregistering track from locals");

        let mut removed = false;
        if let Ok(mut tracks) = self.locals.tracks.write() {
            if let Some(bucket) = tracks.get_mut(self.scope_key.as_str()) {
                removed = bucket.remove(&self.full_name).is_some();
                if bucket.is_empty() {
                    tracks.remove(self.scope_key.as_str());
                }
            }
        }

        if removed {
            let _ = self.locals.track_changes.send(TrackChange::Removed {
                scope: scope_key_to_option(&self.scope_key),
                full_name: self.full_name.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moq_transport::coding::TrackName;

    fn ns(path: &str) -> TrackNamespace {
        TrackNamespace::from_utf8_path(path)
    }

    fn full(namespace: &TrackNamespace, name: &str) -> FullTrackName {
        FullTrackName {
            namespace: namespace.clone(),
            name: TrackName::from(name),
        }
    }

    async fn assert_no_namespace_change(changes: &mut broadcast::Receiver<NamespaceChange>) {
        let change =
            tokio::time::timeout(std::time::Duration::from_millis(50), changes.recv()).await;
        assert!(change.is_err(), "unexpected namespace change: {change:?}");
    }

    #[tokio::test]
    async fn remote_namespace_is_visible_but_not_a_track_route() {
        let mut locals = Locals::new();
        let namespace = ns("room/remote");
        let registration = locals
            .register_remote_namespace(Some("scope-a"), namespace.clone())
            .expect("remote namespace should register");

        assert_eq!(
            locals.list_namespaces_matching(
                Some("scope-a"),
                &TrackNamespacePrefix::from_utf8_path("room"),
            ),
            vec![namespace.clone()]
        );
        assert!(locals
            .get_or_request_track(Some("scope-a"), namespace, "video")
            .await
            .is_none());

        drop(registration);
    }

    #[tokio::test]
    async fn local_then_remote_emits_only_union_transitions() {
        let mut locals = Locals::new();
        let mut changes = locals.subscribe_namespace_changes();
        let namespace = ns("room/shared");
        let (local, _requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("local namespace should register");
        assert!(changes.recv().await.expect("local add").added);

        let remote = locals
            .register_remote_namespace(None, namespace.clone())
            .expect("remote namespace should coexist");
        assert_no_namespace_change(&mut changes).await;

        drop(local);
        assert_no_namespace_change(&mut changes).await;
        assert_eq!(
            locals.list_namespaces_matching(
                None,
                &TrackNamespacePrefix::from_utf8_path("room/shared"),
            ),
            vec![namespace.clone()]
        );

        drop(remote);
        let removed = changes.recv().await.expect("last-source removal");
        assert_eq!(removed.namespace, namespace);
        assert!(!removed.added);
    }

    #[tokio::test]
    async fn remote_then_local_keeps_local_route_until_last_drop() {
        let mut locals = Locals::new();
        let mut changes = locals.subscribe_namespace_changes();
        let namespace = ns("room/shared");
        let remote = locals
            .register_remote_namespace(None, namespace.clone())
            .expect("remote namespace should register");
        assert!(changes.recv().await.expect("remote add").added);

        let (local, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("local namespace should coexist");
        assert_no_namespace_change(&mut changes).await;

        drop(remote);
        assert_no_namespace_change(&mut changes).await;
        let requested = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("local route should remain usable");
        drop(requested);
        assert!(requests.recv().await.is_some());

        drop(local);
        let removed = changes.recv().await.expect("local removal");
        assert_eq!(removed.namespace, namespace);
        assert!(!removed.added);
    }

    #[tokio::test]
    async fn multiple_remote_sources_remove_on_last_drop() {
        let locals = Locals::new();
        let mut changes = locals.subscribe_namespace_changes();
        let namespace = ns("room/remote");
        let first = locals
            .register_remote_namespace(None, namespace.clone())
            .expect("first remote should register");
        assert!(changes.recv().await.expect("first add").added);

        let second = locals
            .register_remote_namespace(None, namespace.clone())
            .expect("second remote should register");
        assert_no_namespace_change(&mut changes).await;

        drop(first);
        assert_no_namespace_change(&mut changes).await;
        drop(second);

        let removed = changes.recv().await.expect("last remote removal");
        assert_eq!(removed.namespace, namespace);
        assert!(!removed.added);
    }

    #[tokio::test]
    async fn remote_source_does_not_allow_duplicate_local_registration() {
        let mut locals = Locals::new();
        let namespace = ns("room/shared");
        let _remote = locals
            .register_remote_namespace(None, namespace.clone())
            .expect("remote namespace should register");
        let (_local, _requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("first local namespace should register");

        let duplicate = locals.register_namespace(None, namespace).await;
        assert!(duplicate.is_err());
    }

    #[tokio::test]
    async fn remote_namespace_scopes_stay_isolated() {
        let locals = Locals::new();
        let _a = locals
            .register_remote_namespace(Some("scope-a"), ns("room/a"))
            .expect("scope-a remote should register");
        let _b = locals
            .register_remote_namespace(Some("scope-b"), ns("room/b"))
            .expect("scope-b remote should register");

        assert_eq!(
            locals.list_namespaces_matching(
                Some("scope-a"),
                &TrackNamespacePrefix::from_utf8_path("room"),
            ),
            vec![ns("room/a")]
        );
        assert_eq!(
            locals.list_namespaces_matching(
                Some("scope-b"),
                &TrackNamespacePrefix::from_utf8_path("room"),
            ),
            vec![ns("room/b")]
        );
    }

    #[tokio::test]
    async fn register_track_makes_exact_track_retrievable_until_drop() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (writer, reader) = Track::new(namespace.clone(), "audio").produce();
        let key = full(&namespace, "audio");

        let registration = locals
            .register_track(None, reader.clone())
            .await
            .expect("track registration should succeed");

        assert!(locals.retrieve_track(None, &key).is_some());

        drop(registration);
        assert!(locals.retrieve_track(None, &key).is_none());

        drop(writer);
    }

    #[tokio::test]
    async fn get_or_request_track_uses_namespace_source_and_caches_reader() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let (reader, guard) = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested from namespace source");
        assert!(
            guard.is_some(),
            "a cached track should hand back an interest guard"
        );

        let requested = requests
            .recv()
            .await
            .expect("source should get TrackRequest");
        assert_eq!(requested.writer.namespace, namespace);
        assert_eq!(requested.writer.name, TrackName::from("video"));

        let key = full(&namespace, "video");
        let cached = locals
            .retrieve_track(None, &key)
            .expect("requested track should be cached");
        assert_eq!(cached.namespace, reader.namespace);
        assert_eq!(cached.name, reader.name);

        let (reader_again, _guard_again) = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("cached track should be returned");
        assert_eq!(reader_again.namespace, namespace);
        assert_eq!(reader_again.name, TrackName::from("video"));

        let no_second_request =
            tokio::time::timeout(std::time::Duration::from_millis(50), requests.recv()).await;
        assert!(
            no_second_request.is_err(),
            "cache hit should not request again"
        );
    }

    #[tokio::test]
    async fn concurrent_get_or_request_track_deduplicates_request() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let mut first = locals.clone();
        let mut second = locals.clone();
        let (first_reader, second_reader) = tokio::join!(
            first.get_or_request_track(None, namespace.clone(), "video"),
            second.get_or_request_track(None, namespace.clone(), "video"),
        );

        let (first_reader, _first_guard) = first_reader.expect("first request should get a reader");
        let (second_reader, _second_guard) =
            second_reader.expect("second request should get cached reader");
        assert_eq!(first_reader.namespace, namespace);
        assert_eq!(second_reader.namespace, namespace);
        assert_eq!(first_reader.name, TrackName::from("video"));
        assert_eq!(second_reader.name, TrackName::from("video"));

        requests
            .recv()
            .await
            .expect("source should receive one TrackRequest");
        let no_second_request =
            tokio::time::timeout(std::time::Duration::from_millis(50), requests.recv()).await;
        assert!(
            no_second_request.is_err(),
            "concurrent misses should be deduplicated"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_get_or_request_track_multi_thread_never_returns_spurious_none() {
        // Exercise the cache-slot race across real threads: many callers miss the
        // cache for the same track at once. Every caller must receive a reader (the
        // winner's reserved reader), and the source must see exactly one request.
        for _ in 0..64 {
            let mut locals = Locals::new();
            let namespace = ns("room/123");
            let (_registration, mut requests) = locals
                .register_namespace(None, namespace.clone())
                .await
                .expect("namespace source should register");

            let mut handles = Vec::new();
            for _ in 0..8 {
                let mut clone = locals.clone();
                let namespace = namespace.clone();
                handles.push(tokio::spawn(async move {
                    clone.get_or_request_track(None, namespace, "video").await
                }));
            }

            for handle in handles {
                let reader = handle.await.expect("task should not panic");
                assert!(
                    reader.is_some(),
                    "every concurrent caller should receive a reader"
                );
            }

            let first = requests
                .recv()
                .await
                .expect("source should receive exactly one TrackRequest");
            assert_eq!(first.writer.namespace, namespace);
            let extra =
                tokio::time::timeout(std::time::Duration::from_millis(50), requests.recv()).await;
            assert!(
                extra.is_err(),
                "concurrent misses across threads must be deduplicated"
            );
        }
    }

    #[tokio::test]
    async fn get_or_request_track_uses_longest_namespace_prefix() {
        let mut locals = Locals::new();
        let (_short_registration, mut short_requests) = locals
            .register_namespace(None, ns("room"))
            .await
            .expect("short prefix should register");
        let (_long_registration, mut long_requests) = locals
            .register_namespace(None, ns("room/123"))
            .await
            .expect("long prefix should register");

        let requested_ns = ns("room/123/camera");
        let (reader, _guard) = locals
            .get_or_request_track(None, requested_ns.clone(), "video")
            .await
            .expect("track should be requested from longest prefix");
        assert_eq!(reader.namespace, requested_ns);

        let long_request = long_requests
            .recv()
            .await
            .expect("longest prefix should receive the request");
        assert_eq!(long_request.writer.namespace, ns("room/123/camera"));
        assert_eq!(long_request.writer.name, TrackName::from("video"));

        let no_short_request =
            tokio::time::timeout(std::time::Duration::from_millis(50), short_requests.recv()).await;
        assert!(
            no_short_request.is_err(),
            "shorter prefix should not receive request"
        );
    }

    #[tokio::test]
    async fn get_or_request_track_returns_none_without_track_or_namespace_source() {
        let mut locals = Locals::new();
        let result = locals
            .get_or_request_track(None, ns("unknown"), "video")
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_namespaces_matching_filters_by_prefix_and_scope() {
        let mut locals = Locals::new();
        let _room = locals
            .register_namespace(Some("scope-a"), ns("room/123"))
            .await
            .expect("room should register");
        let _camera = locals
            .register_namespace(Some("scope-a"), ns("room/123/camera"))
            .await
            .expect("camera should register");
        let _other_scope = locals
            .register_namespace(Some("scope-b"), ns("room/123/other"))
            .await
            .expect("other scope should register");

        let mut matches = locals.list_namespaces_matching(
            Some("scope-a"),
            &TrackNamespacePrefix::from_utf8_path("room/123"),
        );
        matches.sort_by_key(|namespace| namespace.to_utf8_path());

        assert_eq!(matches, vec![ns("room/123"), ns("room/123/camera")]);
    }

    #[tokio::test]
    async fn list_namespaces_matching_keeps_unscoped_and_scoped_separate() {
        let mut locals = Locals::new();
        let _global = locals
            .register_namespace(None, ns("room/global"))
            .await
            .expect("global namespace should register");
        let _scoped = locals
            .register_namespace(Some("scope-a"), ns("room/scoped"))
            .await
            .expect("scoped namespace should register");

        let global =
            locals.list_namespaces_matching(None, &TrackNamespacePrefix::from_utf8_path("room"));
        assert_eq!(global, vec![ns("room/global")]);

        let scoped = locals.list_namespaces_matching(
            Some("scope-a"),
            &TrackNamespacePrefix::from_utf8_path("room"),
        );
        assert_eq!(scoped, vec![ns("room/scoped")]);
    }

    #[tokio::test]
    async fn namespace_changes_reports_register_and_drop() {
        let mut locals = Locals::new();
        let mut changes = locals.subscribe_namespace_changes();
        let namespace = ns("room/123");

        let registration = locals
            .register_namespace(Some("scope-a"), namespace.clone())
            .await
            .expect("namespace should register")
            .0;

        let added = changes.recv().await.expect("added event");
        assert_eq!(
            added,
            NamespaceChange {
                scope: Some("scope-a".to_string()),
                namespace: namespace.clone(),
                added: true,
            }
        );

        drop(registration);

        let removed = changes.recv().await.expect("removed event");
        assert_eq!(
            removed,
            NamespaceChange {
                scope: Some("scope-a".to_string()),
                namespace,
                added: false,
            }
        );
    }

    #[tokio::test]
    async fn namespace_changes_uses_none_for_unscoped() {
        let mut locals = Locals::new();
        let mut changes = locals.subscribe_namespace_changes();
        let namespace = ns("room/123");

        let _registration = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace should register")
            .0;

        let added = changes.recv().await.expect("added event");
        assert_eq!(
            added,
            NamespaceChange {
                scope: None,
                namespace,
                added: true,
            }
        );
    }

    #[tokio::test]
    async fn list_tracks_matching_filters_by_namespace_prefix_and_scope() {
        let mut locals = Locals::new();
        let (_writer_a, reader_a) = Track::new(ns("room/123"), "audio").produce();
        let (_writer_b, reader_b) = Track::new(ns("room/123/camera"), "video").produce();
        let (_writer_other_scope, reader_other_scope) =
            Track::new(ns("room/123"), "other").produce();

        let _a = locals
            .register_track(Some("scope-a"), reader_a)
            .await
            .expect("track a should register");
        let _b = locals
            .register_track(Some("scope-a"), reader_b)
            .await
            .expect("track b should register");
        let _other = locals
            .register_track(Some("scope-b"), reader_other_scope)
            .await
            .expect("other scope should register");

        let mut tracks = locals.list_tracks_matching(
            Some("scope-a"),
            &TrackNamespacePrefix::from_utf8_path("room/123"),
        );
        tracks.sort_by_key(|track| format!("{}/{}", track.namespace, track.name));

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].namespace, ns("room/123"));
        assert_eq!(tracks[0].name, TrackName::from("audio"));
        assert_eq!(tracks[1].namespace, ns("room/123/camera"));
        assert_eq!(tracks[1].name, TrackName::from("video"));
    }

    #[tokio::test]
    async fn list_tracks_matching_excludes_pull_through_cache_entries() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let _reader = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested and cached");
        let _requested = requests.recv().await.expect("source should get request");

        let tracks =
            locals.list_tracks_matching(None, &TrackNamespacePrefix::from_utf8_path("room/123"));

        assert!(tracks.is_empty());
    }

    #[tokio::test]
    async fn list_tracks_matching_keeps_unscoped_and_scoped_separate() {
        let mut locals = Locals::new();
        let (_global_writer, global_reader) = Track::new(ns("room/global"), "audio").produce();
        let (_scoped_writer, scoped_reader) = Track::new(ns("room/scoped"), "video").produce();

        let _global = locals
            .register_track(None, global_reader)
            .await
            .expect("global track should register");
        let _scoped = locals
            .register_track(Some("scope-a"), scoped_reader)
            .await
            .expect("scoped track should register");

        let global =
            locals.list_tracks_matching(None, &TrackNamespacePrefix::from_utf8_path("room"));
        assert_eq!(global.len(), 1);
        assert_eq!(global[0].namespace, ns("room/global"));
        assert_eq!(global[0].name, TrackName::from("audio"));

        let scoped = locals.list_tracks_matching(
            Some("scope-a"),
            &TrackNamespacePrefix::from_utf8_path("room"),
        );
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].namespace, ns("room/scoped"));
        assert_eq!(scoped[0].name, TrackName::from("video"));
    }

    #[tokio::test]
    async fn track_changes_reports_register_and_drop() {
        let mut locals = Locals::new();
        let mut changes = locals.subscribe_track_changes();
        let namespace = ns("room/123");
        let (_writer, reader) = Track::new(namespace.clone(), "audio").produce();

        let registration = locals
            .register_track(Some("scope-a"), reader)
            .await
            .expect("track should register");

        let added = changes.recv().await.expect("added event");
        match added {
            TrackChange::Added { scope, track } => {
                assert_eq!(scope, Some("scope-a".to_string()));
                assert_eq!(track.namespace, namespace);
                assert_eq!(track.name, TrackName::from("audio"));
            }
            TrackChange::Removed { .. } => panic!("expected added event"),
        }

        drop(registration);

        let removed = changes.recv().await.expect("removed event");
        match removed {
            TrackChange::Removed { scope, full_name } => {
                assert_eq!(scope, Some("scope-a".to_string()));
                assert_eq!(full_name.namespace, ns("room/123"));
                assert_eq!(full_name.name, TrackName::from("audio"));
            }
            TrackChange::Added { .. } => panic!("expected removed event"),
        }
    }
    /// The core fix: once the last downstream subscriber leaves, the cache entry
    /// is evicted and the lease resolves so the upstream subscription can be
    /// dropped (sending UNSUBSCRIBE).
    #[tokio::test(start_paused = true)]
    async fn idle_cached_track_is_evicted_and_lease_released() {
        let idle = Duration::from_secs(30);
        let mut locals = Locals::with_cache_idle_timeout(idle);
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let (_reader, guard) = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested");
        let guard = guard.expect("cached track should hand back a guard");

        let request = requests.recv().await.expect("source should get a request");
        let key = full(&namespace, "video");

        // While a subscriber is being served the entry must stay put.
        let released = request.lease.released();
        tokio::pin!(released);
        tokio::select! {
            _ = &mut released => panic!("evicted a track that still has a subscriber"),
            _ = tokio::time::sleep(idle * 4) => {}
        }
        assert!(locals.retrieve_track(None, &key).is_some());

        // Last subscriber leaves: the entry lingers for the grace period, then goes.
        drop(guard);
        tokio::select! {
            _ = &mut released => panic!("evicted before the grace period elapsed"),
            _ = tokio::time::sleep(idle / 2) => {}
        }
        assert!(
            locals.retrieve_track(None, &key).is_some(),
            "a warm cache entry should survive a brief gap between subscribers"
        );

        released.await;
        assert!(
            locals.retrieve_track(None, &key).is_none(),
            "idle entry should be evicted so the next subscriber re-requests upstream"
        );
    }

    /// A subscriber arriving during the grace period keeps the warm entry, and no
    /// second upstream request is made.
    #[tokio::test(start_paused = true)]
    async fn subscriber_returning_within_grace_period_reuses_the_entry() {
        let idle = Duration::from_secs(30);
        let mut locals = Locals::with_cache_idle_timeout(idle);
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let (_reader, guard) = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested");
        let request = requests.recv().await.expect("source should get a request");

        let released = request.lease.released();
        tokio::pin!(released);

        drop(guard.expect("cached track should hand back a guard"));

        tokio::select! {
            _ = &mut released => panic!("evicted before the grace period elapsed"),
            _ = tokio::time::sleep(idle / 2) => {}
        }

        // New subscriber inside the grace period: served from cache, no new request.
        let (_reader, guard) = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("warm entry should still be served");
        let guard = guard.expect("cached track should hand back a guard");

        tokio::select! {
            _ = &mut released => panic!("evicted while a subscriber was present"),
            _ = tokio::time::sleep(idle * 4) => {}
        }

        assert!(
            requests.try_recv().is_err(),
            "reusing the warm entry must not trigger a second upstream SUBSCRIBE"
        );

        // And it still gets evicted once that subscriber leaves too.
        drop(guard);
        released.await;
        assert!(locals
            .retrieve_track(None, &full(&namespace, "video"))
            .is_none());
    }

    /// A subscriber that gives up before the upstream subscription is even set up
    /// must not pin the entry forever.
    #[tokio::test(start_paused = true)]
    async fn cancelled_requester_does_not_pin_the_upstream_subscription() {
        let idle = Duration::from_secs(30);
        let mut locals = Locals::with_cache_idle_timeout(idle);
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let (_reader, guard) = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested");

        // The subscriber goes away before the source has even picked up the
        // request, which is what a cancelled SUBSCRIBE looks like.
        drop(guard);

        let request = requests.recv().await.expect("source should get a request");
        request.lease.released().await;

        assert!(
            locals
                .retrieve_track(None, &full(&namespace, "video"))
                .is_none(),
            "an abandoned request must not leave a permanently leased entry"
        );
    }

    /// A zero timeout preserves the old behaviour: the upstream subscription is
    /// held for as long as the session lives.
    #[tokio::test(start_paused = true)]
    async fn zero_idle_timeout_disables_eviction() {
        let mut locals = Locals::with_cache_idle_timeout(Duration::ZERO);
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let (_reader, guard) = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested");
        let request = requests.recv().await.expect("source should get a request");

        drop(guard);

        tokio::select! {
            _ = request.lease.released() => panic!("eviction should be disabled"),
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }

        assert!(locals
            .retrieve_track(None, &full(&namespace, "video"))
            .is_some());
    }

    /// A guard from an evicted generation must not keep a replacement entry alive.
    #[tokio::test(start_paused = true)]
    async fn stale_guard_does_not_block_eviction_of_a_replacement() {
        let idle = Duration::from_secs(30);
        let mut locals = Locals::with_cache_idle_timeout(idle);
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let (_reader, first_guard) = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("first request");
        let first_request = requests.recv().await.expect("first request received");

        // Drop the first generation's entry while deliberately keeping its guard
        // alive, modelling a slow subscriber that outlives its cache entry.
        let stale_guard = first_guard.expect("guard");
        let key = full(&namespace, "video");
        locals
            .tracks
            .write()
            .unwrap()
            .get_mut(UNSCOPED)
            .unwrap()
            .remove(&key);
        drop(first_request);

        // A second subscriber creates a fresh generation.
        let (_reader, second_guard) = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("second request");
        let second_request = requests.recv().await.expect("second request received");
        drop(second_guard.expect("guard"));

        // The stale guard belongs to the old counter, so it must not make the new
        // entry look busy.
        let _ = &stale_guard;
        second_request.lease.released().await;

        assert!(
            locals.retrieve_track(None, &key).is_none(),
            "a stale guard must not pin a replacement entry"
        );
    }
}
