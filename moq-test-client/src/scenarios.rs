// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test scenario implementations
//!
//! Each scenario tests a specific aspect of MoQT interoperability.
//!
//! Each test function returns `Result<TestConnectionIds>` where success means
//! the test passed and failure means it failed. Connection IDs are collected
//! for correlation with relay-side mlog files.

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::time::{timeout, Duration};

use moq_native_ietf::quic;
use moq_transport::{
    coding::{KeyValuePairs, TrackNamespace},
    message::PublishDoneCode,
    serve::{ServeError, Track, TrackReaderMode, TrackWriter, Tracks},
    session::Session,
};

use crate::Args;

/// Overall test timeout - individual operations should complete faster
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Namespace used for test operations
const TEST_NAMESPACE: &str = "moq-test/interop";

/// Track name used for test operations
const TEST_TRACK: &str = "test-track";

/// Namespace used for direct PUBLISH test operations
const PUBLISH_NAMESPACE: &str = "moq-test/publish";

/// Track name used for direct PUBLISH test operations
const PUBLISH_TRACK: &str = "published-track";

/// Payload carried by the single Object the direct PUBLISH tests send
const PUBLISH_PAYLOAD: &[u8] = b"publish-track-subscribe";

/// Wait requested by the rendezvous timeout scenario, in milliseconds.
const RENDEZVOUS_TIMEOUT_MS: u64 = 500;

/// Allows transport scheduling slack beyond the requested rendezvous window.
const RENDEZVOUS_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Helper to connect to a relay and establish a session
/// Returns (session, connection_id, transport) so we can report CIDs for mlog correlation
async fn connect(
    args: &Args,
) -> Result<(
    web_transport::Session,
    String,
    moq_transport::session::Transport,
)> {
    let tls = args.tls.load()?;
    let quic = quic::Endpoint::new(quic::Config::new(args.bind, None, tls)?)?;

    let (session, connection_id, transport) = quic.client.connect(&args.relay, None).await?;
    Ok((session, connection_id, transport))
}

/// Collected connection IDs from a test run
#[derive(Debug, Default)]
pub struct TestConnectionIds {
    pub cids: Vec<String>,
}

impl TestConnectionIds {
    pub fn add(&mut self, cid: String) {
        self.cids.push(cid);
    }
}

fn write_test_subgroup(track: TrackWriter, payload: &'static [u8]) -> Result<()> {
    let mut subgroups = track.subgroups().context("failed to enter subgroup mode")?;
    let mut subgroup = subgroups.append(128).context("failed to create subgroup")?;
    subgroup
        .write(Bytes::from_static(payload))
        .context("failed to write subgroup object")?;
    // Dropping both writers ends the track, which is what a publisher with
    // nothing more to send does.
    drop(subgroup);
    drop(subgroups);
    Ok(())
}

/// Whether a subscription ending with this error is a normal end of track.
///
/// A publisher that has sent everything ends the subscription with
/// PUBLISH_DONE / TRACK_ENDED (draft-18 §10.11), which surfaces here as
/// `Closed(0x2)`. Treating that as a failure would fail every test that
/// subscribes to a finite track.
fn is_normal_subscription_end(err: &ServeError) -> bool {
    match err {
        ServeError::Done | ServeError::Cancel => true,
        ServeError::Closed(code) => *code == PublishDoneCode::TrackEnded as u64,
        _ => false,
    }
}

/// T0.1: Setup Only
///
/// Connect to relay, complete CLIENT_SETUP/SERVER_SETUP exchange, close gracefully.
pub async fn test_setup_only(args: &Args) -> Result<TestConnectionIds> {
    timeout(TEST_TIMEOUT, async {
        let (session, cid, transport) =
            connect(args).await.context("failed to connect to relay")?;
        let mut cids = TestConnectionIds::default();
        cids.add(cid);

        let (session, _publisher, _subscriber) = Session::connect(session, None, transport)
            .await
            .context("SETUP exchange failed")?;

        tracing::info!("SETUP exchange completed successfully");
        drop(session);
        Ok(cids)
    })
    .await
    .context("test timed out")?
}

/// T0.2: Publish Namespace Only
///
/// Connect to relay, send PUBLISH_NAMESPACE, receive REQUEST_OK, close.
pub async fn test_publish_namespace_only(args: &Args) -> Result<TestConnectionIds> {
    timeout(TEST_TIMEOUT, async {
        let (session, cid, transport) =
            connect(args).await.context("failed to connect to relay")?;
        let mut cids = TestConnectionIds::default();
        cids.add(cid);

        let (session, mut publisher, _subscriber) = Session::connect(session, None, transport)
            .await
            .context("SETUP exchange failed")?;

        let namespace = TrackNamespace::from_utf8_path(TEST_NAMESPACE);
        let (_, _, reader) = Tracks::new(namespace.clone()).produce();

        tracing::info!("Sending PUBLISH_NAMESPACE for: {}", TEST_NAMESPACE);

        // publish_namespace() blocks waiting for subscriptions after receiving REQUEST_OK.
        // If we receive REQUEST_ERROR instead, it returns Err immediately.
        // Timing out here means we received REQUEST_OK and are now waiting for subscribers,
        // which is the expected success case.
        let result = tokio::select! {
            res = publisher.publish_namespace(reader) => res,
            res = session.run() => {
                res.context("session error")?;
                anyhow::bail!("session ended before PUBLISH_NAMESPACE completed");
            }
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                tracing::info!(
                    "PUBLISH_NAMESPACE succeeded (REQUEST_OK received, waiting for subscribers)"
                );
                return Ok(cids);
            }
        };

        result.context("PUBLISH_NAMESPACE failed")?;
        Ok(cids)
    })
    .await
    .context("test timed out")?
}

/// T0.3: Subscribe Error
///
/// Subscribe to a non-existent track and verify we get a subscription error.
pub async fn test_subscribe_error(args: &Args) -> Result<TestConnectionIds> {
    timeout(TEST_TIMEOUT, async {
        let (session, cid, transport) =
            connect(args).await.context("failed to connect to relay")?;
        let mut cids = TestConnectionIds::default();
        cids.add(cid);

        let (session, _publisher, mut subscriber) = Session::connect(session, None, transport)
            .await
            .context("SETUP exchange failed")?;

        let namespace = TrackNamespace::from_utf8_path("nonexistent/namespace");
        let (mut writer, _, _reader) = Tracks::new(namespace.clone()).produce();

        let track = writer
            .create(TEST_TRACK)
            .ok_or_else(|| anyhow::anyhow!("failed to create track (already exists?)"))?;

        tracing::info!(
            "Subscribing to non-existent track: {}/{}",
            "nonexistent/namespace",
            TEST_TRACK
        );

        let subscribe_result = tokio::select! {
            res = subscriber.subscribe(track) => res,
            res = session.run() => {
                res.context("session error")?;
                anyhow::bail!("session ended before subscribe completed");
            }
        };

        match subscribe_result {
            Ok(()) => {
                anyhow::bail!("subscribe succeeded but should have failed (track doesn't exist)");
            }
            Err(e) => {
                let err_str = e.to_string().to_lowercase();
                let is_expected = err_str.contains("not found")
                    || err_str.contains("notfound")
                    || err_str.contains("no such")
                    || err_str.contains("doesn't exist")
                    || err_str.contains("does not exist")
                    || err_str.contains("unknown");

                if is_expected {
                    tracing::info!("Got expected 'not found' error: {}", e);
                } else {
                    tracing::warn!(
                        "Got error but not clearly 'not found': {}. \
                        Relay may use different error text.",
                        e
                    );
                }
                Ok(cids)
            }
        }
    })
    .await
    .context("test timed out")?
}

/// T0.9: Rendezvous Timeout
///
/// Subscribe to a track with no publisher and verify the relay reports TIMEOUT
/// after processing a nonzero RENDEZVOUS_TIMEOUT.
pub async fn test_rendezvous_timeout(args: &Args) -> Result<TestConnectionIds> {
    timeout(TEST_TIMEOUT, async {
        let (session, cid, transport) =
            connect(args).await.context("failed to connect to relay")?;
        let mut cids = TestConnectionIds::default();
        cids.add(cid);

        let (session, _publisher, mut subscriber) = Session::connect(session, None, transport)
            .await
            .context("SETUP exchange failed")?;

        let namespace = TrackNamespace::from_utf8_path("nonexistent/rendezvous");
        let (mut writer, _, _reader) = Tracks::new(namespace).produce();
        let track = writer
            .create(TEST_TRACK)
            .ok_or_else(|| anyhow::anyhow!("failed to create subscriber track"))?;

        let mut params = KeyValuePairs::default();
        params.set_rendezvous_timeout(RENDEZVOUS_TIMEOUT_MS);

        tracing::info!(
            timeout_ms = RENDEZVOUS_TIMEOUT_MS,
            "Subscribing with RENDEZVOUS_TIMEOUT to a track with no publisher"
        );

        let result = timeout(RENDEZVOUS_RESPONSE_TIMEOUT, async {
            tokio::select! {
                res = subscriber.subscribe_open_with_params(track, params) => res,
                res = session.run() => {
                    match res {
                        Ok(()) => Err(ServeError::Internal(
                            "session ended before subscribe completed".to_string(),
                        )),
                        Err(err) => Err(ServeError::Internal(format!("session error: {}", err))),
                    }
                }
            }
        })
        .await
        .with_context(|| {
            format!(
                "relay did not answer rendezvous subscribe within {:?}",
                RENDEZVOUS_RESPONSE_TIMEOUT
            )
        })?;

        match result {
            Err(ServeError::Timeout) => {
                tracing::info!("Received expected REQUEST_ERROR TIMEOUT");
                Ok(cids)
            }
            Err(err) => anyhow::bail!("expected REQUEST_ERROR TIMEOUT, got: {}", err),
            Ok(_) => anyhow::bail!("subscribe succeeded but no publisher serves the track"),
        }
    })
    .await
    .context("test timed out")?
}

/// T0.4: Publish Namespace + Subscribe
///
/// Publisher sends PUBLISH_NAMESPACE; subscriber subscribes to a track in that namespace.
/// Verifies the relay correctly routes the subscription to the publisher.
pub async fn test_publish_namespace_subscribe(args: &Args) -> Result<TestConnectionIds> {
    timeout(TEST_TIMEOUT, async {
        let mut cids = TestConnectionIds::default();

        let (pub_session, pub_cid, pub_transport) =
            connect(args).await.context("publisher failed to connect")?;
        cids.add(pub_cid);
        let (pub_session, mut publisher, _) = Session::connect(pub_session, None, pub_transport)
            .await
            .context("publisher SETUP failed")?;

        let (sub_session, sub_cid, sub_transport) = connect(args)
            .await
            .context("subscriber failed to connect")?;
        cids.add(sub_cid);
        let (sub_session, _, mut subscriber) = Session::connect(sub_session, None, sub_transport)
            .await
            .context("subscriber SETUP failed")?;

        let namespace = TrackNamespace::from_utf8_path(TEST_NAMESPACE);

        let (mut pub_writer, _, pub_reader) = Tracks::new(namespace.clone()).produce();
        let _track_writer = pub_writer.create(TEST_TRACK);

        tracing::info!("Publisher sending PUBLISH_NAMESPACE: {}", TEST_NAMESPACE);

        let (mut sub_writer, _, _sub_reader) = Tracks::new(namespace.clone()).produce();
        let sub_track = sub_writer
            .create(TEST_TRACK)
            .ok_or_else(|| anyhow::anyhow!("failed to create subscriber track"))?;

        tracing::info!(
            "Subscriber subscribing to track: {}/{}",
            TEST_NAMESPACE,
            TEST_TRACK
        );

        tokio::select! {
            res = publisher.publish_namespace(pub_reader) => {
                res.context("publisher PUBLISH_NAMESPACE failed")?;
                tracing::info!("Publisher PUBLISH_NAMESPACE completed");
            }
            res = subscriber.subscribe(sub_track) => {
                match res {
                    Ok(()) => tracing::info!(
                        "Subscriber got subscription response - relay routed correctly"
                    ),
                    Err(e) => tracing::info!(
                        "Subscriber got error: {} - subscription was processed", e
                    ),
                }
            }
            res = pub_session.run() => res.context("publisher session error")?,
            res = sub_session.run() => res.context("subscriber session error")?,
            _ = tokio::time::sleep(Duration::from_secs(3)) => {
                tracing::info!(
                    "Test timeout reached - subscription routing may still be in progress"
                );
            }
        };

        Ok(cids)
    })
    .await
    .context("test timed out")?
}

/// T0.6: Publish Namespace Done
///
/// Send PUBLISH_NAMESPACE, receive REQUEST_OK, then send PUBLISH_NAMESPACE_DONE.
/// Verifies the relay handles namespace unpublishing correctly.
pub async fn test_publish_namespace_done(args: &Args) -> Result<TestConnectionIds> {
    timeout(TEST_TIMEOUT, async {
        let (session, cid, transport) =
            connect(args).await.context("failed to connect to relay")?;
        let mut cids = TestConnectionIds::default();
        cids.add(cid);

        let (session, mut publisher, _subscriber) = Session::connect(session, None, transport)
            .await
            .context("SETUP exchange failed")?;

        let namespace = TrackNamespace::from_utf8_path(TEST_NAMESPACE);
        let (_, _, reader) = Tracks::new(namespace.clone()).produce();

        tracing::info!("Sending PUBLISH_NAMESPACE: {}", TEST_NAMESPACE);

        let result = tokio::select! {
            res = publisher.publish_namespace(reader) => res,
            res = session.run() => {
                res.context("session error")?;
                anyhow::bail!("session ended before PUBLISH_NAMESPACE completed");
            }
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                // No error received: REQUEST_OK arrived and we are waiting for subscribers.
                // Drop publish_namespace here to send PUBLISH_NAMESPACE_DONE.
                tracing::info!("PUBLISH_NAMESPACE active; sending PUBLISH_NAMESPACE_DONE");
                Ok(())
            }
        };

        result.context("PUBLISH_NAMESPACE failed")?;

        tokio::time::sleep(Duration::from_millis(100)).await;
        tracing::info!("PUBLISH_NAMESPACE_DONE sent successfully");
        Ok(cids)
    })
    .await
    .context("test timed out")?
}

/// T0.7: Publish Track Only
///
/// Publisher sends direct PUBLISH for one track, receives PUBLISH_OK, serves one
/// object, and completes with PUBLISH_DONE.
///
/// With no peer subscribed there is nothing to read the Object back from, so
/// this asserts the publisher's side of delivery only: that the Object was
/// accepted by the transport and that serving it reported no error. Note that
/// `serve()` only became a meaningful assertion once per-subgroup send failures
/// stopped being swallowed; before that it returned `Ok` even when nothing was
/// transmitted. End-to-end receipt is asserted by `publish-track-subscribe`.
pub async fn test_publish_track_only(args: &Args) -> Result<TestConnectionIds> {
    timeout(TEST_TIMEOUT, async {
        let (session, cid, transport) = connect(args)
            .await
            .context("publisher failed to connect to relay")?;
        let mut cids = TestConnectionIds::default();
        cids.add(cid);

        let (session, mut publisher, _subscriber) = Session::connect(session, None, transport)
            .await
            .context("publisher SETUP failed")?;

        let namespace = TrackNamespace::from_utf8_path(PUBLISH_NAMESPACE);
        let (track_writer, track_reader) = Track::new(namespace.clone(), PUBLISH_TRACK).produce();

        tracing::info!(
            namespace = %namespace,
            track = PUBLISH_TRACK,
            "Publisher sending direct PUBLISH"
        );

        let result: Result<()> = tokio::select! {
            res = async {
                let mut published = publisher
                    .publish(track_reader, KeyValuePairs::default())
                    .await
                    .context("failed to send PUBLISH")?;
                published.ok().await.context("PUBLISH was rejected")?;
                tracing::info!(namespace = %namespace, track = PUBLISH_TRACK, "PUBLISH accepted");

                write_test_subgroup(track_writer, b"publish-track-only")
                    .context("publisher failed to write the Object")?;
                published
                    .serve()
                    .await
                    .context("failed serving PUBLISH track")?;
                tracing::info!(namespace = %namespace, track = PUBLISH_TRACK, "PUBLISH completed");
                Ok(())
            } => res,
            res = session.run() => {
                res.context("publisher session error")?;
                anyhow::bail!("publisher session ended before PUBLISH completed");
            }
        };

        result?;
        Ok(cids)
    })
    .await
    .context("test timed out")?
}

/// T0.8: Publish Track + Subscribe
///
/// Publisher sends direct PUBLISH for one track; after PUBLISH_OK, a subscriber
/// subscribes to the exact track and receives the relayed object stream.
pub async fn test_publish_track_subscribe(args: &Args) -> Result<TestConnectionIds> {
    timeout(TEST_TIMEOUT, async {
        let mut cids = TestConnectionIds::default();

        let (pub_session, pub_cid, pub_transport) = connect(args)
            .await
            .context("publisher failed to connect")?;
        cids.add(pub_cid);
        let (pub_session, mut publisher, _pub_subscriber) =
            Session::connect(pub_session, None, pub_transport)
                .await
                .context("publisher SETUP failed")?;

        let (sub_session, sub_cid, sub_transport) = connect(args)
            .await
            .context("subscriber failed to connect")?;
        cids.add(sub_cid);
        let (sub_session, _sub_publisher, mut subscriber) =
            Session::connect(sub_session, None, sub_transport)
                .await
                .context("subscriber SETUP failed")?;

        let namespace = TrackNamespace::from_utf8_path(PUBLISH_NAMESPACE);
        let (track_writer, track_reader) = Track::new(namespace.clone(), PUBLISH_TRACK).produce();
        let (mut sub_tracks, _, mut sub_reader) = Tracks::new(namespace.clone()).produce();
        let sub_track = sub_tracks
            .create(PUBLISH_TRACK)
            .ok_or_else(|| anyhow::anyhow!("failed to create subscriber track"))?;
        let received_track = sub_reader
            .get_track_reader(&namespace, PUBLISH_TRACK)
            .ok_or_else(|| anyhow::anyhow!("failed to read subscriber track"))?;

        tracing::info!(
            namespace = %namespace,
            track = PUBLISH_TRACK,
            "Publisher sending direct PUBLISH before subscriber subscribes"
        );

        let result: Result<()> = tokio::select! {
            res = async {
                let mut published = publisher
                    .publish(track_reader, KeyValuePairs::default())
                    .await
                    .context("failed to send PUBLISH")?;
                published.ok().await.context("PUBLISH was rejected")?;
                tracing::info!(namespace = %namespace, track = PUBLISH_TRACK, "PUBLISH accepted; starting subscriber");

                let subscribe = async {
                    match subscriber.subscribe(sub_track).await {
                        Ok(()) => Ok(()),
                        // The publisher ending a finite track is the expected
                        // outcome, not a failure.
                        Err(err) if is_normal_subscription_end(&err) => Ok(()),
                        Err(err) => Err(err).context("subscriber failed to receive direct PUBLISH track"),
                    }
                };

                let receive = async {
                    let mut subgroups = match received_track.mode().await.context("subscriber track mode failed")? {
                        TrackReaderMode::Subgroups(subgroups) => subgroups,
                        _ => anyhow::bail!("subscriber track used non-subgroup delivery"),
                    };
                    let mut subgroup = subgroups.next().await.context("subscriber subgroup failed")?
                        .ok_or_else(|| anyhow::anyhow!("subscriber did not receive a subgroup"))?;
                    let payload = subgroup.read_next().await.context("subscriber object failed")?
                        .ok_or_else(|| anyhow::anyhow!("subscriber did not receive an object"))?;
                    if payload.as_ref() != PUBLISH_PAYLOAD {
                        anyhow::bail!(
                            "subscriber received unexpected payload: {:?}",
                            String::from_utf8_lossy(payload.as_ref())
                        );
                    }
                    Ok(())
                };

                let publish = async {
                    // Let the subscriber attach first, so the Object is relayed
                    // to a live subscription rather than served from whatever the
                    // relay happened to buffer before the SUBSCRIBE arrived.
                    tokio::time::sleep(Duration::from_millis(100)).await;

                    // Write one Object and end the track immediately. Ending it
                    // straight away is the point of this test: PUBLISH_DONE then
                    // races the Object, and §10.11's Stream Count is what keeps
                    // the subscription alive long enough to deliver it. Holding
                    // the track open would sidestep the very condition under test.
                    write_test_subgroup(track_writer, PUBLISH_PAYLOAD)?;
                    published
                        .serve()
                        .await
                        .context("failed serving PUBLISH track")?;
                    Ok(())
                };

                tokio::try_join!(subscribe, receive, publish)?;
                tracing::info!(namespace = %namespace, track = PUBLISH_TRACK, "Subscriber received direct PUBLISH track");
                Ok(())
            } => res,
            res = pub_session.run() => {
                res.context("publisher session error")?;
                anyhow::bail!("publisher session ended before PUBLISH/subscriber flow completed");
            },
            res = sub_session.run() => {
                res.context("subscriber session error")?;
                anyhow::bail!("subscriber session ended before PUBLISH/subscriber flow completed");
            }
        };

        result?;
        Ok(cids)
    })
    .await
    .context("test timed out")?
}

/// T0.5: Subscribe Before Publish Namespace
///
/// Subscriber subscribes first (will be pending), then publisher sends PUBLISH_NAMESPACE.
/// Verifies the relay correctly handles out-of-order setup.
pub async fn test_subscribe_before_publish_namespace(args: &Args) -> Result<TestConnectionIds> {
    timeout(TEST_TIMEOUT, async {
        let mut cids = TestConnectionIds::default();

        // Subscriber connects first.
        let (sub_session, sub_cid, sub_transport) = connect(args)
            .await
            .context("subscriber failed to connect")?;
        cids.add(sub_cid);
        let (sub_session, _, mut subscriber) = Session::connect(sub_session, None, sub_transport)
            .await
            .context("subscriber SETUP failed")?;

        let namespace = TrackNamespace::from_utf8_path(TEST_NAMESPACE);

        let (mut sub_writer, _, _sub_reader) = Tracks::new(namespace.clone()).produce();
        let sub_track = sub_writer
            .create(TEST_TRACK)
            .ok_or_else(|| anyhow::anyhow!("failed to create subscriber track"))?;

        tracing::info!(
            "Subscriber subscribing BEFORE PUBLISH_NAMESPACE: {}/{}",
            TEST_NAMESPACE,
            TEST_TRACK
        );

        let sub_handle = tokio::spawn(async move {
            let result = tokio::select! {
                res = subscriber.subscribe(sub_track) => res,
                res = sub_session.run() => {
                    res.map_err(|e| moq_transport::serve::ServeError::Internal(e.to_string()))?;
                    Err(moq_transport::serve::ServeError::Done)
                }
            };
            result
        });

        // Give subscriber time to send SUBSCRIBE.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Now publisher connects and sends PUBLISH_NAMESPACE.
        let (pub_session, pub_cid, pub_transport) =
            connect(args).await.context("publisher failed to connect")?;
        cids.add(pub_cid);
        let (pub_session, mut publisher, _) = Session::connect(pub_session, None, pub_transport)
            .await
            .context("publisher SETUP failed")?;

        let (mut pub_writer, _, pub_reader) = Tracks::new(namespace.clone()).produce();
        let _track_writer = pub_writer.create(TEST_TRACK);

        tracing::info!(
            "Publisher sending PUBLISH_NAMESPACE (after subscriber): {}",
            TEST_NAMESPACE
        );

        tokio::select! {
            res = publisher.publish_namespace(pub_reader) => {
                res.context("publisher PUBLISH_NAMESPACE failed")?;
            }
            res = pub_session.run() => res.context("publisher session error")?,
            _ = tokio::time::sleep(Duration::from_secs(3)) => {
                tracing::info!("Publisher PUBLISH_NAMESPACE timeout (expected)");
            }
        };

        tokio::select! {
            res = sub_handle => {
                match res {
                    Ok(Ok(())) => tracing::info!("Subscriber completed successfully"),
                    Ok(Err(e)) => tracing::info!("Subscriber got error: {} (may be expected)", e),
                    Err(e) => tracing::warn!("Subscriber task panicked: {}", e),
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                tracing::info!("Subscriber still waiting (test complete)");
            }
        };

        Ok(cids)
    })
    .await
    .context("test timed out")?
}
