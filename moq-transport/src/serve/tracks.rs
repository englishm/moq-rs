// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A broadcast is a collection of tracks, split into two handles: [Writer] and [Reader].
//!
//! The [Writer] can create tracks, either manually or on request.
//! It receives all requests by a [Reader] for a tracks that don't exist.
//! The simplest implementation is to close every unknown track with [ServeError::NotFound].
//!
//! A [Reader] can request tracks by name.
//! If the track already exists, it will be returned.
//! If the track doesn't exist, it will be sent to [Unknown] to be handled.
//! A [Reader] can be cloned to create multiple subscriptions.
//!
//! The broadcast is automatically closed with [ServeError::Done] when [Writer] is dropped, or all [Reader]s are dropped.
use std::{collections::HashMap, ops::Deref, sync::Arc, time::Duration};

use super::{
    ServeError, Track, TrackInterest, TrackInterestGuard, TrackReader, TrackWriter,
    DEFAULT_CACHE_IDLE_TIMEOUT,
};
use crate::coding::TrackNamespace;
use crate::watch::{Queue, State, StateWeak};

/// Full track identifier: namespace + track name
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct FullTrackName {
    pub namespace: TrackNamespace,
    pub name: String,
}

/// Static information about a broadcast.
#[derive(Debug)]
pub struct Tracks {
    pub namespace: TrackNamespace,
}

impl Tracks {
    pub fn new(namespace: TrackNamespace) -> Self {
        Self { namespace }
    }

    pub fn produce(self) -> (TracksWriter, TracksRequest, TracksReader) {
        self.produce_with_cache_idle_timeout(DEFAULT_CACHE_IDLE_TIMEOUT)
    }

    /// Produce a broadcast whose pull-through cache entries are evicted after
    /// `cache_idle_timeout` with no downstream subscribers.
    ///
    /// A zero timeout disables eviction, restoring the previous behaviour of
    /// holding upstream subscriptions for as long as the session lives.
    ///
    /// This is a separate constructor rather than a field on [`Tracks`] so that
    /// existing `Tracks { namespace }` construction keeps compiling.
    pub fn produce_with_cache_idle_timeout(
        self,
        cache_idle_timeout: Duration,
    ) -> (TracksWriter, TracksRequest, TracksReader) {
        let info = Arc::new(self);
        let state = State::new(TracksState::new(cache_idle_timeout)).split();
        let queue = Queue::default().split();

        let writer = TracksWriter::new(state.0.clone(), info.clone());
        let request = TracksRequest::new(state.0, queue.0, info.clone());
        let reader = TracksReader::new(state.1, queue.1, info);

        (writer, request, reader)
    }
}

/// A cached track reader plus the downstream interest in it.
struct TrackCacheEntry {
    reader: TrackReader,

    /// Downstream subscribers currently being served from this reader, for
    /// pull-through cache entries only.
    ///
    /// Locally created tracks ([`TracksWriter::create`]) have no upstream
    /// subscription to release, so there is nothing to count.
    ///
    /// Held here rather than beside the upstream subscription so that
    /// [`TracksReader::subscribe`] can register interest under the same lock it
    /// reads the cache under, which is what makes eviction race-free.
    interest: Option<TrackInterest>,
}

pub struct TracksState {
    tracks: HashMap<FullTrackName, TrackCacheEntry>,

    /// How long an unwatched cache entry is retained before its upstream
    /// subscription is released. Zero disables eviction.
    cache_idle_timeout: Duration,
}

impl TracksState {
    fn new(cache_idle_timeout: Duration) -> Self {
        Self {
            tracks: HashMap::new(),
            cache_idle_timeout,
        }
    }
}

impl Default for TracksState {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_IDLE_TIMEOUT)
    }
}

/// A request for a track that is not cached yet, sent to the session that
/// announced the namespace so it can SUBSCRIBE upstream.
pub struct TrackRequest {
    /// Writer the upstream subscription should fill.
    pub writer: TrackWriter,

    /// Lease on the pull-through cache entry backing this request.
    ///
    /// The requester should hold the upstream subscription open until
    /// [`CacheLease::released`] resolves, then drop it to send UNSUBSCRIBE.
    pub lease: CacheLease,
}

impl Deref for TrackRequest {
    type Target = TrackWriter;

    fn deref(&self) -> &Self::Target {
        &self.writer
    }
}

/// Ties an upstream subscription to downstream interest in its cache entry.
///
/// Handed to whichever session owns the upstream subscription so it can tell when
/// the cached track has gone unwatched and stop paying for it.
pub struct CacheLease {
    state: StateWeak<TracksState>,
    full_name: FullTrackName,
    interest: TrackInterest,
    cache_idle_timeout: Duration,
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
        if self.cache_idle_timeout.is_zero() {
            std::future::pending::<()>().await;
        }

        loop {
            self.interest.idle_for(self.cache_idle_timeout).await;

            if self.evict_if_idle() {
                return;
            }

            // A subscriber arrived between the timer firing and the eviction
            // taking the lock, so the entry is busy again. Go back to waiting.
            tracing::trace!(
                target: "moq_transport::tracks",
                namespace = %self.full_name.namespace,
                track = %self.full_name.name,
                "cache eviction abandoned; downstream interest returned"
            );
        }
    }

    /// Remove the entry if it is still ours and still unwatched.
    ///
    /// Returns false, leaving the entry in place, only when interest returned
    /// before the lock was acquired. Everything else — the broadcast being gone,
    /// or the entry having been replaced by a newer generation — means this
    /// upstream subscription is no longer serving anything, so it should be
    /// released rather than pinned forever.
    ///
    /// The idle check happens under the same lock guards are created under, so a
    /// subscriber racing eviction either gets counted here (and eviction is
    /// abandoned) or misses the cache entirely and requests a fresh entry.
    fn evict_if_idle(&self) -> bool {
        // All `TracksReader`s are gone, so nothing can be served from this cache.
        let Some(state) = self.state.upgrade() else {
            return true;
        };

        let Some(mut state) = state.lock_mut() else {
            return true;
        };

        match state.tracks.get(&self.full_name) {
            Some(entry) => {
                // Only evict the generation this lease belongs to. A replacement
                // entry has its own lease and its own idle timer, and a locally
                // created track has no upstream subscription at all.
                let ours = entry
                    .interest
                    .as_ref()
                    .is_some_and(|current| current.same_generation(&self.interest));

                if !ours {
                    return true;
                }

                if !self.interest.is_idle() {
                    return false;
                }

                state.tracks.remove(&self.full_name);
            }
            // Already gone (e.g. evicted as stale), so the upstream subscription
            // is no longer serving anything.
            None => return true,
        }

        tracing::debug!(
            target: "moq_transport::tracks",
            namespace = %self.full_name.namespace,
            track = %self.full_name.name,
            "evicting idle cached track and releasing upstream subscription"
        );

        true
    }
}

/// Publish new tracks for a broadcast by name.
pub struct TracksWriter {
    state: State<TracksState>,
    pub info: Arc<Tracks>,
}

impl TracksWriter {
    fn new(state: State<TracksState>, info: Arc<Tracks>) -> Self {
        Self { state, info }
    }

    /// Create a new track with the given name, inserting it into the broadcast.
    /// The track will use this writer's namespace.
    /// None is returned if all [TracksReader]s have been dropped.
    pub fn create(&mut self, track: &str) -> Option<TrackWriter> {
        let (writer, reader) = Track {
            namespace: self.namespace.clone(),
            name: track.to_owned(),
        }
        .produce();

        // NOTE: We overwrite the track if it already exists.
        let full_name = FullTrackName {
            namespace: self.namespace.clone(),
            name: track.to_owned(),
        };
        self.state.lock_mut()?.tracks.insert(
            full_name,
            TrackCacheEntry {
                reader,
                // Created locally, so there is no upstream subscription to
                // release and nothing to count interest against.
                interest: None,
            },
        );

        Some(writer)
    }

    /// Remove a track from the broadcast by full name.
    pub fn remove(&mut self, namespace: &TrackNamespace, track_name: &str) -> Option<TrackReader> {
        let full_name = FullTrackName {
            namespace: namespace.clone(),
            name: track_name.to_owned(),
        };
        self.state
            .lock_mut()?
            .tracks
            .remove(&full_name)
            .map(|entry| entry.reader)
    }
}

impl Deref for TracksWriter {
    type Target = Tracks;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

pub struct TracksRequest {
    #[allow(dead_code)] // Avoid dropping the write side
    state: State<TracksState>,
    incoming: Option<Queue<TrackRequest>>,
    pub info: Arc<Tracks>,
}

impl TracksRequest {
    fn new(state: State<TracksState>, incoming: Queue<TrackRequest>, info: Arc<Tracks>) -> Self {
        Self {
            state,
            incoming: Some(incoming),
            info,
        }
    }

    /// Wait for a request to create a new track.
    ///
    /// The returned [`TrackRequest`] carries both the writer to fill and a
    /// [`CacheLease`]; the handler should hold its upstream subscription until
    /// [`CacheLease::released`] resolves and then drop it, which sends
    /// UNSUBSCRIBE.
    ///
    /// None is returned if all [TracksReader]s have been dropped.
    pub async fn next(&mut self) -> Option<TrackRequest> {
        self.incoming.as_mut()?.pop().await
    }
}

impl Deref for TracksRequest {
    type Target = Tracks;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

impl Drop for TracksRequest {
    fn drop(&mut self) {
        // Close any tracks still in the Queue
        let pending_tracks = self.incoming.take().unwrap().close();
        if !pending_tracks.is_empty() {
            tracing::debug!(
                target: "moq_transport::tracks",
                namespace = %self.info.namespace,
                count = pending_tracks.len(),
                "TracksRequest dropped with pending track requests"
            );
        }
        for track in pending_tracks {
            let _ = track.writer.close(ServeError::not_found_ctx(
                "tracks request dropped before track handled",
            ));
        }
    }
}

/// Subscribe to a broadcast by requesting tracks.
///
/// This can be cloned to create handles.
#[derive(Clone)]
pub struct TracksReader {
    state: State<TracksState>,
    queue: Queue<TrackRequest>,
    pub info: Arc<Tracks>,
}

impl TracksReader {
    fn new(state: State<TracksState>, queue: Queue<TrackRequest>, info: Arc<Tracks>) -> Self {
        Self { state, queue, info }
    }

    /// Get a track from the broadcast by full name, if it exists and is still alive.
    /// Returns None if the track doesn't exist or has been closed.
    ///
    /// This deliberately registers no interest: it is a point-in-time lookup (for
    /// TRACK_STATUS) rather than the start of a subscription, so it must not keep
    /// an otherwise-idle upstream subscription alive. Use [`Self::subscribe`] when
    /// the reader will actually be served.
    pub fn get_track_reader(
        &mut self,
        namespace: &TrackNamespace,
        track_name: &str,
    ) -> Option<TrackReader> {
        let state = self.state.lock();
        let full_name = FullTrackName {
            namespace: namespace.clone(),
            name: track_name.to_owned(),
        };

        if let Some(entry) = state.tracks.get(&full_name) {
            if !entry.reader.is_closed() {
                return Some(entry.reader.clone());
            }
            // Track exists but is closed/stale - don't return it
        }
        None
    }

    /// Get or request a track from the broadcast by full name.
    /// The namespace parameter should be the full requested namespace, not just the announced prefix.
    /// None is returned if [TracksWriter] or [TracksRequest] cannot fufill the request.
    ///
    /// Returns the reader plus, for pull-through cache entries, a guard the caller
    /// must hold for as long as it is serving that reader. Dropping the guard is
    /// what eventually lets the upstream subscription be released; a locally
    /// created track has no upstream subscription and so yields no guard.
    ///
    /// The guard is taken while the cache lock is held. Taking it afterwards would
    /// leave a window in which eviction sees an idle entry and removes the reader
    /// this caller is about to serve.
    pub fn subscribe(
        &mut self,
        namespace: TrackNamespace,
        track_name: &str,
    ) -> Option<(TrackReader, Option<TrackInterestGuard>)> {
        let state = self.state.lock();
        let full_name = FullTrackName {
            namespace: namespace.clone(),
            name: track_name.to_owned(),
        };

        // Check if we have a cached track that is still alive
        if let Some(entry) = state.tracks.get(&full_name) {
            if !entry.reader.is_closed() {
                // Track is still active, return the cached reader
                tracing::debug!(
                    target: "moq_transport::tracks",
                    namespace = %namespace,
                    track = %track_name,
                    "track cache hit (active)"
                );
                let guard = entry.interest.as_ref().map(TrackInterest::guard);
                return Some((entry.reader.clone(), guard));
            }
            // Track is closed/stale, fall through to create a new one
            tracing::debug!(
                target: "moq_transport::tracks",
                namespace = %namespace,
                track = %track_name,
                "track cache hit but stale, will evict and re-request"
            );
        }

        let mut state = state.into_mut()?;

        // Remove the stale track if it exists (it was closed)
        state.tracks.remove(&full_name);
        // Use the full requested namespace, not self.namespace
        let (writer, reader) = Track {
            namespace: namespace.clone(),
            name: track_name.to_owned(),
        }
        .produce();

        let interest = TrackInterest::new();

        // Take this caller's guard before the entry is visible to anyone else, so
        // the entry can never be seen as idle while we are still setting up the
        // upstream subscription.
        let guard = interest.guard();

        let lease = CacheLease {
            state: self.state.downgrade(),
            full_name: full_name.clone(),
            interest: interest.clone(),
            cache_idle_timeout: state.cache_idle_timeout,
        };

        if self.queue.push(TrackRequest { writer, lease }).is_err() {
            tracing::debug!(
                target: "moq_transport::tracks",
                namespace = %namespace,
                track = %track_name,
                "track request queue closed"
            );
            return None;
        }

        // We requested the track successfully so we can deduplicate it by full name.
        state.tracks.insert(
            full_name,
            TrackCacheEntry {
                reader: reader.clone(),
                interest: Some(interest),
            },
        );

        tracing::debug!(
            target: "moq_transport::tracks",
            namespace = %namespace,
            track = %track_name,
            "track cache miss, requested from upstream"
        );

        Some((reader, Some(guard)))
    }
}

impl Deref for TracksReader {
    type Target = Tracks;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the stale track caching bug.
    ///
    /// Scenario:
    /// 1. Subscriber requests a track via subscribe()
    /// 2. Publisher receives TrackWriter, closes it with an error (simulating failure)
    /// 3. Subscriber requests the same track again
    /// 4. Publisher should receive a new TrackWriter (previously didn't due to stale cache)
    ///
    /// This test verifies the fix for an issue seen in production where a track became
    /// "stale" after a connection timeout, and subsequent subscribers never received
    /// data because the publisher was never notified of new subscriptions.
    #[tokio::test]
    async fn test_stale_track_cache_bug() {
        let namespace = TrackNamespace::from_utf8_path("test/namespace");
        let track_name = "test-track";

        // Create the Tracks producer (simulates what the relay does)
        let (_writer, mut request, mut reader) = Tracks::new(namespace.clone()).produce();

        // First subscription: subscriber requests the track
        let (track_reader_1, _interest_1) = reader
            .subscribe(namespace.clone(), track_name)
            .expect("first subscribe should succeed");

        // Publisher receives the request and gets a TrackWriter
        let track_request_1 = request
            .next()
            .await
            .expect("publisher should receive first track request");

        assert_eq!(track_request_1.name, track_name);

        // Publisher closes the track with an error (simulates connection failure)
        track_request_1
            .writer
            .close(ServeError::Cancel)
            .expect("close should succeed");

        // Verify the first track reader is now closed
        // (This is what makes subsequent reads fail immediately)
        let closed_result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            track_reader_1.closed(),
        )
        .await;
        assert!(
            closed_result.is_ok(),
            "track_reader_1 should be closed after writer closes"
        );

        // Second subscription: subscriber requests the SAME track again
        let (track_reader_2, _interest_2) = reader
            .subscribe(namespace.clone(), track_name)
            .expect("second subscribe should succeed");

        // With the fix, the stale cached TrackReader is detected and evicted,
        // so the publisher receives a new TrackWriter for the second subscription.
        let maybe_track_writer_2 =
            tokio::time::timeout(std::time::Duration::from_millis(100), request.next()).await;

        // Publisher should receive a new TrackWriter (stale cache entry was evicted)
        assert!(
            maybe_track_writer_2.is_ok(),
            "Publisher should receive a new track request after the first one was closed"
        );

        let track_request_2 = maybe_track_writer_2
            .unwrap()
            .expect("publisher should receive second track request");

        assert_eq!(track_request_2.name, track_name);

        // Verify that track_reader_2 is NOT already closed
        // (It should be a fresh, working track)
        let closed_result_2 = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            track_reader_2.closed(),
        )
        .await;
        assert!(
            closed_result_2.is_err(),
            "track_reader_2 should NOT be immediately closed - it should be a fresh track"
        );
    }

    /// Test that normal track caching works correctly when tracks are still alive.
    ///
    /// Multiple subscribers to the same track should share the same TrackReader
    /// (deduplication), and the publisher should only receive one request.
    #[tokio::test]
    async fn test_track_deduplication_while_alive() {
        let namespace = TrackNamespace::from_utf8_path("test/namespace");
        let track_name = "test-track";

        let (_writer, mut request, mut reader) = Tracks::new(namespace.clone()).produce();

        // First subscription
        let (track_reader_1, _interest_1) = reader
            .subscribe(namespace.clone(), track_name)
            .expect("first subscribe should succeed");

        // Publisher receives request
        let _track_request = request
            .next()
            .await
            .expect("publisher should receive track request");

        // Second subscription to the SAME track (while it's still alive)
        let (track_reader_2, _interest_2) = reader
            .subscribe(namespace.clone(), track_name)
            .expect("second subscribe should succeed");

        // Publisher should NOT receive another request (track is cached and alive)
        let maybe_second_request =
            tokio::time::timeout(std::time::Duration::from_millis(100), request.next()).await;

        assert!(
            maybe_second_request.is_err(),
            "Publisher should NOT receive a second request - track is cached and alive"
        );

        // Both readers should refer to the same track
        assert_eq!(track_reader_1.name, track_reader_2.name);
        assert_eq!(track_reader_1.namespace, track_reader_2.namespace);
    }

    /// Test that a track is NOT considered stale after the writer transitions to
    /// subgroups mode. This is the core regression: TrackWriter::subgroups()
    /// consumes self, dropping the Track-level State, but the SubgroupsWriter
    /// is still alive — so is_closed() must return false.
    #[tokio::test]
    async fn test_track_not_stale_after_subgroups_transition() {
        let namespace = TrackNamespace::from_utf8_path("test/namespace");
        let track_name = "test-track";

        let (_writer, mut request, mut reader) = Tracks::new(namespace.clone()).produce();

        let _sub_1 = reader
            .subscribe(namespace.clone(), track_name)
            .expect("first subscribe should succeed");

        let track_request = request
            .next()
            .await
            .expect("publisher should receive track request");

        let _subgroups_writer = track_request
            .writer
            .subgroups()
            .expect("subgroups transition should succeed");

        let _sub_2 = reader
            .subscribe(namespace.clone(), track_name)
            .expect("second subscribe should succeed");

        let maybe_second_request =
            tokio::time::timeout(std::time::Duration::from_millis(100), request.next()).await;

        assert!(
            maybe_second_request.is_err(),
            "publisher should NOT get a second request while SubgroupsWriter is alive"
        );
    }

    /// Test that a track IS considered stale after the SubgroupsWriter is dropped.
    /// This preserves the RT-458 eviction behavior for dead publishers.
    #[tokio::test]
    async fn test_track_stale_after_subgroups_writer_dropped() {
        let namespace = TrackNamespace::from_utf8_path("test/namespace");
        let track_name = "test-track";

        let (_writer, mut request, mut reader) = Tracks::new(namespace.clone()).produce();

        let _sub_1 = reader
            .subscribe(namespace.clone(), track_name)
            .expect("first subscribe should succeed");

        let track_request = request
            .next()
            .await
            .expect("publisher should receive track request");

        let subgroups_writer = track_request
            .writer
            .subgroups()
            .expect("subgroups transition should succeed");
        drop(subgroups_writer);

        let _sub_2 = reader
            .subscribe(namespace.clone(), track_name)
            .expect("second subscribe should succeed");

        let maybe_second_request =
            tokio::time::timeout(std::time::Duration::from_millis(100), request.next()).await;

        assert!(
            maybe_second_request.is_ok(),
            "publisher should get a new request after SubgroupsWriter is dropped"
        );

        let _second_request = maybe_second_request
            .unwrap()
            .expect("publisher should receive second track request");
    }

    const GRACE: Duration = Duration::from_secs(30);

    /// The bug this fixes: once the last downstream subscriber leaves, the
    /// upstream subscription must be released rather than held for the session's
    /// lifetime.
    #[tokio::test(start_paused = true)]
    async fn idle_cache_entry_is_evicted_and_the_lease_released() {
        let namespace = TrackNamespace::from_utf8_path("test/namespace");
        let track_name = "test-track";

        let (_writer, mut request, mut reader) =
            Tracks::new(namespace.clone()).produce_with_cache_idle_timeout(GRACE);

        let (_track_reader, interest) = reader
            .subscribe(namespace.clone(), track_name)
            .expect("subscribe should succeed");
        let request_1 = request.next().await.expect("upstream request");

        let released = request_1.lease.released();
        tokio::pin!(released);

        // While a subscriber is being served the lease must never release.
        tokio::select! {
            _ = &mut released => panic!("a watched track must not be released"),
            _ = tokio::time::sleep(GRACE * 10) => {}
        }

        // Last subscriber leaves.
        drop(interest);
        released.await;

        // The entry was evicted, so the next subscriber gets a fresh upstream
        // request rather than a reader nobody is feeding.
        let _ = reader
            .subscribe(namespace.clone(), track_name)
            .expect("resubscribe should succeed");
        let request_2 = tokio::time::timeout(Duration::from_millis(100), request.next())
            .await
            .expect("a fresh upstream request should be issued")
            .expect("upstream request");
        assert_eq!(request_2.name, track_name);
    }

    /// The grace period is what keeps a reconnecting subscriber, or a player
    /// switching renditions, off a fresh upstream SUBSCRIBE round trip.
    #[tokio::test(start_paused = true)]
    async fn warm_cache_is_reused_within_the_grace_period() {
        let namespace = TrackNamespace::from_utf8_path("test/namespace");
        let track_name = "test-track";

        let (_writer, mut request, mut reader) =
            Tracks::new(namespace.clone()).produce_with_cache_idle_timeout(GRACE);

        let (reader_1, interest) = reader
            .subscribe(namespace.clone(), track_name)
            .expect("subscribe should succeed");
        let request_1 = request.next().await.expect("upstream request");

        let released = request_1.lease.released();
        tokio::pin!(released);

        drop(interest);

        // Part-way through the grace period a new subscriber arrives.
        tokio::select! {
            _ = &mut released => panic!("released before the grace period elapsed"),
            _ = tokio::time::sleep(GRACE / 2) => {}
        }

        let (reader_2, interest_2) = reader
            .subscribe(namespace.clone(), track_name)
            .expect("resubscribe should succeed");

        // Same warm entry, and no second upstream subscription.
        assert!(Arc::ptr_eq(&reader_1.info, &reader_2.info));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), request.next())
                .await
                .is_err(),
            "a warm cache hit must not issue a new upstream request"
        );

        tokio::select! {
            _ = &mut released => panic!("released while a subscriber was present"),
            _ = tokio::time::sleep(GRACE * 10) => {}
        }

        drop(interest_2);
        released.await;
    }

    /// A zero timeout is the opt-out: upstream subscriptions live as long as the
    /// session, which is the behaviour before this change.
    #[tokio::test(start_paused = true)]
    async fn zero_timeout_never_releases_the_lease() {
        let namespace = TrackNamespace::from_utf8_path("test/namespace");
        let track_name = "test-track";

        let (_writer, mut request, mut reader) =
            Tracks::new(namespace.clone()).produce_with_cache_idle_timeout(Duration::ZERO);

        let (_track_reader, interest) = reader
            .subscribe(namespace.clone(), track_name)
            .expect("subscribe should succeed");
        let request_1 = request.next().await.expect("upstream request");

        drop(interest);

        tokio::select! {
            _ = request_1.lease.released() => panic!("a zero timeout must disable eviction"),
            _ = tokio::time::sleep(GRACE * 100) => {}
        }
    }

    /// If every `TracksReader` is gone the broadcast can no longer serve anyone,
    /// so the upstream subscription should be released rather than pinned by a
    /// lease that can never be evicted.
    #[tokio::test(start_paused = true)]
    async fn lease_is_released_once_every_reader_is_dropped() {
        let namespace = TrackNamespace::from_utf8_path("test/namespace");
        let track_name = "test-track";

        let (_writer, mut request, mut reader) =
            Tracks::new(namespace.clone()).produce_with_cache_idle_timeout(GRACE);

        let (_track_reader, interest) = reader
            .subscribe(namespace.clone(), track_name)
            .expect("subscribe should succeed");
        let request_1 = request.next().await.expect("upstream request");

        drop(interest);
        drop(reader);

        request_1.lease.released().await;
    }

    /// A guard outstanding from an evicted entry must not keep a same-named
    /// replacement looking busy, and must not evict it either.
    #[tokio::test(start_paused = true)]
    async fn a_stale_lease_does_not_evict_a_replacement_entry() {
        let namespace = TrackNamespace::from_utf8_path("test/namespace");
        let track_name = "test-track";

        let (_writer, mut request, mut reader) =
            Tracks::new(namespace.clone()).produce_with_cache_idle_timeout(GRACE);

        let (_reader_1, interest_1) = reader
            .subscribe(namespace.clone(), track_name)
            .expect("subscribe should succeed");
        let request_1 = request.next().await.expect("first upstream request");

        drop(interest_1);
        request_1.lease.released().await;

        // A replacement generation for the same track name.
        let (_reader_2, interest_2) = reader
            .subscribe(namespace.clone(), track_name)
            .expect("resubscribe should succeed");
        let request_2 = request.next().await.expect("second upstream request");

        // Re-driving the old lease must be a no-op: it reports released (its own
        // generation is gone) without touching the live replacement.
        request_1.lease.released().await;

        let released_2 = request_2.lease.released();
        tokio::pin!(released_2);
        tokio::select! {
            _ = &mut released_2 => panic!("the replacement entry is still watched"),
            _ = tokio::time::sleep(GRACE * 10) => {}
        }

        drop(interest_2);
        released_2.await;
    }
}
