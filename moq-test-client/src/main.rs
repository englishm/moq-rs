// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MoQT Interop Test Client
//!
//! A standardized test client for MoQT interoperability testing.
//! This tool can run various test scenarios against a MoQT relay to verify
//! protocol compliance and interoperability.
//!
//! ## Usage
//!
//! ```bash
//! # Run all tests against a relay
//! moq-test-client --relay https://localhost:4443
//!
//! # Run a specific test
//! moq-test-client --relay https://localhost:4443 --test setup-only
//!
//! # List available tests
//! moq-test-client --list
//!
//! # Run an ad-hoc moq-test scenario from a 16-field tuple
//! moq-test-client --relay https://localhost:4443 \
//!   --moq-test-tuple "moq-test-00/0/0/0/2/4/5/64/32/2/1/1/0/-1/-1/0"
//! ```
//!
//! The `moq-test-*` tests implement the moq-test protocol
//! (draft-afrind-moq-test) as a self-publishing scoreboard test: the client
//! generates a deterministic object set from the tuple, publishes it through
//! the relay, subscribes, and verifies the exact set plus terminal signals.

use std::net;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use url::Url;

mod moqtest;
mod scenarios;

/// MoQT Interop Test Client
#[derive(Parser, Clone)]
#[command(name = "moq-test-client")]
#[command(about = "MoQT Interoperability Test Client", long_about = None)]
pub struct Args {
    /// Relay URL to test against (e.g., https://localhost:4443)
    #[arg(
        short,
        long,
        default_value = "https://localhost:4443",
        env = "RELAY_URL"
    )]
    pub relay: Url,

    /// Specific test to run (runs all if not specified)
    #[arg(short, long, env = "TESTCASE")]
    pub test: Option<TestCase>,

    /// Run an ad-hoc moq-test scenario from a tuple: 16 '/'-separated
    /// namespace fields (e.g. "moq-test-00/0/0/0/2/4/5/64/32/2/1/1/0///"),
    /// subscribe-first choreography. Blank fields select defaults.
    #[arg(long, value_name = "TUPLE", conflicts_with_all = ["test", "list"])]
    pub moq_test_tuple: Option<String>,

    /// List available test cases and exit
    #[arg(short, long)]
    pub list: bool,

    /// Listen for UDP packets on the given address
    #[arg(long, default_value = "[::]:0")]
    pub bind: net::SocketAddr,

    /// The TLS configuration
    #[command(flatten)]
    pub tls: moq_native_ietf::tls::Args,

    /// Enable verbose output
    #[arg(short, long, env = "VERBOSE")]
    pub verbose: bool,
}

/// Available test cases
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TestCase {
    /// T0.1: Connect, complete SETUP exchange, close gracefully
    SetupOnly,
    /// T0.2: Connect, send PUBLISH_NAMESPACE, receive REQUEST_OK, close
    PublishNamespaceOnly,
    /// T0.3: Subscribe to non-existent track, expect error
    SubscribeError,
    /// T0.4: Publisher sends PUBLISH_NAMESPACE, subscriber subscribes, verify handshake
    PublishNamespaceSubscribe,
    /// T0.5: Subscriber subscribes before publisher sends PUBLISH_NAMESPACE
    SubscribeBeforePublishNamespace,
    /// T0.6: Send PUBLISH_NAMESPACE, receive REQUEST_OK, send PUBLISH_NAMESPACE_DONE
    PublishNamespaceDone,
    /// T0.7: Publisher sends PUBLISH for one track, receives PUBLISH_OK, then completes
    PublishTrackOnly,
    /// T0.8: Publisher sends PUBLISH for one track, subscriber receives it through relay routing
    PublishTrackSubscribe,
    /// T0.9: SUBSCRIBE with RENDEZVOUS_TIMEOUT expires with TIMEOUT
    RendezvousTimeout,
    /// moq-test: one subgroup per group, scoreboard-verified
    MoqTestSubgroupPerGroup,
    /// moq-test: one subgroup per group with End of Group markers
    MoqTestSubgroupPerGroupEog,
    /// moq-test: one subgroup per object
    MoqTestSubgroupPerObject,
    /// moq-test: two subgroups per group (parity) with End of Group markers
    MoqTestTwoSubgroupsEog,
    /// moq-test: datagram forwarding
    MoqTestDatagram,
    /// moq-test: datagram forwarding with End of Group markers
    MoqTestDatagramEog,
    /// moq-test: integer and variable extensions on every object
    MoqTestExtensions,
    /// moq-test: non-default start/increment arithmetic
    MoqTestIncrements,
    /// moq-test: SUBSCRIBE with FORWARD=0 (setup and PUBLISH_DONE, no data)
    MoqTestForwardZero,
}

impl TestCase {
    fn all() -> Vec<TestCase> {
        vec![
            TestCase::SetupOnly,
            TestCase::PublishNamespaceOnly,
            TestCase::SubscribeError,
            TestCase::PublishNamespaceSubscribe,
            TestCase::SubscribeBeforePublishNamespace,
            TestCase::PublishNamespaceDone,
            TestCase::PublishTrackOnly,
            TestCase::PublishTrackSubscribe,
            TestCase::RendezvousTimeout,
            TestCase::MoqTestSubgroupPerGroup,
            TestCase::MoqTestSubgroupPerGroupEog,
            TestCase::MoqTestSubgroupPerObject,
            TestCase::MoqTestTwoSubgroupsEog,
            TestCase::MoqTestDatagram,
            TestCase::MoqTestDatagramEog,
            TestCase::MoqTestExtensions,
            TestCase::MoqTestIncrements,
            TestCase::MoqTestForwardZero,
        ]
    }

    fn name(&self) -> &'static str {
        match self {
            TestCase::SetupOnly => "setup-only",
            TestCase::PublishNamespaceOnly => "publish-namespace-only",
            TestCase::SubscribeError => "subscribe-error",
            TestCase::PublishNamespaceSubscribe => "publish-namespace-subscribe",
            TestCase::SubscribeBeforePublishNamespace => "subscribe-before-publish-namespace",
            TestCase::PublishNamespaceDone => "publish-namespace-done",
            TestCase::PublishTrackOnly => "publish-track-only",
            TestCase::PublishTrackSubscribe => "publish-track-subscribe",
            TestCase::RendezvousTimeout => "rendezvous-timeout",
            TestCase::MoqTestSubgroupPerGroup => "moq-test-subgroup-per-group",
            TestCase::MoqTestSubgroupPerGroupEog => "moq-test-subgroup-per-group-eog",
            TestCase::MoqTestSubgroupPerObject => "moq-test-subgroup-per-object",
            TestCase::MoqTestTwoSubgroupsEog => "moq-test-two-subgroups-eog",
            TestCase::MoqTestDatagram => "moq-test-datagram",
            TestCase::MoqTestDatagramEog => "moq-test-datagram-eog",
            TestCase::MoqTestExtensions => "moq-test-extensions",
            TestCase::MoqTestIncrements => "moq-test-increments",
            TestCase::MoqTestForwardZero => "moq-test-forward-zero",
        }
    }
}

/// Result of running a test case
#[derive(Debug)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub duration: Duration,
    pub message: Option<String>,
    pub cids: Vec<String>,
    /// Multi-connection tests where the subscriber connected first (affects
    /// CID labeling only).
    subscriber_first: bool,
}

impl TestResult {
    fn success(
        name: String,
        duration: Duration,
        cids: Vec<String>,
        subscriber_first: bool,
    ) -> Self {
        Self {
            name,
            passed: true,
            duration,
            message: None,
            cids,
            subscriber_first,
        }
    }

    fn failure(name: String, duration: Duration, message: String) -> Self {
        Self {
            name,
            passed: false,
            duration,
            message: Some(message),
            cids: Vec::new(),
            subscriber_first: false,
        }
    }
}

/// Run a moq-test scenario under the shared timeout and translate the
/// verification report into connection IDs or an error.
async fn run_moqtest_scenario(
    args: &Args,
    scenario: &moqtest::Scenario,
) -> Result<scenarios::TestConnectionIds> {
    let (cids, report) =
        tokio::time::timeout(scenarios::TEST_TIMEOUT, moqtest::run(args, scenario))
            .await
            .context("test timed out")??;

    if report.passed() {
        tracing::info!("moq-test passed: {}", report.summary());
        Ok(cids)
    } else {
        bail!(
            "moq-test verification failed: {}",
            report.failures.join("; ")
        )
    }
}

/// Run a named moq-test scenario.
async fn run_moqtest(args: &Args, test_case: TestCase) -> Result<scenarios::TestConnectionIds> {
    let scenario = moqtest::scenario(test_case.name())?
        .ok_or_else(|| anyhow::anyhow!("unregistered moq-test scenario"))?;
    run_moqtest_scenario(args, scenario).await
}

/// Run a single test case
async fn run_test(args: &Args, test_case: TestCase) -> TestResult {
    let start = Instant::now();

    let result = match test_case {
        TestCase::SetupOnly => scenarios::test_setup_only(args).await,
        TestCase::PublishNamespaceOnly => scenarios::test_publish_namespace_only(args).await,
        TestCase::SubscribeError => scenarios::test_subscribe_error(args).await,
        TestCase::PublishNamespaceSubscribe => {
            scenarios::test_publish_namespace_subscribe(args).await
        }
        TestCase::SubscribeBeforePublishNamespace => {
            scenarios::test_subscribe_before_publish_namespace(args).await
        }
        TestCase::PublishNamespaceDone => scenarios::test_publish_namespace_done(args).await,
        TestCase::PublishTrackOnly => scenarios::test_publish_track_only(args).await,
        TestCase::PublishTrackSubscribe => scenarios::test_publish_track_subscribe(args).await,
        TestCase::RendezvousTimeout => scenarios::test_rendezvous_timeout(args).await,
        TestCase::MoqTestSubgroupPerGroup
        | TestCase::MoqTestSubgroupPerGroupEog
        | TestCase::MoqTestSubgroupPerObject
        | TestCase::MoqTestTwoSubgroupsEog
        | TestCase::MoqTestDatagram
        | TestCase::MoqTestDatagramEog
        | TestCase::MoqTestExtensions
        | TestCase::MoqTestIncrements
        | TestCase::MoqTestForwardZero => run_moqtest(args, test_case).await,
    };

    let duration = start.elapsed();

    // T0.5 and every moq-test scenario connect the subscriber first.
    let subscriber_first = test_case == TestCase::SubscribeBeforePublishNamespace
        || test_case.name().starts_with("moq-test-");
    match result {
        Ok(cids) => TestResult::success(
            test_case.name().to_string(),
            duration,
            cids.cids,
            subscriber_first,
        ),
        Err(e) => TestResult::failure(test_case.name().to_string(), duration, format!("{:#}", e)),
    }
}

fn print_tap_result(test_number: usize, result: &TestResult, verbose: bool) {
    let status = if result.passed { "ok" } else { "not ok" };
    println!("{} {} - {}", status, test_number, result.name);

    // YAML diagnostic block
    println!("  ---");
    println!("  duration_ms: {}", result.duration.as_millis());

    // Connection IDs for mlog correlation
    match result.cids.len() {
        0 => {}
        1 => println!("  connection_id: {}", result.cids[0]),
        2 => {
            // Multi-connection tests: first is publisher, second is subscriber
            // (except subscribe-first choreographies)
            if result.subscriber_first {
                println!("  subscriber_connection_id: {}", result.cids[0]);
                println!("  publisher_connection_id: {}", result.cids[1]);
            } else {
                println!("  publisher_connection_id: {}", result.cids[0]);
                println!("  subscriber_connection_id: {}", result.cids[1]);
            }
        }
        _ => {
            // More than 2 CIDs - just list them all
            for (i, cid) in result.cids.iter().enumerate() {
                println!("  connection_id_{}: {}", i + 1, cid);
            }
        }
    }

    // Error message for failed tests
    if let Some(ref msg) = result.message {
        // Escape quotes and newlines for YAML string
        let escaped = if verbose {
            msg.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
        } else {
            // Non-verbose: just first line
            msg.lines()
                .next()
                .unwrap_or(msg)
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        };
        println!("  message: \"{}\"", escaped);
    }

    println!("  ...");
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing with env filter (respects RUST_LOG environment variable)
    // Default to info level, but suppress quinn's verbose output
    //
    // Logs go to stderr so they can't corrupt the TAP report this binary
    // writes to stdout.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,quinn=warn")),
        )
        .init();

    let args = Args::parse();

    // List tests and exit if requested
    // Output one identifier per line for machine parsing (per TEST-CLIENT-INTERFACE.md)
    if args.list {
        for tc in TestCase::all() {
            println!("{}", tc.name());
        }
        return Ok(());
    }

    // Ad-hoc moq-test tuple: parse, run, report as a single TAP line.
    if let Some(tuple) = &args.moq_test_tuple {
        let fields: Vec<String> = tuple.split('/').map(str::to_string).collect();
        let start = Instant::now();
        let result = async {
            let params = moqtest::MoqTestParams::from_namespace_fields(&fields)
                .context("invalid moq-test tuple")?;
            let scenario = moqtest::Scenario {
                params,
                forward_zero: false,
            };
            run_moqtest_scenario(&args, &scenario).await
        }
        .await;
        let duration = start.elapsed();
        let result = match result {
            Ok(cids) => {
                TestResult::success("moq-test-adhoc".to_string(), duration, cids.cids, true)
            }
            Err(e) => {
                TestResult::failure("moq-test-adhoc".to_string(), duration, format!("{:#}", e))
            }
        };
        println!("TAP version 14");
        println!("# moq-test-client v{}", env!("CARGO_PKG_VERSION"));
        println!("# Relay: {}", args.relay);
        println!("1..1");
        print_tap_result(1, &result, args.verbose);
        return if result.passed {
            Ok(())
        } else {
            std::process::exit(1);
        };
    }

    let tests_to_run = match args.test {
        Some(tc) => vec![tc],
        None => TestCase::all(),
    };

    // TAP version 14 header with run-level comments
    println!("TAP version 14");
    println!("# moq-test-client v{}", env!("CARGO_PKG_VERSION"));
    println!("# Relay: {}", args.relay);
    println!("1..{}", tests_to_run.len());

    let mut failed = 0;

    for (i, test_case) in tests_to_run.iter().enumerate() {
        let result = run_test(&args, *test_case).await;
        print_tap_result(i + 1, &result, args.verbose);

        if !result.passed {
            failed += 1;
        }
    }

    if failed == 0 {
        Ok(())
    } else {
        std::process::exit(1);
    }
}
