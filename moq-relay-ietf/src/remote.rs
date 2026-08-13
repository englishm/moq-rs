// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use moq_native_ietf::quic;
use moq_transport::coding::TrackNamespace;
use moq_transport::serve::{Track, TrackInterest, TrackInterestGuard, TrackReader};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{metrics::GaugeGuard, Coordinator, CoordinatorError, RelayTuning};

/// Cache key for upstream relay-to-relay connections.
///
/// Keyed by both URL and destination address so that connections are reused
/// only when both match.
type RemoteCacheKey = (Url, Option<SocketAddr>);
type RemoteSlot = Arc<Mutex<Option<Remote>>>;
type TrackCacheKey = (TrackNamespace, String);
type TrackSlot = Arc<Mutex<Option<CachedTrack>>>;

/// A cached cross-relay track reader plus the downstream interest in it.
#[derive(Clone)]
struct CachedTrack {
    reader: TrackReader,

    /// Interest in this cached reader. When it goes idle for the configured
    /// grace period the upstream subscription to the peer relay is released.
    interest: TrackInterest,
}

/// Live users of one pooled peer connection.
///
/// Eviction only reclaims an entry whose count is zero, so a connection can
/// never be closed out from under something still using it. Guards are taken
/// under the pool lock — the same lock eviction runs under — or by a caller
/// that already holds one, so a connection cannot be handed out and evicted at
/// the same time.
#[derive(Clone, Debug, Default)]
struct RemoteUsers {
    count: Arc<AtomicUsize>,
}

impl RemoteUsers {
    fn guard(&self) -> RemoteUseGuard {
        self.count.fetch_add(1, Ordering::AcqRel);
        RemoteUseGuard {
            users: self.clone(),
        }
    }

    fn is_idle(&self) -> bool {
        self.count.load(Ordering::Acquire) == 0
    }
}

/// Held for as long as a caller is using a pooled connection to a peer relay.
///
/// Dropping it only makes the connection eligible for eviction again; it never
/// closes anything by itself.
#[derive(Debug)]
struct RemoteUseGuard {
    users: RemoteUsers,
}

impl RemoteUseGuard {
    /// The counter behind this guard, so a longer lived user of the same
    /// connection can take its own guard.
    fn users(&self) -> RemoteUsers {
        self.users.clone()
    }
}

impl Drop for RemoteUseGuard {
    fn drop(&mut self) {
        // Saturating because an underflow here would make a connection look
        // permanently busy, pinning it in the pool forever.
        let _ = self
            .users
            .count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                Some(count.saturating_sub(1))
            });
    }
}

/// One pooled peer connection: the slot holding it plus its LRU bookkeeping.
struct RemoteEntry {
    slot: RemoteSlot,
    users: RemoteUsers,

    /// Tick of the last time this entry was handed out; the lowest tick is the
    /// least recently used entry.
    used: u64,
}

/// Bounded pool of connections to peer relays.
///
/// Recency is a monotonic tick rather than an intrusive list: the pool holds a
/// few hundred entries at most and only a new connection — already a QUIC
/// handshake — can trigger a scan, so the linear walk is not worth a second
/// data structure to keep in sync.
#[derive(Default)]
struct RemoteCache {
    entries: HashMap<RemoteCacheKey, RemoteEntry>,
    clock: u64,
}

impl RemoteCache {
    /// Get or create the slot for `key`, mark it most recently used, and take a
    /// use guard so it cannot be evicted while the caller works with it.
    fn acquire(&mut self, key: &RemoteCacheKey) -> (RemoteSlot, RemoteUseGuard) {
        self.clock += 1;
        let used = self.clock;

        let entry = self
            .entries
            .entry(key.clone())
            .or_insert_with(|| RemoteEntry {
                slot: Arc::new(Mutex::new(None)),
                users: RemoteUsers::default(),
                used,
            });

        // Recency is refreshed on a hit, not just on insertion.
        entry.used = used;

        (entry.slot.clone(), entry.users.guard())
    }

    /// The slot cached for `key`, without touching its recency.
    fn slot(&self, key: &RemoteCacheKey) -> Option<&RemoteSlot> {
        self.entries.get(key).map(|entry| &entry.slot)
    }

    fn is_current(&self, key: &RemoteCacheKey, slot: &RemoteSlot) -> bool {
        matches!(self.slot(key), Some(current) if Arc::ptr_eq(current, slot))
    }

    fn remove(&mut self, key: &RemoteCacheKey) {
        self.entries.remove(key);
    }

    fn drain(&mut self) -> Vec<(RemoteCacheKey, RemoteSlot)> {
        self.entries
            .drain()
            .map(|(key, entry)| (key, entry.slot))
            .collect()
    }

    /// Take the least recently used unused connections until at most `capacity`
    /// entries remain. A zero `capacity` disables the bound.
    ///
    /// An entry with a live user is skipped, as is one whose slot lock is held
    /// (a connect or teardown is in flight), so the pool is left over capacity
    /// rather than cutting off a connection that is still serving someone. The
    /// evicted connections are returned instead of closed here because shutdown
    /// is async and this runs under the pool lock.
    fn evict_over_capacity(&mut self, capacity: usize) -> Vec<(RemoteCacheKey, Option<Remote>)> {
        if capacity == 0 {
            return Vec::new();
        }

        let mut evicted = Vec::new();

        while self.entries.len() > capacity {
            let mut candidates: Vec<(u64, RemoteCacheKey)> = self
                .entries
                .iter()
                .filter(|(_, entry)| entry.users.is_idle())
                .map(|(key, entry)| (entry.used, key.clone()))
                .collect();
            candidates.sort_unstable_by_key(|(used, _)| *used);

            let victim = candidates.into_iter().find_map(|(_, key)| {
                // try_lock never blocks, so probing a slot while holding the
                // pool lock cannot deadlock against a task that takes the slot
                // lock first.
                let mut cached = self.entries.get(&key)?.slot.try_lock().ok()?;
                Some((key, cached.take()))
            });

            let Some((key, remote)) = victim else {
                tracing::debug!(
                    entries = self.entries.len(),
                    capacity,
                    "remote connection pool is over capacity but every connection is in use"
                );
                break;
            };

            self.entries.remove(&key);
            evicted.push((key, remote));
        }

        evicted
    }
}

/// Manages connections to remote relays.
///
/// When a subscription request comes in for a namespace that isn't local,
/// RemoteManager uses the coordinator to find which remote relay serves it,
/// establishes a connection if needed, and subscribes to the track.
#[derive(Clone)]
pub struct RemoteManager {
    coordinator: Arc<dyn Coordinator>,
    clients: Vec<quic::Client>,
    remotes: Arc<Mutex<RemoteCache>>,

    /// Timeouts and limits applied to this relay's upstream connections.
    tuning: RelayTuning,
}

impl RemoteManager {
    /// Create a new RemoteManager.
    pub fn new(coordinator: Arc<dyn Coordinator>, clients: Vec<quic::Client>) -> Self {
        Self {
            coordinator,
            clients,
            remotes: Arc::new(Mutex::new(RemoteCache::default())),
            tuning: RelayTuning::default(),
        }
    }

    /// Override how long an unwatched cross-relay cache entry is retained before
    /// its upstream subscription is released.
    ///
    /// A zero timeout disables idle eviction, holding subscriptions to peer
    /// relays for as long as the peer session lives.
    pub fn with_cache_idle_timeout(mut self, cache_idle_timeout: Duration) -> Self {
        self.tuning.cache_idle_timeout = cache_idle_timeout;
        self
    }

    /// Override how long to wait for a peer relay to acknowledge a SUBSCRIBE.
    pub fn with_subscribe_timeout(mut self, subscribe_timeout: Duration) -> Self {
        self.tuning.subscribe_timeout = subscribe_timeout;
        self
    }

    /// Override how many peer relay connections are pooled.
    ///
    /// Adding a connection to a full pool closes the least recently used
    /// connection that nothing is using. Connections still serving a
    /// subscription are never closed, so the pool may exceed this limit while
    /// every entry is in use. Zero disables the bound, restoring the previous
    /// behaviour of caching a connection per peer for the life of the process.
    ///
    /// A connection counts as unused once every subscription over it has been
    /// released, which for cached tracks is governed by the cache idle timeout.
    pub fn with_max_remote_connections(mut self, max_remote_connections: usize) -> Self {
        self.tuning.max_remote_connections = max_remote_connections;
        self
    }

    /// Subscribe to a track from a remote relay.
    ///
    /// `scope` is the resolved scope identity from `Coordinator::resolve_scope()`,
    /// passed through to the coordinator's `lookup()` to scope the search.
    ///
    /// Returns None if the namespace isn't found in any remote relay.
    pub async fn subscribe(
        &self,
        scope: Option<&str>,
        namespace: &TrackNamespace,
        track_name: &str,
    ) -> anyhow::Result<Option<(TrackReader, TrackInterestGuard)>> {
        let (origin, client) = match self.coordinator.lookup(scope, namespace).await {
            Ok(result) => result,
            Err(CoordinatorError::NamespaceNotFound) => return Ok(None),
            Err(err) => return Err(err.into()),
        };

        let url = origin.url();
        let cache_key = (url.clone(), origin.addr());

        // The guard keeps this connection out of the pool's eviction candidate
        // set until the upstream subscription has taken one of its own.
        let (remote, _peer_use) = match self
            .get_or_connect(cache_key.clone(), client.as_ref())
            .await
        {
            Ok(remote) => remote,
            Err(err) => {
                tracing::error!(remote_url = %url, error = %err, "failed to connect to remote relay: {}", err);
                return Err(err);
            }
        };

        match remote
            .subscribe(namespace.clone(), track_name.to_string())
            .await
        {
            Ok(reader) => Ok(reader),
            Err(err) => {
                tracing::warn!(remote_url = %url, error = %err, "remote subscribe failed, removing from cache");
                self.remove_if_same_remote(&cache_key, &remote).await;

                Err(err)
            }
        }
    }

    /// Get an existing remote connection or create a new one.
    ///
    /// The returned guard keeps the connection in the pool: it is only evicted
    /// once every guard on it has been dropped.
    async fn get_or_connect(
        &self,
        cache_key: RemoteCacheKey,
        client: Option<&quic::Client>,
    ) -> anyhow::Result<(Remote, RemoteUseGuard)> {
        let client = match client {
            Some(client) => client,
            None => self.clients.first().ok_or_else(|| {
                anyhow::anyhow!("no QUIC clients configured for remote connections")
            })?,
        };

        loop {
            // The manager lock only protects the pool. The per-key slot lock protects
            // that key's connection state, so unrelated remotes can connect in parallel.
            // The use guard is taken under the manager lock, which is also where
            // eviction picks its victims, so this entry cannot be evicted from
            // under us.
            let (slot, peer_use) = {
                let mut remotes = self.remotes.lock().await;
                remotes.acquire(&cache_key)
            };

            let mut cached = slot.lock().await;

            let is_current_slot = {
                let remotes = self.remotes.lock().await;
                remotes.is_current(&cache_key, &slot)
            };

            if !is_current_slot {
                continue;
            }

            if let Some(remote) = cached.as_ref() {
                if remote.is_connected() {
                    return Ok((remote.clone(), peer_use));
                }

                tracing::info!(remote_url = %cache_key.0, "removing dead connection to remote relay");
            };

            if let Some(remote) = cached.take() {
                remote.shutdown().await;
            }

            tracing::info!(remote_url = %cache_key.0, "connecting to remote relay");
            let remote = match Remote::connect(
                cache_key.0.clone(),
                cache_key.1,
                client,
                self.tuning,
                Arc::downgrade(&self.remotes),
                cache_key.clone(),
                Arc::downgrade(&slot),
                peer_use.users(),
            )
            .await
            {
                Ok(remote) => remote,
                Err(err) => {
                    drop(cached);
                    remove_empty_remote_slot(&self.remotes, &cache_key, &slot).await;
                    return Err(err);
                }
            };

            *cached = Some(remote.clone());
            drop(cached);

            // This connection may have pushed the pool over its limit. Run after
            // releasing the slot lock so nobody waits on this key while the
            // evicted connections are shut down.
            self.evict_over_capacity().await;

            return Ok((remote, peer_use));
        }
    }

    /// Close pooled connections that nothing is using, least recently used
    /// first, until the pool is back within its limit.
    async fn evict_over_capacity(&self) {
        let evicted = {
            let mut remotes = self.remotes.lock().await;
            remotes.evict_over_capacity(self.tuning.max_remote_connections)
        };

        // Shutdown is async, so it happens after the pool lock is released.
        for (cache_key, remote) in evicted {
            tracing::info!(remote_url = %cache_key.0, "evicting least recently used peer relay from the connection pool");
            metrics::counter!("moq_relay_remote_cache_evictions_total").increment(1);

            if let Some(remote) = remote {
                remote.shutdown().await;
            }
        }
    }

    async fn remove_if_same_remote(&self, cache_key: &RemoteCacheKey, remote: &Remote) {
        let slot = {
            let remotes = self.remotes.lock().await;
            remotes.slot(cache_key).cloned()
        };

        if let Some(slot) = slot {
            let removed = {
                let mut cached = slot.lock().await;
                match cached.as_ref() {
                    Some(current) if current.is_same_connection(remote) => cached.take(),
                    _ => None,
                }
            };

            if let Some(remote) = removed {
                remote.shutdown().await;
                remove_empty_remote_slot(&self.remotes, cache_key, &slot).await;
            }
        }
    }

    /// Shutdown all remote connections.
    pub(crate) async fn shutdown(&self) {
        let remotes = {
            let mut remotes = self.remotes.lock().await;
            remotes.drain()
        };

        for (cache_key, slot) in remotes {
            tracing::info!(remote_url = %cache_key.0, "shutting down remote connection");
            let mut remote = slot.lock().await;
            if let Some(remote) = remote.take() {
                remote.shutdown().await;
            }
        }
    }
}

async fn remove_empty_remote_slot(
    remotes: &Arc<Mutex<RemoteCache>>,
    cache_key: &RemoteCacheKey,
    slot: &RemoteSlot,
) {
    let cached = slot.lock().await;
    if cached.is_some() {
        return;
    }

    let mut remotes = remotes.lock().await;
    if remotes.is_current(cache_key, slot) {
        remotes.remove(cache_key);
    }
}

/// Clear a cached cross-relay track once it has been unwatched for `timeout`.
///
/// Resolving means the caller should drop its subscription to the peer relay. The
/// slot is cleared *before* returning, while the lock is still held, so a
/// subscriber arriving afterwards misses the cache and re-subscribes instead of
/// attaching to a reader whose upstream subscription is being torn down.
///
/// The idle re-check happens under the slot lock, which is also where interest
/// guards are created, so a subscriber racing eviction is either counted here
/// (and we keep waiting) or misses the slot entirely.
///
/// Never resolves when `timeout` is zero, preserving the previous behaviour of
/// holding the subscription for as long as the peer session lives.
async fn evict_when_idle(slot: &TrackSlot, interest: &TrackInterest, timeout: Duration) {
    if timeout.is_zero() {
        // Idle eviction disabled.
        std::future::pending::<()>().await;
    }

    loop {
        interest.idle_for(timeout).await;

        let mut cached = slot.lock().await;

        // A different generation now owns the slot; it has its own idle timer, so
        // this subscription is no longer serving anything and must not clear it.
        let ours = matches!(
            cached.as_ref(),
            Some(current) if current.interest.same_generation(interest)
        );

        if !ours {
            return;
        }

        if interest.is_idle() {
            cached.take();
            return;
        }

        // Interest returned between the timer firing and taking the lock, so keep
        // serving and start waiting again.
        drop(cached);
    }
}

async fn remove_empty_track_slot(
    tracks: &Arc<Mutex<HashMap<TrackCacheKey, TrackSlot>>>,
    key: &TrackCacheKey,
    slot: &TrackSlot,
) {
    let cached = slot.lock().await;
    if cached.is_some() {
        return;
    }

    let mut tracks = tracks.lock().await;
    if matches!(tracks.get(key), Some(current) if Arc::ptr_eq(current, slot)) {
        tracks.remove(key);
    }
}

/// A connection to a single remote relay with its own QUIC client.
#[derive(Clone)]
struct Remote {
    url: Url,
    subscriber: moq_transport::session::Subscriber,
    /// Track subscriptions keyed by full track name.
    tracks: Arc<Mutex<HashMap<TrackCacheKey, TrackSlot>>>,
    /// Flag indicating if the connection is still alive.
    connected: Arc<AtomicBool>,
    /// Cancellation token for the session task.
    cancel: CancellationToken,
    /// Timeouts applied to this peer's track subscriptions.
    tuning: RelayTuning,
    /// Live users of this pooled connection, shared with its cache entry.
    users: RemoteUsers,
}

impl Remote {
    /// Connect to a remote relay with a dedicated QUIC client.
    async fn connect(
        url: Url,
        addr: Option<SocketAddr>,
        client: &quic::Client,
        tuning: RelayTuning,
        remotes: Weak<Mutex<RemoteCache>>,
        cache_key: RemoteCacheKey,
        cache_slot: Weak<Mutex<Option<Remote>>>,
        users: RemoteUsers,
    ) -> anyhow::Result<Self> {
        let (session, _quic_client_initial_cid, transport) = match client.connect(&url, addr).await
        {
            Ok(session) => session,
            Err(err) => {
                metrics::counter!("moq_relay_upstream_errors_total", "stage" => "connect")
                    .increment(1);
                return Err(err);
            }
        };

        let (session, subscriber) =
            match moq_transport::session::Subscriber::connect(session, transport).await {
                Ok(session) => session,
                Err(err) => {
                    metrics::counter!("moq_relay_upstream_errors_total", "stage" => "session")
                        .increment(1);
                    return Err(err.into());
                }
            };

        let connected = Arc::new(AtomicBool::new(true));
        let cancel = CancellationToken::new();
        let upstream_guard = GaugeGuard::new("moq_relay_upstream_connections");

        let session_url = url.clone();
        let session_connected = connected.clone();
        let session_cancel = cancel.clone();

        tokio::spawn(async move {
            let _upstream_guard = upstream_guard;
            tokio::select! {
                result = session.run() => {
                    if let Err(err) = result {
                        tracing::warn!(remote_url = %session_url, error = %err, "remote session closed: {}", err);
                    } else {
                        tracing::info!(remote_url = %session_url, "remote session closed normally");
                    }
                }
                _ = session_cancel.cancelled() => {
                    tracing::info!(remote_url = %session_url, "remote session cancelled");
                }
            }

            session_connected.store(false, Ordering::Release);

            if let Some(cache_slot) = cache_slot.upgrade() {
                let mut cleared = false;
                let mut cached = cache_slot.lock().await;
                if matches!(cached.as_ref(), Some(remote) if Arc::ptr_eq(&remote.connected, &session_connected))
                {
                    cached.take();
                    cleared = true;
                    tracing::info!(remote_url = %session_url, "cleared closed remote connection from cache");
                }
                drop(cached);

                if cleared {
                    if let Some(remotes) = remotes.upgrade() {
                        remove_empty_remote_slot(&remotes, &cache_key, &cache_slot).await;
                    }
                }
            }
        });

        Ok(Self {
            url,
            subscriber,
            tracks: Arc::new(Mutex::new(HashMap::new())),
            connected,
            cancel,
            tuning,
            users,
        })
    }

    /// Check if the connection is still alive.
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    fn is_same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.connected, &other.connected)
    }

    /// Shutdown the remote connection.
    async fn shutdown(&self) {
        self.cancel.cancel();
        self.connected.store(false, Ordering::Release);
        self.tracks.lock().await.clear();
    }

    /// Subscribe to a track on this remote relay.
    async fn subscribe(
        &self,
        namespace: TrackNamespace,
        track_name: String,
    ) -> anyhow::Result<Option<(TrackReader, TrackInterestGuard)>> {
        let key = (namespace.clone(), track_name.clone());

        loop {
            if !self.is_connected() {
                anyhow::bail!("remote connection to {} is closed", self.url);
            }

            let slot = {
                let mut tracks = self.tracks.lock().await;
                tracks
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(None)))
                    .clone()
            };

            let mut cached = slot.lock().await;

            let is_current_slot = {
                let tracks = self.tracks.lock().await;
                matches!(tracks.get(&key), Some(current) if Arc::ptr_eq(current, &slot))
            };

            if !is_current_slot {
                continue;
            }

            if let Some(entry) = cached.as_ref() {
                if !entry.reader.is_closed() {
                    // Register interest while still holding the slot lock, which
                    // is the same lock idle eviction re-checks under.
                    return Ok(Some((entry.reader.clone(), entry.interest.guard())));
                }

                tracing::debug!(remote_url = %self.url, namespace = %key.0, track = %key.1, "removing closed remote track from cache");
            }

            cached.take();

            let mut subscriber = self.subscriber.clone();
            let url = self.url.clone();
            let tracks = Arc::downgrade(&self.tracks);
            let cancel = self.cancel.clone();

            tracing::info!(remote_url = %url, namespace = %key.0, track = %key.1, "subscribing to remote track");

            let (writer, reader) = Track::new(namespace.clone(), track_name.clone()).produce();

            // `subscribe_open` waits for SUBSCRIBE_OK, which a peer relay is under
            // no obligation to ever send, so the handshake is bounded three ways:
            // the peer connection closing, a wall-clock timeout, and — once the
            // entry exists — idle eviction. Dropping the in-flight future drops the
            // `Subscribe`, whose `Drop` sends UNSUBSCRIBE, so giving up here leaves
            // nothing dangling on the peer.
            let subscribe_result = tokio::select! {
                result = tokio::time::timeout(self.tuning.subscribe_timeout, subscriber.subscribe_open(writer)) => result,
                _ = cancel.cancelled() => {
                    drop(cached);
                    remove_empty_track_slot(&self.tracks, &key, &slot).await;
                    anyhow::bail!("subscribe cancelled, remote connection to {} is closed", self.url);
                }
            };

            let subscribe = match subscribe_result {
                Ok(Ok(subscribe)) => subscribe,
                Ok(Err(err)) => {
                    drop(cached);
                    remove_empty_track_slot(&self.tracks, &key, &slot).await;
                    return Err(err.into());
                }
                Err(_elapsed) => {
                    tracing::warn!(remote_url = %url, namespace = %key.0, track = %key.1, timeout = ?self.tuning.subscribe_timeout, "remote relay did not acknowledge SUBSCRIBE in time");
                    metrics::counter!("moq_relay_subscribe_timeouts_total", "source" => "remote")
                        .increment(1);
                    drop(cached);
                    remove_empty_track_slot(&self.tracks, &key, &slot).await;
                    anyhow::bail!(
                        "remote relay {} did not acknowledge SUBSCRIBE within {:?}",
                        self.url,
                        self.tuning.subscribe_timeout
                    );
                }
            };

            if !self.is_connected() {
                drop(cached);
                remove_empty_track_slot(&self.tracks, &key, &slot).await;
                anyhow::bail!("remote connection to {} is closed", self.url);
            }

            let interest = TrackInterest::new();

            // Take this caller's guard before the entry becomes visible, so the
            // cleanup task cannot see a brand new entry as idle.
            let guard = interest.guard();

            *cached = Some(CachedTrack {
                reader: reader.clone(),
                interest: interest.clone(),
            });
            drop(cached);

            let cleanup_key = key.clone();
            let cleanup_reader = reader.clone();
            let cleanup_slot = slot.clone();
            let idle_timeout = self.tuning.cache_idle_timeout;

            // Pins the peer connection for as long as this upstream subscription
            // lives, so pool eviction cannot close it underneath a downstream
            // subscriber. Taken before the task is spawned, while the caller's
            // own guard is still alive, so the connection is never momentarily
            // unused.
            let peer_use = self.users.guard();

            tokio::spawn(async move {
                let _peer_use = peer_use;

                tokio::select! {
                    result = subscribe.closed() => {
                        match result {
                            Ok(()) => {
                                tracing::debug!(remote_url = %url, namespace = %cleanup_key.0, track = %cleanup_key.1, "remote track subscription ended");
                            }
                            Err(err) => {
                                tracing::warn!(remote_url = %url, namespace = %cleanup_key.0, track = %cleanup_key.1, error = %err, "remote track subscription ended with error: {}", err);
                            }
                        }
                    }
                    _ = cancel.cancelled() => {
                        tracing::debug!(remote_url = %url, namespace = %cleanup_key.0, track = %cleanup_key.1, "remote track subscription cancelled");
                    }
                    // Nobody downstream is watching this cross-relay track any
                    // more, so stop pulling it from the peer relay. The slot is
                    // already cleared by the time this resolves, so a later
                    // subscriber re-subscribes rather than attaching to a reader
                    // that is about to go silent.
                    _ = evict_when_idle(&cleanup_slot, &interest, idle_timeout) => {
                        tracing::info!(remote_url = %url, namespace = %cleanup_key.0, track = %cleanup_key.1, "releasing idle remote track subscription");
                        metrics::counter!("moq_relay_cache_idle_evictions_total", "source" => "remote").increment(1);
                    }
                }

                // Sends UNSUBSCRIBE to the peer relay if it is still open.
                drop(subscribe);

                if let Some(tracks) = tracks.upgrade() {
                    let mut cached = cleanup_slot.lock().await;
                    if matches!(cached.as_ref(), Some(current) if Arc::ptr_eq(&current.reader.info, &cleanup_reader.info))
                    {
                        cached.take();
                    }
                    drop(cached);

                    remove_empty_track_slot(&tracks, &cleanup_key, &cleanup_slot).await;
                }
            });

            return Ok(Some((reader, guard)));
        }
    }
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote")
            .field("url", &self.url.to_string())
            .field("connected", &self.is_connected())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: Duration = Duration::from_secs(30);

    fn pool_key(host: &str) -> RemoteCacheKey {
        (
            Url::parse(&format!("https://{host}.example.com/live")).unwrap(),
            None,
        )
    }

    /// Insert `hosts` in order, releasing each use guard so every entry is
    /// evictable, and return the pool.
    fn unused_pool(hosts: &[&str]) -> RemoteCache {
        let mut cache = RemoteCache::default();
        for host in hosts {
            let (_slot, _peer_use) = cache.acquire(&pool_key(host));
        }
        cache
    }

    fn evicted_keys(evicted: Vec<(RemoteCacheKey, Option<Remote>)>) -> Vec<RemoteCacheKey> {
        evicted.into_iter().map(|(key, _)| key).collect()
    }

    #[test]
    fn acquire_reuses_the_slot_for_a_key() {
        let mut cache = RemoteCache::default();

        let (first, _) = cache.acquire(&pool_key("a"));
        let (second, _) = cache.acquire(&pool_key("a"));

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn insertion_beyond_capacity_evicts_the_least_recently_used() {
        let mut cache = unused_pool(&["a", "b", "c"]);

        assert_eq!(evicted_keys(cache.evict_over_capacity(2)), [pool_key("a")]);
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.slot(&pool_key("a")).is_none());
        assert!(cache.slot(&pool_key("b")).is_some());
        assert!(cache.slot(&pool_key("c")).is_some());
    }

    #[test]
    fn a_cache_hit_refreshes_recency() {
        let mut cache = unused_pool(&["a", "b", "c"]);

        // Reusing the oldest connection makes the next oldest the victim.
        let (_slot, _peer_use) = cache.acquire(&pool_key("a"));

        assert_eq!(evicted_keys(cache.evict_over_capacity(2)), [pool_key("b")]);
        assert!(cache.slot(&pool_key("a")).is_some());
    }

    #[test]
    fn an_entry_in_use_is_not_torn_down() {
        let mut cache = unused_pool(&["a"]);
        let (_slot, in_use) = cache.acquire(&pool_key("a"));
        let (_newer, _) = cache.acquire(&pool_key("b"));

        // "a" is the least recently used but is still serving someone, so the
        // newer unused entry is reclaimed instead.
        assert_eq!(evicted_keys(cache.evict_over_capacity(1)), [pool_key("b")]);
        assert!(cache.slot(&pool_key("a")).is_some());

        // ...and it becomes evictable again once its last user goes away.
        drop(in_use);
        let (_newer, _) = cache.acquire(&pool_key("c"));

        assert_eq!(evicted_keys(cache.evict_over_capacity(1)), [pool_key("a")]);
    }

    #[test]
    fn a_pool_of_connections_in_use_is_left_over_capacity() {
        let mut cache = RemoteCache::default();
        let (_a, _a_use) = cache.acquire(&pool_key("a"));
        let (_b, _b_use) = cache.acquire(&pool_key("b"));

        assert!(cache.evict_over_capacity(1).is_empty());
        assert_eq!(
            cache.entries.len(),
            2,
            "an entry in use must never be evicted"
        );
    }

    #[test]
    fn a_busy_slot_is_skipped() {
        let mut cache = unused_pool(&["a", "b"]);
        let slot = cache.slot(&pool_key("a")).unwrap().clone();

        // A connect or teardown holds the slot lock; leave that entry alone.
        let _connecting = slot.try_lock().unwrap();

        assert_eq!(evicted_keys(cache.evict_over_capacity(1)), [pool_key("b")]);
        assert!(cache.slot(&pool_key("a")).is_some());
    }

    #[test]
    fn a_zero_capacity_disables_the_bound() {
        let mut cache = unused_pool(&["a", "b", "c"]);

        assert!(cache.evict_over_capacity(0).is_empty());
        assert_eq!(cache.entries.len(), 3);
    }

    fn cached_track() -> (TrackSlot, TrackInterest) {
        let (_writer, reader) = Track::new(
            TrackNamespace::from_utf8_path("example.com"),
            "video".to_string(),
        )
        .produce();
        let interest = TrackInterest::new();
        let slot: TrackSlot = Arc::new(Mutex::new(Some(CachedTrack {
            reader,
            interest: interest.clone(),
        })));
        (slot, interest)
    }

    /// The slot must be cleared before the caller drops its peer subscription, so
    /// a later subscriber re-subscribes instead of attaching to a dying reader.
    #[tokio::test(start_paused = true)]
    async fn evict_when_idle_clears_the_slot_once_unwatched() {
        let (slot, interest) = cached_track();

        evict_when_idle(&slot, &interest, GRACE).await;

        assert!(
            slot.lock().await.is_none(),
            "slot must be cleared before the subscription is released"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn evict_when_idle_waits_for_the_last_subscriber() {
        let (slot, interest) = cached_track();
        let guard = interest.guard();

        let evict = evict_when_idle(&slot, &interest, GRACE);
        tokio::pin!(evict);

        tokio::select! {
            _ = &mut evict => panic!("evicted a track that still has a subscriber"),
            _ = tokio::time::sleep(GRACE * 4) => {}
        }
        assert!(slot.lock().await.is_some());

        drop(guard);
        evict.await;
        assert!(slot.lock().await.is_none());
    }

    /// A replacement generation owns the slot now, so this subscription is stale:
    /// it should be released without disturbing the new entry.
    #[tokio::test(start_paused = true)]
    async fn evict_when_idle_leaves_a_replacement_generation_alone() {
        let (slot, interest) = cached_track();
        let (_replacement_slot, replacement_interest) = cached_track();

        {
            let mut cached = slot.lock().await;
            let reader = cached.as_ref().unwrap().reader.clone();
            *cached = Some(CachedTrack {
                reader,
                interest: replacement_interest.clone(),
            });
        }

        // Resolves (so the stale subscription is dropped) but must not clear the
        // slot the replacement is using.
        evict_when_idle(&slot, &interest, GRACE).await;

        assert!(
            slot.lock().await.is_some(),
            "a stale lease must not evict the replacement entry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn evict_when_idle_is_disabled_by_a_zero_timeout() {
        let (slot, interest) = cached_track();

        tokio::select! {
            _ = evict_when_idle(&slot, &interest, Duration::ZERO) => {
                panic!("a zero timeout must disable eviction")
            }
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }

        assert!(slot.lock().await.is_some());
    }
}
