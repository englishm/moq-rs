// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::hash_map;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

use moq_transport::{
    coding::{Location, TrackNamespace, TrackNamespacePrefix},
    serve::{FullTrackName, ServeError, Track, TrackReader, TrackWriter},
};
use tokio::sync::{broadcast, mpsc, watch, OwnedSemaphorePermit, Semaphore};

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

/// Maximum rendezvous requests that may hold relay state at once.
///
/// One [`Locals`] registry is shared by every session on a relay. Matching the
/// per-session bidirectional stream limit bounds aggregate rendezvous work to
/// what one fully occupied session could create.
pub(crate) const MAX_CONCURRENT_RENDEZVOUS_HOLDS: usize = 128;

// REQUEST_ERROR stores the minimum delay in milliseconds plus one. The stride
// is coprime with the spread, so one relay-wide sequence visits every delay
// before repeating.
const RENDEZVOUS_RETRY_INTERVAL_MIN: u64 = 1_001;
const RENDEZVOUS_RETRY_INTERVAL_SPREAD: u64 = 1_000;
const RENDEZVOUS_RETRY_INTERVAL_STRIDE: u64 = 577;

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
    /// [`CacheLease::released`] resolves, wait for upstream cancellation, then
    /// call [`CacheLease::finish_release`].
    pub lease: CacheLease,

    /// Reports whether the upstream subscription was established.
    ///
    /// Downstream subscribers wait on this before SUBSCRIBE_OK is sent, so the
    /// requester must report the outcome as soon as the upstream publisher
    /// answers — via [`UpstreamReadyTx::established`] or
    /// [`UpstreamReadyTx::failed`] — or drop the sender to abandon the request,
    /// which releases any waiting subscribers with an error rather than
    /// stranding them until the upstream response timeout.
    pub upstream: UpstreamReadyTx,
}

/// State of the upstream subscription backing a pull-through cache entry.
#[derive(Clone)]
enum UpstreamState {
    /// The upstream SUBSCRIBE has been queued but not answered yet.
    Pending,

    /// The upstream publisher acknowledged the SUBSCRIBE.
    Established,

    /// The upstream subscription could not be established.
    Failed(ServeError),
}

/// Readiness gate for the upstream subscription behind a cached track.
///
/// Draft-18 Section 9.4 requires a relay to have an established upstream subscription
/// before it sends SUBSCRIBE_OK for a downstream SUBSCRIBE. The pull-through
/// cache reserves its entry synchronously but subscribes upstream asynchronously
/// (the publishing session owns that side), so the entry exists before the
/// upstream subscription does. Serving a downstream subscriber has to wait for
/// this gate rather than for the entry alone.
///
/// Shared by every downstream subscriber of the same cache entry, so a second
/// subscriber arriving mid-handshake waits on the same result instead of
/// triggering a duplicate upstream SUBSCRIBE.
#[derive(Clone)]
pub struct UpstreamReady {
    state: watch::Receiver<UpstreamState>,
}

impl UpstreamReady {
    /// Whether the upstream publisher has not answered yet.
    pub(crate) fn is_pending(&self) -> bool {
        matches!(*self.state.borrow(), UpstreamState::Pending)
    }

    /// Wait until the upstream subscription is established.
    ///
    /// Returns the upstream failure if it could not be established, so the caller
    /// can reject the downstream request with a matching REQUEST_ERROR instead of
    /// accepting it and later reporting the failure as PUBLISH_DONE.
    ///
    /// Resolves as an error rather than hanging if the requester is dropped
    /// without answering — an abandoned request is not an established one.
    pub async fn established(&self) -> Result<(), ServeError> {
        let mut state = self.state.clone();

        loop {
            match &*state.borrow_and_update() {
                UpstreamState::Established => return Ok(()),
                UpstreamState::Failed(err) => return Err(err.clone()),
                UpstreamState::Pending => {}
            }

            if state.changed().await.is_err() {
                return Err(ServeError::internal_ctx(
                    "upstream subscription abandoned before it was established",
                ));
            }
        }
    }
}

/// Sender half of [`UpstreamReady`], held by the session that owns the upstream
/// subscription.
///
/// Dropping this without resolving it releases any waiting downstream subscriber
/// with an error, so a requester that gives up cannot strand them.
pub struct UpstreamReadyTx {
    state: watch::Sender<UpstreamState>,
}

impl UpstreamReadyTx {
    /// Report that the upstream publisher acknowledged the SUBSCRIBE.
    pub fn established(&self) {
        self.resolve(UpstreamState::Established);
    }

    /// Report that the upstream subscription could not be established.
    pub fn failed(&self, err: ServeError) {
        self.resolve(UpstreamState::Failed(err));
    }

    /// Record the outcome, whether or not anyone is currently waiting.
    ///
    /// `watch::Sender::send` refuses to update once the last receiver is gone,
    /// which would leave the state on `Pending` forever and make a resolved
    /// subscription indistinguishable from an abandoned one. The outcome is
    /// what [`Drop`] keys off, so it has to be recorded unconditionally.
    fn resolve(&self, outcome: UpstreamState) {
        self.state.send_replace(outcome);
    }
}

/// Resolve the gate on drop so abandonment is never silent.
///
/// Most ways this sender dies are not calls to [`UpstreamReadyTx::established`]
/// or [`UpstreamReadyTx::failed`]: the owning session can return early, be torn
/// down mid-handshake, or have its task dropped while parked on the upstream
/// SUBSCRIBE. In every one of those cases the publisher really is gone. Failing
/// the gate releases waiting subscribers before the response timeout.
///
/// Without this, the sender's disappearance was only observable as a closed
/// channel, which surfaced downstream as an unattributable internal error with
/// no indication of which session gave up or why.
impl Drop for UpstreamReadyTx {
    fn drop(&mut self) {
        // Fast path: already resolved. Checked before building the error so a
        // successful subscription's teardown neither allocates nor takes the
        // write lock.
        //
        // This must stay an `if` around `matches!`. The condition of an `if` is
        // a temporary scope, so the read guard is released before the body
        // runs; `if let` is not, and would hold it across the write lock below
        // and self-deadlock every abandoned request.
        if !matches!(&*self.state.borrow(), UpstreamState::Pending) {
            return;
        }

        // Deliberately not `ServeError::internal_ctx`: that mints a UUID, and
        // `Uuid::new_v4` panics rather than degrading when the OS entropy
        // source is unavailable (seccomp-blocked `getrandom`, an exhausted fd
        // table with no `/dev/urandom`). A `Drop` must not make a fallible
        // syscall. This code can run during
        // unwinding: a panic here while another panic is in flight aborts the
        // process, turning one failed task into a dead relay.
        //
        // Nothing is logged here either. The waiting subscriber already reports
        // this at its own site, with the namespace and track attached, so the
        // reason only has to be carried in the error itself. Spelling it out
        // rather than hiding it behind a correlation id makes that report
        // self-explanatory and gives the downstream REQUEST_ERROR an honest
        // reason phrase; there is nothing sensitive in "the publisher left".
        //
        // Built out here rather than in the closure so the allocation stays off
        // the write lock, and so the closure cannot do anything but match and
        // move, because `send_if_modified` re-raises a panicking closure.
        let err = ServeError::Internal(
            "upstream publisher went away before the subscription was established".to_string(),
        );

        // Re-check under the lock so a concurrently recorded outcome always
        // wins over this fallback; the sender is never cloned, so this is
        // belt-and-braces rather than a live race.
        self.state.send_if_modified(|state| match state {
            UpstreamState::Pending => {
                *state = UpstreamState::Failed(err);
                true
            }
            _ => false,
        });
    }
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
    finished: bool,
}

impl CacheLease {
    /// Resolve once the cache entry is ready for ordered upstream cancellation.
    ///
    /// The entry remains as a closing generation until
    /// [`finish_release`](Self::finish_release) is called after the peer has
    /// ended the old request stream. Subscribers wait for that transition so a
    /// replacement SUBSCRIBE cannot overtake the old UNSUBSCRIBE.
    ///
    /// Never resolves when the idle timeout is disabled, preserving the previous
    /// behaviour of holding the upstream subscription for the session's lifetime.
    pub async fn released(&self) {
        let timeout = self.locals.cache_idle_timeout;
        if timeout.is_zero() {
            std::future::pending::<()>().await;
        }

        self.release_after(timeout).await;
    }

    /// Release an upstream request as soon as every waiting subscriber leaves.
    ///
    /// A request that has not been accepted upstream has no warm state worth
    /// retaining for the normal cache grace period.
    pub(crate) async fn abandoned(&self) {
        self.release_after(Duration::ZERO).await;
    }

    pub(crate) fn finish_release(mut self) {
        self.release();
    }

    pub(crate) fn begin_release(&self) {
        self.locals
            .mark_cache_entry_closing(&self.scope_key, &self.full_name, &self.interest);
    }

    fn release(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.locals
            .remove_cache_generation(&self.scope_key, &self.full_name, &self.interest);
        self.interest.mark_removed();
    }

    async fn release_after(&self, timeout: Duration) {
        loop {
            self.interest.idle_for(timeout).await;

            if self.locals.mark_idle_cache_entry_closing(
                &self.scope_key,
                &self.full_name,
                &self.interest,
            ) {
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

impl Drop for CacheLease {
    fn drop(&mut self) {
        // Requests can be abandoned while still buffered in the namespace
        // queue, before a session takes responsibility for their teardown.
        self.release();
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

    /// Upstream readiness for [`TrackSource::Cache`] entries only.
    ///
    /// Locally published tracks are already established by the time they are
    /// registered, so there is nothing to wait for.
    upstream: Option<UpstreamReady>,
}

/// A track resolved from the relay-local registry, with the handles a caller must
/// observe while serving it.
pub struct LocalTrack {
    /// The media reader to serve downstream.
    pub reader: TrackReader,

    /// Greatest Object present when this lookup resolved.
    pub largest_location: Option<Location>,

    /// Held for as long as the caller serves [`Self::reader`]. Dropping it is what
    /// eventually lets an idle upstream subscription be released.
    ///
    /// `None` for locally published tracks, which have no upstream subscription.
    pub interest: Option<TrackInterestGuard>,

    /// Must resolve before the caller sends SUBSCRIBE_OK downstream (draft-18
    /// Section 9.4).
    ///
    /// `None` for locally published tracks, which are already established.
    pub upstream: Option<UpstreamReady>,
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

    /// Shared by every session using this relay-local registry. Admission never
    /// waits because queued requests would still pin their bidirectional streams.
    rendezvous_hold_permits: Arc<Semaphore>,

    /// Shared sequence keeps overload responses from separate sessions from
    /// choosing the same point in the retry window.
    rendezvous_retry_sequence: Arc<AtomicU64>,
}

struct CacheGenerationCleanup {
    locals: Locals,
    scope_key: ScopeKey,
    full_name: FullTrackName,
    interest: TrackInterest,
    armed: bool,
}

impl CacheGenerationCleanup {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for CacheGenerationCleanup {
    fn drop(&mut self) {
        if self.armed {
            self.locals
                .remove_cache_generation(&self.scope_key, &self.full_name, &self.interest);
        }
    }
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
        Self::with_cache_idle_timeout_and_rendezvous_capacity(
            cache_idle_timeout,
            MAX_CONCURRENT_RENDEZVOUS_HOLDS,
        )
    }

    fn with_cache_idle_timeout_and_rendezvous_capacity(
        cache_idle_timeout: Duration,
        rendezvous_capacity: usize,
    ) -> Self {
        let (namespace_changes, _) = broadcast::channel(NAMESPACE_CHANGE_CHANNEL_CAPACITY);
        let (track_changes, _) = broadcast::channel(TRACK_CHANGE_CHANNEL_CAPACITY);
        Self {
            tracks: Default::default(),
            namespaces: Default::default(),
            namespace_changes,
            track_changes,
            cache_idle_timeout,
            rendezvous_hold_permits: Arc::new(Semaphore::new(rendezvous_capacity)),
            rendezvous_retry_sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_rendezvous_capacity(rendezvous_capacity: usize) -> Self {
        Self::with_cache_idle_timeout_and_rendezvous_capacity(
            DEFAULT_CACHE_IDLE_TIMEOUT,
            rendezvous_capacity,
        )
    }

    pub(crate) fn try_acquire_rendezvous_hold(&self) -> Option<OwnedSemaphorePermit> {
        self.rendezvous_hold_permits
            .clone()
            .try_acquire_owned()
            .ok()
    }

    #[cfg(test)]
    pub(crate) fn available_rendezvous_holds(&self) -> usize {
        self.rendezvous_hold_permits.available_permits()
    }

    #[cfg(test)]
    fn contains_track_entry(&self, scope: Option<&str>, full_name: &FullTrackName) -> bool {
        let scope_key = scope.unwrap_or(UNSCOPED);
        self.tracks
            .read()
            .ok()
            .and_then(|tracks| {
                tracks
                    .get(scope_key)
                    .map(|bucket| bucket.contains_key(full_name))
            })
            .unwrap_or(false)
    }

    pub(crate) fn rendezvous_overload_retry_interval(&self) -> u64 {
        let sequence = self
            .rendezvous_retry_sequence
            .fetch_add(1, Ordering::Relaxed);
        RENDEZVOUS_RETRY_INTERVAL_MIN
            + sequence.wrapping_mul(RENDEZVOUS_RETRY_INTERVAL_STRIDE)
                % RENDEZVOUS_RETRY_INTERVAL_SPREAD
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
        // closed entries and handle them afterwards under a brief write lock, keeping
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
            self.mark_closed_cache_tracks(scope_key, &stale);
        }

        matches
    }

    /// Mark closed cache entries as closing without removing published generations.
    ///
    /// Invoked off the read path (e.g. by [`Locals::list_tracks_matching`]) so that
    /// the transition takes the write lock only briefly. The re-check avoids marking
    /// an entry that a concurrent writer replaced with a live reader.
    fn mark_closed_cache_tracks(&self, scope_key: &str, keys: &[FullTrackName]) {
        let Ok(tracks) = self.tracks.write() else {
            return;
        };
        let Some(bucket) = tracks.get(scope_key) else {
            return;
        };
        for key in keys {
            if let Some(entry) = bucket.get(key).filter(|entry| entry.reader.is_closed()) {
                // Cache entries remain as a closing generation until their
                // upstream request stream is torn down. Published entries are
                // removed only by their generation-owning registration.
                if let Some(interest) = &entry.interest {
                    interest.mark_closing();
                }
            }
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
                // release, so there is nothing to count interest against and
                // nothing to wait for before accepting a downstream SUBSCRIBE.
                interest: None,
                upstream: None,
            }),
            hash_map::Entry::Occupied(_) => return Err(ServeError::Duplicate.into()),
        };

        let _ = self.track_changes.send(TrackChange::Added {
            scope: scope_key_to_option(&scope_key),
            track: track.clone(),
        });

        Ok(LocalTrackRegistration {
            locals: self.clone(),
            scope_key,
            full_name,
            reader: track,
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

        // A closed cache entry must remain visible as a closing generation until
        // its upstream request stream is torn down.
        self.mark_closed_cache_tracks(scope_key, std::slice::from_ref(full_name));
        None
    }

    /// Mark an unwatched pull-through cache entry as closing.
    ///
    /// Returns false, leaving the entry in place, if interest returned before the
    /// write lock was acquired, or if the entry has since been replaced by a
    /// different generation or by a locally published track.
    ///
    /// The idle check happens under the same lock that guards are created under,
    /// so a subscriber racing eviction either gets counted here (and eviction is
    /// abandoned) or misses the cache entirely and requests a fresh one.
    fn mark_idle_cache_entry_closing(
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
                    interest.mark_removed();
                    return true;
                }

                if !interest.is_idle() {
                    return false;
                }

                interest.mark_closing();
                metrics::counter!("moq_relay_cache_idle_evictions_total", "source" => "local")
                    .increment(1);
            }
            // Already gone (e.g. pruned as closed), so the upstream subscription
            // is no longer serving anything.
            None => {
                interest.mark_removed();
                return true;
            }
        }

        tracing::debug!(
            namespace = %full_name.namespace,
            track = %full_name.name,
            "closing idle cached track before releasing upstream subscription"
        );

        true
    }

    fn mark_cache_entry_closing(
        &self,
        scope_key: &str,
        full_name: &FullTrackName,
        interest: &TrackInterest,
    ) {
        let Ok(tracks) = self.tracks.write() else {
            return;
        };
        let ours = tracks
            .get(scope_key)
            .and_then(|bucket| bucket.get(full_name))
            .and_then(|entry| entry.interest.as_ref())
            .is_some_and(|current| current.same_generation(interest));
        if ours {
            interest.mark_closing();
        } else {
            interest.mark_removed();
        }
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

    pub(crate) fn has_namespace_route(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
    ) -> bool {
        self.route_namespace(scope, namespace).is_some()
    }

    /// Get an existing exact track or request it from a matching namespace source.
    ///
    /// This replaces the old `TracksReader::subscribe` relay registry behavior:
    /// the actual track reader is stored in `tracks`, while PUBLISH_NAMESPACE is
    /// only a source to ask when a track is missing.
    /// Returns the reader plus, for pull-through cache entries, a guard the caller
    /// must hold for as long as it is serving that reader (dropping it is what
    /// eventually lets the upstream subscription be released) and a readiness gate
    /// the caller must await before accepting the downstream request.
    pub async fn get_or_request_track(
        &mut self,
        scope: Option<&str>,
        namespace: TrackNamespace,
        track_name: impl Into<moq_transport::coding::TrackName>,
    ) -> Option<LocalTrack> {
        let track_name = track_name.into();
        let full_name = FullTrackName {
            namespace: namespace.clone(),
            name: track_name.clone(),
        };
        let scope_key = scope.unwrap_or(UNSCOPED).to_string();

        loop {
            if let Some(hit) = self.retrieve_track_with_interest(scope, &full_name) {
                return Some(hit);
            }

            let source = self.route_namespace(scope, &namespace)?;

            // Reserve the pull-through cache slot under a single lock so concurrent
            // misses for the same track collapse onto one produced reader. The caller
            // that finds no live entry claims the slot with a freshly produced track and
            // owns requesting it from the source; concurrent callers share the reserved
            // reader instead of racing to insert and failing with a spurious `None`.
            let mut closing_generation = None;
            let mut removed_published = false;
            let created = {
                let mut tracks = self.tracks.write().ok()?;
                let bucket = tracks.entry(scope_key.clone()).or_default();

                if let Some(entry) = bucket.get(&full_name) {
                    if let Some(closing) = entry
                        .interest
                        .as_ref()
                        .filter(|interest| interest.is_closing())
                        .cloned()
                    {
                        closing_generation = Some(closing);
                    } else if !entry.reader.is_closed() {
                        return Some(LocalTrack {
                            largest_location: entry.reader.largest_location(),
                            reader: entry.reader.clone(),
                            interest: entry.interest.as_ref().map(TrackInterest::guard),
                            upstream: entry.upstream.clone(),
                        });
                    } else if let Some(interest) = &entry.interest {
                        interest.mark_closing();
                        closing_generation = Some(interest.clone());
                    } else if entry.source == TrackSource::Published {
                        removed_published = true;
                    }
                }

                if closing_generation.is_some() {
                    None
                } else {
                    // Vacant slot (or a stale, closed reader): claim it with a fresh track.
                    let (writer, reader) =
                        Track::new(namespace.clone(), track_name.clone()).produce();
                    let interest = TrackInterest::new();

                    // Take this caller's guard before the entry is visible to anyone
                    // else, so the entry can never be seen as idle while we are still
                    // setting up the upstream subscription.
                    let guard = interest.guard();

                    // Published alongside the entry so that a subscriber arriving while
                    // the upstream handshake is still in flight shares this gate rather
                    // than being served a reader nothing has subscribed to yet.
                    let (upstream_tx, upstream_rx) = watch::channel(UpstreamState::Pending);
                    let upstream = UpstreamReady { state: upstream_rx };

                    bucket.insert(
                        full_name.clone(),
                        TrackEntry {
                            reader: reader.clone(),
                            source: TrackSource::Cache,
                            interest: Some(interest.clone()),
                            upstream: Some(upstream.clone()),
                        },
                    );
                    Some((
                        writer,
                        reader,
                        interest,
                        guard,
                        UpstreamReadyTx { state: upstream_tx },
                        upstream,
                    ))
                }
            };

            if removed_published {
                let _ = self.track_changes.send(TrackChange::Removed {
                    scope: scope_key_to_option(&scope_key),
                    full_name: full_name.clone(),
                });
            }

            if let Some(closing) = closing_generation {
                closing.removed().await;
                continue;
            }
            let (writer, reader, interest, guard, upstream_tx, upstream) = created?;

            let lease = CacheLease {
                locals: self.clone(),
                scope_key: scope_key.clone(),
                full_name: full_name.clone(),
                interest: interest.clone(),
                finished: false,
            };
            let cleanup = CacheGenerationCleanup {
                locals: self.clone(),
                scope_key: scope_key.clone(),
                full_name: full_name.clone(),
                interest: interest.clone(),
                armed: true,
            };

            if source
                .requests
                .send(TrackRequest {
                    writer,
                    lease,
                    upstream: upstream_tx,
                })
                .await
                .is_err()
            {
                return None;
            }

            cleanup.disarm();

            return Some(LocalTrack {
                reader,
                largest_location: None,
                interest: Some(guard),
                upstream: Some(upstream),
            });
        }
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

        interest.mark_removed();
    }

    /// Cache lookup that also registers interest, under a single lock.
    ///
    /// Interest must be registered while the lock is held: taking the guard after
    /// releasing it would leave a window where eviction sees an idle entry and
    /// removes the reader this caller is about to serve.
    pub(crate) fn retrieve_track_with_interest(
        &self,
        scope: Option<&str>,
        full_name: &FullTrackName,
    ) -> Option<LocalTrack> {
        let scope_key = scope.unwrap_or(UNSCOPED);
        {
            let tracks = self.tracks.read().ok()?;
            let bucket = tracks.get(scope_key)?;
            match bucket.get(full_name) {
                Some(entry)
                    if !entry.reader.is_closed()
                        && !entry
                            .interest
                            .as_ref()
                            .is_some_and(TrackInterest::is_closing) =>
                {
                    return Some(LocalTrack {
                        largest_location: entry.reader.largest_location(),
                        reader: entry.reader.clone(),
                        interest: entry.interest.as_ref().map(TrackInterest::guard),
                        upstream: entry.upstream.clone(),
                    });
                }
                Some(_) => {} // closed: fall through to prune under the write lock
                None => return None,
            }
        }

        self.mark_closed_cache_tracks(scope_key, std::slice::from_ref(full_name));
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
    reader: TrackReader,
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

        if let Ok(mut tracks) = self.locals.tracks.write() {
            if let Some(bucket) = tracks.get_mut(self.scope_key.as_str()) {
                let ours = bucket.get(&self.full_name).is_some_and(|entry| {
                    entry.source == TrackSource::Published
                        && Arc::ptr_eq(&entry.reader.info, &self.reader.info)
                });
                if ours && bucket.remove(&self.full_name).is_some() {
                    let _ = self.locals.track_changes.send(TrackChange::Removed {
                        scope: scope_key_to_option(&self.scope_key),
                        full_name: self.full_name.clone(),
                    });
                }
                if bucket.is_empty() {
                    tracks.remove(self.scope_key.as_str());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moq_transport::coding::TrackName;
    use moq_transport::message::RequestErrorCode;

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

    #[test]
    fn rendezvous_hold_capacity_is_shared_across_sessions() {
        let locals = Locals::with_rendezvous_capacity(2);
        let other_session = locals.clone();
        let mut permits = (0..2)
            .map(|_| {
                locals
                    .try_acquire_rendezvous_hold()
                    .expect("capacity should admit the configured number of holds")
            })
            .collect::<Vec<_>>();

        assert!(other_session.try_acquire_rendezvous_hold().is_none());

        permits.pop();
        assert!(other_session.try_acquire_rendezvous_hold().is_some());
    }

    #[test]
    fn overload_retries_use_one_relay_wide_sequence() {
        let locals = Locals::new();
        let other_session = locals.clone();
        let intervals = (0..1_000)
            .map(|index| {
                if index % 2 == 0 {
                    locals.rendezvous_overload_retry_interval()
                } else {
                    other_session.rendezvous_overload_retry_interval()
                }
            })
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(intervals.len(), 1_000);
        assert!(intervals
            .iter()
            .all(|interval| (1_001..=2_000).contains(interval)));
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
    async fn closed_published_track_remains_owned_until_registration_drops() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (writer, reader) = Track::new(namespace.clone(), "audio").produce();
        let registration = locals
            .register_track(None, reader)
            .await
            .expect("track registration should succeed");
        drop(writer);

        assert!(locals
            .retrieve_track(None, &full(&namespace, "audio"))
            .is_none());
        let (_replacement_writer, replacement_reader) =
            Track::new(namespace.clone(), "audio").produce();
        assert!(locals
            .register_track(None, replacement_reader.clone())
            .await
            .is_err());

        drop(registration);
        let _replacement = locals
            .register_track(None, replacement_reader)
            .await
            .expect("replacement should register after the old owner drops");
    }

    #[tokio::test]
    async fn stale_published_registration_does_not_remove_cache_replacement() {
        let mut locals = Locals::new();
        let mut changes = locals.subscribe_track_changes();
        let namespace = ns("room/123");
        let (_namespace_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");
        let (writer, reader) = Track::new(namespace.clone(), "audio").produce();
        let published_registration = locals
            .register_track(None, reader)
            .await
            .expect("published track should register");
        assert!(matches!(
            changes.recv().await.expect("published track added"),
            TrackChange::Added { .. }
        ));
        drop(writer);

        let cached = locals
            .get_or_request_track(None, namespace.clone(), "audio")
            .await
            .expect("closed published track should be replaced from namespace source");
        let request = requests.recv().await.expect("cache request");
        assert!(matches!(
            changes
                .recv()
                .await
                .expect("closed published track removed"),
            TrackChange::Removed { .. }
        ));

        drop(published_registration);
        assert!(locals
            .retrieve_track(None, &full(&namespace, "audio"))
            .is_some());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), changes.recv())
                .await
                .is_err(),
            "stale registration emitted a duplicate removal"
        );

        drop(cached);
        request.lease.finish_release();
        let (_replacement_writer, replacement_reader) =
            Track::new(namespace.clone(), "audio").produce();
        let _replacement = locals
            .register_track(None, replacement_reader)
            .await
            .expect("published replacement should register");
        assert!(matches!(
            changes.recv().await.expect("replacement track added"),
            TrackChange::Added { .. }
        ));
    }

    #[tokio::test]
    async fn get_or_request_track_uses_namespace_source_and_caches_reader() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let local = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested from namespace source");
        let reader = local.reader;
        assert!(
            local.interest.is_some(),
            "a cached track should hand back an interest guard"
        );
        assert!(
            local.upstream.is_some(),
            "a cached track should hand back an upstream readiness gate"
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

        let again = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("cached track should be returned");
        assert_eq!(again.reader.namespace, namespace);
        assert_eq!(again.reader.name, TrackName::from("video"));

        let no_second_request =
            tokio::time::timeout(std::time::Duration::from_millis(50), requests.recv()).await;
        assert!(
            no_second_request.is_err(),
            "cache hit should not request again"
        );
    }

    #[tokio::test]
    async fn new_pull_through_track_keeps_its_pre_publish_snapshot() {
        let mut locals = Locals::new();
        let namespace = ns("room/snapshot");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let local = locals
            .get_or_request_track(None, namespace, "video")
            .await
            .expect("track should resolve through namespace source");
        assert_eq!(local.largest_location, None);

        let request = requests.recv().await.expect("source should get request");
        let mut datagrams = request.writer.datagrams().unwrap();
        datagrams
            .write(moq_transport::serve::Datagram {
                group_id: 2,
                object_id: 3,
                priority: 128,
                status: moq_transport::data::ObjectStatus::NormalObject,
                end_of_group: false,
                payload: b"payload".to_vec().into(),
                extension_headers: moq_transport::data::ExtensionHeaders::default(),
            })
            .unwrap();

        assert_eq!(local.reader.largest_location(), Some(Location::new(2, 3)));
        assert_eq!(local.largest_location, None);
    }

    #[tokio::test]
    async fn cancelled_track_request_removes_cache_generation() {
        let mut locals = Locals::new();
        let namespace = ns("room/cancelled");
        let (requests, _request_reader) = mpsc::channel(1);
        locals
            .namespaces
            .write()
            .unwrap()
            .entry(UNSCOPED.to_string())
            .or_default()
            .insert(
                namespace.clone(),
                NamespaceEntry {
                    local: Some(NamespaceSource { requests }),
                    remote: Weak::new(),
                },
            );

        // Fill the source channel so the next miss is cancelled while sending
        // its TrackRequest, after reserving a cache generation.
        let _first = locals
            .get_or_request_track(None, namespace.clone(), "first")
            .await
            .expect("first request should fill the source channel");

        let mut task_locals = locals.clone();
        let task_namespace = namespace.clone();
        let task = tokio::spawn(async move {
            task_locals
                .get_or_request_track(None, task_namespace, "cancelled")
                .await
        });
        let cancelled = full(&namespace, "cancelled");

        for _ in 0..10 {
            if locals.retrieve_track(None, &cancelled).is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            locals.retrieve_track(None, &cancelled).is_some(),
            "pending request should reserve its cache generation"
        );

        task.abort();
        let join_error = match task.await {
            Err(err) => err,
            Ok(_) => panic!("request task should have been cancelled"),
        };
        assert!(join_error.is_cancelled());
        assert!(
            locals.retrieve_track(None, &cancelled).is_none(),
            "cancelling the request must remove its cache generation"
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

        let first_reader = first_reader
            .expect("first request should get a reader")
            .reader;
        let second_reader = second_reader
            .expect("second request should get cached reader")
            .reader;
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
        let local = locals
            .get_or_request_track(None, requested_ns.clone(), "video")
            .await
            .expect("track should be requested from longest prefix");
        assert_eq!(local.reader.namespace, requested_ns);

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
    /// Draft-16 §8.4: the gate must not resolve until the requester reports that
    /// the upstream subscription is established, because the caller sends
    /// SUBSCRIBE_OK as soon as it does.
    #[tokio::test]
    async fn upstream_gate_waits_for_the_requester() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let upstream = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested")
            .upstream
            .expect("a cached track should hand back an upstream readiness gate");
        let request = requests.recv().await.expect("source should get a request");

        let pending = tokio::time::timeout(Duration::from_millis(50), upstream.established()).await;
        assert!(
            pending.is_err(),
            "the gate must not resolve before the upstream subscription is established"
        );

        request.upstream.established();

        upstream
            .established()
            .await
            .expect("gate should resolve once the upstream subscription is established");
    }

    /// An upstream rejection has to surface as the same error, so the caller can
    /// answer REQUEST_ERROR with a matching code instead of accepting the request
    /// and reporting the failure later as PUBLISH_DONE.
    #[tokio::test]
    async fn upstream_gate_propagates_the_upstream_rejection() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let upstream = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested")
            .upstream
            .expect("cached track should hand back a gate");
        let request = requests.recv().await.expect("source should get a request");

        let rejection = ServeError::Closed(RequestErrorCode::DoesNotExist as u64);
        request.upstream.failed(rejection.clone());

        let err = upstream
            .established()
            .await
            .expect_err("a rejected upstream subscription must not resolve as established");
        assert_eq!(err, rejection);
    }

    /// A subscriber arriving mid-handshake is a cache hit on an entry that is not
    /// established yet, so it must wait on the same gate rather than be served a
    /// reader with no upstream subscription behind it.
    #[tokio::test]
    async fn upstream_gate_is_shared_by_concurrent_subscribers() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let first = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("first request")
            .upstream
            .expect("first gate");
        let request = requests.recv().await.expect("source should get a request");

        let second = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("second request")
            .upstream
            .expect("second gate");

        let pending = tokio::time::timeout(Duration::from_millis(50), second.established()).await;
        assert!(
            pending.is_err(),
            "a cache hit on an unestablished entry must still wait"
        );

        request.upstream.established();

        first
            .established()
            .await
            .expect("first subscriber should be released");
        second
            .established()
            .await
            .expect("second subscriber should be released");
        assert!(
            requests.try_recv().is_err(),
            "sharing the gate must not trigger a second upstream SUBSCRIBE"
        );
    }

    /// A requester that gives up without answering must release waiting
    /// subscribers with an error rather than stranding them: an abandoned request
    /// is not an established subscription.
    #[tokio::test]
    async fn abandoned_request_releases_waiting_subscribers() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let upstream = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested")
            .upstream
            .expect("cached track should hand back a gate");

        drop(requests.recv().await.expect("source should get a request"));

        let err = upstream
            .established()
            .await
            .expect_err("an abandoned request must not leave subscribers waiting forever");
        // The reason is spelled out rather than reported as an opaque internal
        // error: abandonment used to be indistinguishable from any other
        // internal fault once it reached the subscriber.
        assert!(
            matches!(&err, ServeError::Internal(reason) if reason.contains("upstream publisher went away")),
            "unexpected error: {err}"
        );
    }

    /// The production strand: the owning session gives up *before* it ever
    /// drains its request queue, so the request is destroyed while still
    /// buffered in the channel rather than after being received.
    ///
    /// This is `Consumer::serve` returning early after a failed coordinator
    /// registration or session teardown, once
    /// `register_namespace` has already published the route source. Distinct
    /// from `abandoned_request_releases_waiting_subscribers`, which drops a
    /// request that was successfully received.
    #[tokio::test]
    async fn requests_stranded_in_the_queue_release_waiting_subscribers() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let full_name = full(&namespace, "video");
        let (_registration, requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        // A downstream SUBSCRIBE lands in the window between the route source
        // being published and the owning session reaching its drain loop.
        let upstream = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested")
            .upstream
            .expect("cached track should hand back a gate");
        assert!(locals.contains_track_entry(None, &full_name));

        // The session aborts without ever calling `requests.recv()`.
        drop(requests);
        assert!(!locals.contains_track_entry(None, &full_name));

        let err = tokio::time::timeout(Duration::from_secs(5), upstream.established())
            .await
            .expect("a stranded request must not leave subscribers waiting")
            .expect_err("a stranded request is not an established subscription");
        assert!(
            matches!(&err, ServeError::Internal(reason) if reason.contains("upstream publisher went away")),
            "unexpected error: {err}"
        );
    }

    /// A resolved gate must survive the sender's drop.
    ///
    /// The sender outlives `established()` by the whole lifetime of the
    /// upstream subscription, so if its `Drop` overwrote the outcome, every
    /// subscriber arriving after a track stopped would be told the upstream
    /// failed when it had in fact succeeded.
    #[tokio::test]
    async fn dropping_a_resolved_sender_does_not_overwrite_the_outcome() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let upstream = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested")
            .upstream
            .expect("cached track should hand back a gate");

        let request = requests.recv().await.expect("source should get a request");
        request.upstream.established();
        drop(request);

        tokio::time::timeout(Duration::from_secs(5), upstream.established())
            .await
            .expect("a resolved gate must not block")
            .expect("dropping the sender must not turn success into failure");
    }

    /// The same guarantee for an explicit failure: the reported reason must not
    /// be replaced by the generic drop fallback.
    #[tokio::test]
    async fn dropping_a_failed_sender_preserves_the_reported_reason() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let upstream = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested")
            .upstream
            .expect("cached track should hand back a gate");

        let request = requests.recv().await.expect("source should get a request");
        request.upstream.failed(ServeError::NotFound);
        drop(request);

        let err = tokio::time::timeout(Duration::from_secs(5), upstream.established())
            .await
            .expect("a resolved gate must not block")
            .expect_err("the upstream failure should surface");
        assert!(
            matches!(err, ServeError::NotFound),
            "drop fallback overwrote the real reason: {err}"
        );
    }

    /// Resolving with every receiver already gone must still be recorded.
    ///
    /// `watch::Sender::send` refuses to update once the last receiver drops,
    /// which would leave a successfully established subscription sitting on
    /// `Pending`. The `Drop` fallback would treat that state as abandonment and report a
    /// spurious failure by the `Drop` fallback.
    ///
    /// Driven against the sender directly: in the registry the cache entry
    /// holds its own receiver, so the last-receiver case cannot be staged
    /// through `get_or_request_track`.
    #[test]
    fn outcome_is_recorded_after_the_last_receiver_is_gone() {
        let (state_tx, state_rx) = watch::channel(UpstreamState::Pending);
        let tx = UpstreamReadyTx { state: state_tx };

        drop(state_rx);
        tx.established();

        assert!(
            matches!(&*tx.state.borrow(), UpstreamState::Established),
            "the outcome must be recorded even with no receivers left"
        );
    }

    /// With the outcome recorded, `Drop` must leave it alone. Otherwise the
    /// fallback would rewrite a success into a failure.
    #[test]
    fn drop_does_not_rewrite_an_outcome_recorded_without_receivers() {
        let (state_tx, state_rx) = watch::channel(UpstreamState::Pending);
        let tx = UpstreamReadyTx { state: state_tx };

        // Genuinely zero receivers at the moment the outcome is reported; the
        // observer re-attaches only afterwards, standing in for a subscriber
        // that arrives while the track is still cached.
        drop(state_rx);
        tx.established();
        let observer = tx.state.subscribe();
        drop(tx);

        assert!(
            matches!(&*observer.borrow(), UpstreamState::Established),
            "drop must not overwrite a recorded success"
        );
    }

    /// An unresolved sender must fail the gate on drop rather than leaving it
    /// pending: that is what releases subscribers when a session is torn down
    /// while parked on the upstream SUBSCRIBE.
    #[test]
    fn drop_resolves_an_unreported_outcome_as_failed() {
        let (state_tx, state_rx) = watch::channel(UpstreamState::Pending);
        let tx = UpstreamReadyTx { state: state_tx };

        drop(tx);

        assert!(
            matches!(&*state_rx.borrow(), UpstreamState::Failed(_)),
            "an unresolved sender must fail the gate on drop"
        );
    }

    /// Locally published tracks are already established when they are registered,
    /// so serving one must not wait on anything.
    #[tokio::test]
    async fn published_track_has_no_upstream_gate() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (_writer, reader) = Track::new(namespace.clone(), "audio").produce();
        let _registration = locals
            .register_track(None, reader)
            .await
            .expect("track should register");

        let local = locals
            .get_or_request_track(None, namespace, "audio")
            .await
            .expect("published track should be served");

        assert!(
            local.upstream.is_none(),
            "a published track needs no upstream readiness gate"
        );
        assert!(
            local.interest.is_none(),
            "a published track has no upstream subscription to lease"
        );
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

        let guard = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested")
            .interest
            .expect("cached track should hand back a guard");

        let request = requests.recv().await.expect("source should get a request");
        let key = full(&namespace, "video");

        {
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
        }
        request.lease.finish_release();
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

        let local = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested");
        let request = requests.recv().await.expect("source should get a request");

        {
            let released = request.lease.released();
            tokio::pin!(released);

            drop(
                local
                    .interest
                    .expect("cached track should hand back a guard"),
            );

            tokio::select! {
                _ = &mut released => panic!("evicted before the grace period elapsed"),
                _ = tokio::time::sleep(idle / 2) => {}
            }

            // New subscriber inside the grace period: served from cache, no new request.
            let guard = locals
                .get_or_request_track(None, namespace.clone(), "video")
                .await
                .expect("warm entry should still be served")
                .interest
                .expect("cached track should hand back a guard");

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
        }
        request.lease.finish_release();
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

        let local = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested");

        // The subscriber goes away before the source has even picked up the
        // request, which is what a cancelled SUBSCRIBE looks like.
        drop(local);

        let request = requests.recv().await.expect("source should get a request");
        request.lease.released().await;
        request.lease.finish_release();

        assert!(
            locals
                .retrieve_track(None, &full(&namespace, "video"))
                .is_none(),
            "an abandoned request must not leave a permanently leased entry"
        );
    }

    #[tokio::test]
    async fn pending_request_is_released_without_cache_grace() {
        let idle = Duration::from_secs(30);
        let mut locals = Locals::with_cache_idle_timeout(idle);
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");

        let local = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested");
        let request = requests.recv().await.expect("source should get a request");
        drop(local);

        tokio::time::timeout(Duration::from_secs(1), request.lease.abandoned())
            .await
            .expect("pending request retained the cache grace period");
        request.lease.finish_release();
        assert!(locals
            .retrieve_track(None, &full(&namespace, "video"))
            .is_none());
    }

    #[tokio::test]
    async fn closed_cache_generation_blocks_replacement_until_lease_finishes() {
        let mut locals = Locals::new();
        let namespace = ns("room/123");
        let (_registration, mut requests) = locals
            .register_namespace(None, namespace.clone())
            .await
            .expect("namespace source should register");
        let first = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("first request");
        let TrackRequest {
            writer,
            lease,
            upstream,
        } = requests.recv().await.expect("first request received");

        drop(writer);
        drop(upstream);

        let replacement = locals.get_or_request_track(None, namespace.clone(), "video");
        tokio::pin!(replacement);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut replacement)
                .await
                .is_err(),
            "a replacement overtook the closing cache generation"
        );
        assert!(requests.try_recv().is_err());

        lease.finish_release();
        let second = tokio::time::timeout(Duration::from_secs(1), &mut replacement)
            .await
            .expect("replacement remained blocked after teardown")
            .expect("replacement request");
        let second_request = requests.recv().await.expect("second request received");

        drop(first);
        drop(second);
        second_request.lease.finish_release();
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

        let local = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("missing track should be requested");
        let request = requests.recv().await.expect("source should get a request");

        drop(local);

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

        let first = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("first request");
        let first_request = requests.recv().await.expect("first request received");

        // Drop the first generation's entry while deliberately keeping its guard
        // alive, modelling a slow subscriber that outlives its cache entry.
        let stale_guard = first.interest.expect("guard");
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
        let second = locals
            .get_or_request_track(None, namespace.clone(), "video")
            .await
            .expect("second request");
        let second_request = requests.recv().await.expect("second request received");
        drop(second.interest.expect("guard"));

        // The stale guard belongs to the old counter, so it must not make the new
        // entry look busy.
        let _ = &stale_guard;
        second_request.lease.released().await;
        second_request.lease.finish_release();

        assert!(
            locals.retrieve_track(None, &key).is_none(),
            "a stale guard must not pin a replacement entry"
        );
    }
}
