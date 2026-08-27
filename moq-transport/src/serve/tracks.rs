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
use std::{collections::HashMap, ops::Deref, sync::Arc};

use super::{ServeError, Track, TrackReader, TrackWriter};
use crate::coding::{TrackName, TrackNamespace};
use crate::watch::{Queue, State};

/// Full track identifier: namespace + track name
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct FullTrackName {
    pub namespace: TrackNamespace,
    pub name: TrackName,
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
        let info = Arc::new(self);
        let state = State::default().split();
        let queue = Queue::default().split();

        let writer = TracksWriter::new(state.0.clone(), info.clone());
        let request = TracksRequest::new(state.0, queue.0, info.clone());
        let reader = TracksReader::new(state.1, queue.1, info);

        (writer, request, reader)
    }
}

#[derive(Default)]
pub struct TracksState {
    tracks: HashMap<FullTrackName, TrackReader>,
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
    pub fn create(&mut self, track: impl Into<TrackName>) -> Option<TrackWriter> {
        let track = track.into();
        let (writer, reader) = Track {
            namespace: self.namespace.clone(),
            name: track.clone(),
        }
        .produce();

        // NOTE: We overwrite the track if it already exists.
        let full_name = FullTrackName {
            namespace: self.namespace.clone(),
            name: track,
        };
        self.state.lock_mut()?.tracks.insert(full_name, reader);

        Some(writer)
    }

    /// Remove a track from the broadcast by full name.
    pub fn remove(
        &mut self,
        namespace: &TrackNamespace,
        track_name: impl Into<TrackName>,
    ) -> Option<TrackReader> {
        let full_name = FullTrackName {
            namespace: namespace.clone(),
            name: track_name.into(),
        };
        self.state.lock_mut()?.tracks.remove(&full_name)
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
    incoming: Option<Queue<TrackWriter>>,
    pub info: Arc<Tracks>,
}

impl TracksRequest {
    fn new(state: State<TracksState>, incoming: Queue<TrackWriter>, info: Arc<Tracks>) -> Self {
        Self {
            state,
            incoming: Some(incoming),
            info,
        }
    }

    /// Wait for a request to create a new track.
    /// None is returned if all [TracksReader]s have been dropped.
    pub async fn next(&mut self) -> Option<TrackWriter> {
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
            let _ = track.close(ServeError::not_found_ctx(
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
    queue: Queue<TrackWriter>,
    pub info: Arc<Tracks>,
}

impl TracksReader {
    fn new(state: State<TracksState>, queue: Queue<TrackWriter>, info: Arc<Tracks>) -> Self {
        Self { state, queue, info }
    }

    /// Get a track from the broadcast by full name, if it exists and is still alive.
    /// Returns None if the track doesn't exist or has been closed.
    pub fn get_track_reader(
        &mut self,
        namespace: &TrackNamespace,
        track_name: impl Into<TrackName>,
    ) -> Option<TrackReader> {
        let track_name = track_name.into();
        let state = self.state.lock();
        let full_name = FullTrackName {
            namespace: namespace.clone(),
            name: track_name.clone(),
        };

        if let Some(track_reader) = state.tracks.get(&full_name) {
            if !track_reader.is_closed() {
                return Some(track_reader.clone());
            }
            // Track exists but is closed/stale - don't return it
        }
        None
    }

    /// Get or request a track from the broadcast by full name.
    /// The namespace parameter should be the full requested namespace, not just the announced prefix.
    /// None is returned if [TracksWriter] or [TracksRequest] cannot fufill the request.
    pub fn subscribe(
        &mut self,
        namespace: TrackNamespace,
        track_name: impl Into<TrackName>,
    ) -> Option<TrackReader> {
        let track_name = track_name.into();
        let state = self.state.lock();
        let full_name = FullTrackName {
            namespace: namespace.clone(),
            name: track_name.clone(),
        };

        // Check if we have a cached track that is still alive
        if let Some(track_reader) = state.tracks.get(&full_name) {
            if !track_reader.is_closed() {
                // Track is still active, return the cached reader
                tracing::debug!(
                    target: "moq_transport::tracks",
                    namespace = %namespace,
                    track = %track_name,
                    "track cache hit (active)"
                );
                return Some(track_reader.clone());
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
        let track_writer_reader = Track {
            namespace: namespace.clone(),
            name: track_name.clone(),
        }
        .produce();

        if self.queue.push(track_writer_reader.0).is_err() {
            tracing::debug!(
                target: "moq_transport::tracks",
                namespace = %namespace,
                track = %track_name,
                "track request queue closed"
            );
            return None;
        }

        // We requested the track successfully so we can deduplicate it by full name.
        state
            .tracks
            .insert(full_name, track_writer_reader.1.clone());

        tracing::debug!(
            target: "moq_transport::tracks",
            namespace = %namespace,
            track = %track_name,
            "track cache miss, requested from upstream"
        );

        Some(track_writer_reader.1)
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
    use crate::serve::TrackReaderMode;
    use bytes::Bytes;

    /// Helper: deterministic payload for (group_id, object_id).
    fn payload(group_id: u64, object_id: u64) -> Bytes {
        Bytes::from(format!("g{}-o{}", group_id, object_id))
    }

    /// Helper: drain all objects from a TrackReader in Subgroups mode.
    async fn drain_subgroups(track_reader: TrackReader) -> Vec<(u64, u64, Bytes)> {
        tokio::time::timeout(std::time::Duration::from_secs(5), async move {
            let mode = track_reader
                .mode()
                .await
                .expect("track_reader.mode() should succeed");
            let mut received: Vec<(u64, u64, Bytes)> = Vec::new();
            if let TrackReaderMode::Subgroups(mut subgroups) = mode {
                while let Ok(Some(mut subgroup)) = subgroups.next().await {
                    let gid = subgroup.group_id;
                    while let Ok(Some(mut obj)) = subgroup.next().await {
                        let oid = obj.object_id;
                        let data: Bytes =
                            obj.read_all().await.expect("obj.read_all() should succeed");
                        received.push((gid, oid, data));
                    }
                }
            } else {
                panic!("expected Subgroups mode");
            }
            received
        })
        .await
        .expect("drain_subgroups timed out after 5 seconds")
    }

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
        let track_reader_1 = reader
            .subscribe(namespace.clone(), track_name)
            .expect("first subscribe should succeed");

        // Publisher receives the request and gets a TrackWriter
        let track_writer_1 = request
            .next()
            .await
            .expect("publisher should receive first track request");

        assert_eq!(track_writer_1.name, TrackName::from(track_name));

        // Publisher closes the track with an error (simulates connection failure)
        track_writer_1
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
        let track_reader_2 = reader
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

        let track_writer_2 = maybe_track_writer_2
            .unwrap()
            .expect("publisher should receive second track request");

        assert_eq!(track_writer_2.name, TrackName::from(track_name));

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
        let track_reader_1 = reader
            .subscribe(namespace.clone(), track_name)
            .expect("first subscribe should succeed");

        // Publisher receives request
        let _track_writer = request
            .next()
            .await
            .expect("publisher should receive track request");

        // Second subscription to the SAME track (while it's still alive)
        let track_reader_2 = reader
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

    // ---------------------------------------------------------------
    // I1: Write-read round-trip through Track
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn track_round_trip() {
        let namespace = TrackNamespace::from_utf8_path("test/ns");
        let (track_writer, track_reader) =
            Track::new(namespace, "data-track".to_string()).produce();

        let num_objects = 10u64;

        // Writer: create subgroups, write 1 group x 10 objects
        let write_handle = tokio::spawn(async move {
            let mut subgroups = track_writer.subgroups().unwrap();
            let mut sg = subgroups.append(0).unwrap();
            for oid in 0..num_objects {
                sg.write(payload(0, oid)).unwrap();
            }
            drop(sg);
            subgroups.close(ServeError::Done).ok();
        });

        // Reader: drain all data
        let received = drain_subgroups(track_reader).await;
        write_handle.await.unwrap();

        assert_eq!(received.len(), num_objects as usize);
        for (gid, oid, data) in &received {
            assert_eq!(*gid, 0);
            assert_eq!(data, &payload(0, *oid));
        }
    }

    // ---------------------------------------------------------------
    // I2: Tracks subscribe round-trip
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn tracks_subscribe_round_trip() {
        let namespace = TrackNamespace::from_utf8_path("test/ns");
        let track_name = "data-track";
        let num_objects = 5u64;

        let (writer, mut request, mut reader) = Tracks::new(namespace.clone()).produce();

        // Subscriber side: request the track
        let track_reader = reader
            .subscribe(namespace.clone(), track_name)
            .expect("subscribe should succeed");

        // Publisher side: receive request, write data
        let pub_handle = tokio::spawn(async move {
            let track_writer = request.next().await.expect("should receive track request");

            assert_eq!(track_writer.name, TrackName::from(track_name));

            let mut subgroups = track_writer.subgroups().unwrap();
            let mut sg = subgroups.append(0).unwrap();
            for oid in 0..num_objects {
                sg.write(payload(0, oid)).unwrap();
            }
            drop(sg);
            subgroups.close(ServeError::Done).ok();

            // Keep writer alive until data is consumed
            drop(writer);
        });

        // Subscriber side: read data
        let received = drain_subgroups(track_reader).await;
        pub_handle.await.unwrap();

        assert_eq!(received.len(), num_objects as usize);
        for (gid, oid, data) in &received {
            assert_eq!(*gid, 0);
            assert_eq!(data, &payload(0, *oid));
        }
    }

    // ---------------------------------------------------------------
    // I3: Multiple tracks independence
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn multiple_tracks_independence() {
        let namespace = TrackNamespace::from_utf8_path("test/ns");

        // Create two independent tracks directly (no Tracks layer needed).
        let (track_a_writer, track_a_reader) =
            Track::new(namespace.clone(), "track-a".to_string()).produce();
        let (track_b_writer, track_b_reader) =
            Track::new(namespace.clone(), "track-b".to_string()).produce();

        // Write different data to each
        let handle_a = tokio::spawn(async move {
            let mut subgroups = track_a_writer.subgroups().unwrap();
            let mut sg = subgroups.append(0).unwrap();
            sg.write(Bytes::from_static(b"alpha-0")).unwrap();
            sg.write(Bytes::from_static(b"alpha-1")).unwrap();
            drop(sg);
            subgroups.close(ServeError::Done).ok();
        });

        let handle_b = tokio::spawn(async move {
            let mut subgroups = track_b_writer.subgroups().unwrap();
            let mut sg = subgroups.append(0).unwrap();
            sg.write(Bytes::from_static(b"beta-0")).unwrap();
            sg.write(Bytes::from_static(b"beta-1")).unwrap();
            sg.write(Bytes::from_static(b"beta-2")).unwrap();
            drop(sg);
            subgroups.close(ServeError::Done).ok();
        });

        let data_a = drain_subgroups(track_a_reader).await;
        let data_b = drain_subgroups(track_b_reader).await;

        handle_a.await.unwrap();
        handle_b.await.unwrap();

        // Verify no cross-contamination
        assert_eq!(data_a.len(), 2);
        assert_eq!(data_a[0].2, Bytes::from_static(b"alpha-0"));
        assert_eq!(data_a[1].2, Bytes::from_static(b"alpha-1"));

        assert_eq!(data_b.len(), 3);
        assert_eq!(data_b[0].2, Bytes::from_static(b"beta-0"));
        assert_eq!(data_b[1].2, Bytes::from_static(b"beta-1"));
        assert_eq!(data_b[2].2, Bytes::from_static(b"beta-2"));
    }

    // ---------------------------------------------------------------
    // I4: Stale track re-subscribe with data integrity
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn stale_resubscribe_data_integrity() {
        let namespace = TrackNamespace::from_utf8_path("test/ns");
        let track_name = "data-track";

        let (_writer, mut request, mut reader) = Tracks::new(namespace.clone()).produce();

        // First subscription
        let track_reader_1 = reader
            .subscribe(namespace.clone(), track_name)
            .expect("first subscribe");

        // Publisher receives and closes (simulates failure)
        let track_writer_1 = request.next().await.expect("first request");
        track_writer_1.close(ServeError::Cancel).unwrap();

        // Wait for close to propagate using closed() with timeout
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            track_reader_1.closed(),
        )
        .await;

        // Second subscription (stale eviction should happen)
        let track_reader_2 = reader
            .subscribe(namespace.clone(), track_name)
            .expect("second subscribe");

        // Publisher receives the new request and writes real data
        let track_writer_2 =
            tokio::time::timeout(std::time::Duration::from_millis(100), request.next())
                .await
                .expect("should receive second request")
                .expect("second request should be Some");

        let write_handle = tokio::spawn(async move {
            let mut subgroups = track_writer_2.subgroups().unwrap();
            let mut sg = subgroups.append(0).unwrap();
            for oid in 0..3u64 {
                sg.write(payload(0, oid)).unwrap();
            }
            drop(sg);
            subgroups.close(ServeError::Done).ok();
        });

        // Read data from the second subscription
        let received = drain_subgroups(track_reader_2).await;
        write_handle.await.unwrap();

        // All 3 objects from the second subscription must be intact
        assert_eq!(received.len(), 3);
        for (gid, oid, data) in &received {
            assert_eq!(*gid, 0);
            assert_eq!(data, &payload(0, *oid));
        }
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

        let _track_reader_1 = reader
            .subscribe(namespace.clone(), track_name)
            .expect("first subscribe should succeed");

        let track_writer = request
            .next()
            .await
            .expect("publisher should receive track request");

        let _subgroups_writer = track_writer
            .subgroups()
            .expect("subgroups transition should succeed");

        let _track_reader_2 = reader
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

        let _track_reader_1 = reader
            .subscribe(namespace.clone(), track_name)
            .expect("first subscribe should succeed");

        let track_writer = request
            .next()
            .await
            .expect("publisher should receive track request");

        let subgroups_writer = track_writer
            .subgroups()
            .expect("subgroups transition should succeed");
        drop(subgroups_writer);

        let _track_reader_2 = reader
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
}
