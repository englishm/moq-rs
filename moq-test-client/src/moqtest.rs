// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! moq-test protocol implementation: generator plus scoreboard verifier.
//!
//! The client is both the original publisher and the end subscriber of the
//! same track: it derives a deterministic object set from a moq-test tuple
//! (the "scoreboard"), publishes it through the relay under test, subscribes
//! to the same track, and verifies that exactly the expected set arrives,
//! with correct metadata and terminal signals. The relay under test needs no
//! moq-test implementation; it sees an ordinary PUBLISH and SUBSCRIBE.
//!
//! The tuple semantics of draft-afrind-moq-test, made precise:
//!
//! - Groups: `start_group + k*group_increment` while `<= last_group`.
//! - Objects per group: `start_object + k*object_increment` while
//!   `<= last_object`. Field 6 (`objects_per_group`) constrains the valid
//!   range of `last_object` but does not drive generation.
//! - When end-of-group markers are enabled, the final slot (`object_id ==
//!   last_object`) of each group is an End of Group status object instead of
//!   a data object.
//! - A data object's size is `size_object_zero` when `object_id ==
//!   start_object`, otherwise `size_object_rest`.
//! - Payload is the byte `b't'` repeated.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use tokio::time::sleep;

use moq_transport::coding::{KeyValuePairs, TrackNamespace, TupleField};
use moq_transport::data::{ExtensionHeaders, ObjectStatus};
use moq_transport::message::PublishDoneCode;
use moq_transport::serve::{Datagram, Subgroup, Track, TrackReader, TrackReaderMode, TrackWriter};
use moq_transport::session::{PublishDoneInfo, Session, Subscribe};

use crate::scenarios::{is_normal_subscription_end, TestConnectionIds};
use crate::Args;

/// Tuple field 0: identifies the moq-test protocol version.
pub const TUPLE_FIELD_0: &str = "moq-test-00";

/// moq-test namespaces carry exactly this many tuple fields.
pub const NUM_TUPLE_FIELDS: usize = 16;

/// Publisher priority used for every subgroup and datagram.
const PRIORITY: u8 = 128;

/// Rendezvous window requested on SUBSCRIBE so the relay holds the
/// subscription pending until the publisher appears (subscribe-first
/// choreography).
const RENDEZVOUS_TIMEOUT_MS: u64 = 10_000;

/// Worst-case datagram payload budget; larger datagram-mode objects are a
/// test-declaration error.
const MAX_DATAGRAM_PAYLOAD: usize = 1200;

/// Worst-case object size in any mode. Payloads are materialized as
/// `vec![b't'; size]` during generation, so an absurd operator-supplied
/// size must be a clean validation error, not an allocator failure.
/// Generous test-scale limit; the datagram budget is much tighter.
const MAX_OBJECT_SIZE: u64 = 16 * 1024 * 1024;

/// moq-test forwarding preference (tuple field 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forwarding {
    /// 0: one subgroup stream per group, Subgroup ID 0.
    SubgroupPerGroup,
    /// 1: one subgroup stream per object, Subgroup ID == Object ID.
    SubgroupPerObject,
    /// 2: two subgroup streams per group; objects alternate by offset from
    /// start_object: `(object_id - start_object) % 2` is the Subgroup ID.
    TwoSubgroupsPerGroup,
    /// 3: one datagram per object.
    Datagram,
}

impl Forwarding {
    fn from_u64(value: u64) -> Result<Self> {
        match value {
            0 => Ok(Self::SubgroupPerGroup),
            1 => Ok(Self::SubgroupPerObject),
            2 => Ok(Self::TwoSubgroupsPerGroup),
            3 => Ok(Self::Datagram),
            _ => bail!("invalid forwarding preference: {value}"),
        }
    }

    fn to_u64(self) -> u64 {
        match self {
            Self::SubgroupPerGroup => 0,
            Self::SubgroupPerObject => 1,
            Self::TwoSubgroupsPerGroup => 2,
            Self::Datagram => 3,
        }
    }
}

/// A parsed moq-test tuple: the test declaration shared by the generator and
/// the verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoqTestParams {
    pub forwarding: Forwarding,
    pub start_group: u64,
    pub start_object: u64,
    pub last_group: u64,
    pub last_object: u64,
    pub objects_per_group: u64,
    pub size_object_zero: u64,
    pub size_object_rest: u64,
    pub frequency_ms: u64,
    pub group_increment: u64,
    pub object_increment: u64,
    pub eog_markers: bool,
    /// Tuple field 13 value; the extension ID is `2 * value`.
    pub int_extension: Option<u64>,
    /// Tuple field 14 value; the extension ID is `2 * value + 1`.
    pub var_extension: Option<u64>,
    pub delivery_timeout_ms: Option<u64>,
}

impl MoqTestParams {
    /// Parse the 16 namespace tuple fields. Fields 1-15 may be blank, which
    /// selects the default (matching draft-afrind-moq-test).
    pub fn from_namespace_fields(fields: &[String]) -> Result<Self> {
        if fields.len() != NUM_TUPLE_FIELDS {
            bail!(
                "moq-test namespace must have {} fields, got {}",
                NUM_TUPLE_FIELDS,
                fields.len()
            );
        }
        if fields[0] != TUPLE_FIELD_0 {
            bail!(
                "tuple field 0 must be {TUPLE_FIELD_0:?}, got {:?}",
                fields[0]
            );
        }

        let mut rest = fields[1..].iter().map(String::as_str);

        let forwarding = Forwarding::from_u64(next_u64(&mut rest, 0)?)?;
        let start_group = next_u64(&mut rest, 0)?;
        let start_object = next_u64(&mut rest, 0)?;
        let last_group = next_u64(&mut rest, (1u64 << 62) - 1)?;
        // Field 5's default is "the maximum value" (draft): computed once the
        // other fields are known, below.
        let last_object = optional_u64(&mut rest)?;
        let objects_per_group = next_u64(&mut rest, 10)?;
        let size_object_zero = next_u64(&mut rest, 1024)?;
        let size_object_rest = next_u64(&mut rest, 100)?;
        let frequency_ms = next_u64(&mut rest, 1000)?;
        let group_increment = next_u64(&mut rest, 1)?;
        let object_increment = next_u64(&mut rest, 1)?;
        let eog_markers = match next_u64(&mut rest, 0)? {
            0 => false,
            1 => true,
            other => bail!("end-of-group marker field must be 0 or 1, got {other}"),
        };
        let int_extension = optional_extension(&mut rest)?;
        let var_extension = optional_extension(&mut rest)?;
        let delivery_timeout_ms = optional_timeout(&mut rest)?;

        let last_object = match last_object {
            Some(value) => value,
            None => default_last_object(
                start_object,
                objects_per_group,
                eog_markers,
                object_increment,
            )?,
        };

        let params = Self {
            forwarding,
            start_group,
            start_object,
            last_group,
            last_object,
            objects_per_group,
            size_object_zero,
            size_object_rest,
            frequency_ms,
            group_increment,
            object_increment,
            eog_markers,
            int_extension,
            var_extension,
            delivery_timeout_ms,
        };
        params.validate()?;
        Ok(params)
    }

    /// Serialize as the 16 namespace tuple fields.
    ///
    /// moq-transport cannot encode empty namespace fields, so "no extension"
    /// is written as `-1` (the de-facto wire encoding of a disabled test
    /// extension) and "no delivery timeout" as `0`. The parser accepts
    /// blank for all three as well (the draft's spelling of "default").
    pub fn to_namespace_fields(&self) -> Vec<String> {
        vec![
            TUPLE_FIELD_0.to_string(),
            self.forwarding.to_u64().to_string(),
            self.start_group.to_string(),
            self.start_object.to_string(),
            self.last_group.to_string(),
            self.last_object.to_string(),
            self.objects_per_group.to_string(),
            self.size_object_zero.to_string(),
            self.size_object_rest.to_string(),
            self.frequency_ms.to_string(),
            self.group_increment.to_string(),
            self.object_increment.to_string(),
            (self.eog_markers as u64).to_string(),
            self.int_extension
                .map_or_else(|| "-1".to_string(), |v| v.to_string()),
            self.var_extension
                .map_or_else(|| "-1".to_string(), |v| v.to_string()),
            self.delivery_timeout_ms
                .map_or_else(|| "0".to_string(), |v| v.to_string()),
        ]
    }

    /// Build the track namespace for this tuple. Serialization never emits
    /// empty fields (moq-transport cannot encode them); see
    /// [`Self::to_namespace_fields`].
    pub fn to_namespace(&self) -> TrackNamespace {
        let mut ns = TrackNamespace::new();
        for field in self.to_namespace_fields() {
            ns.add(TupleField::from_utf8(&field));
        }
        ns
    }

    fn validate(&self) -> Result<()> {
        if self.start_group > self.last_group {
            bail!(
                "start group {} exceeds last group {}",
                self.start_group,
                self.last_group
            );
        }
        if self.start_object > self.last_object {
            bail!(
                "start object {} exceeds last object {}",
                self.start_object,
                self.last_object
            );
        }
        if self.group_increment == 0 {
            bail!("group increment cannot be zero");
        }
        if self.object_increment == 0 {
            bail!("object increment cannot be zero");
        }
        if self.objects_per_group == 0 {
            bail!("objects per group cannot be zero");
        }

        // Field 6 constrains how far `last_object` may reach: the object
        // series may contain at most `objects_per_group` data objects plus
        // one EOG marker slot. (the naive formula assumes start_object == 0;
        // this is the same rule offset by start_object.)
        let max_last = default_last_object(
            self.start_object,
            self.objects_per_group,
            self.eog_markers,
            self.object_increment,
        )?;
        if self.last_object > max_last {
            bail!(
                "last object {} exceeds maximum {} for {} objects per group",
                self.last_object,
                max_last,
                self.objects_per_group
            );
        }

        let max_size = self.size_object_zero.max(self.size_object_rest);
        if max_size > MAX_OBJECT_SIZE {
            bail!("object size {max_size} exceeds the {MAX_OBJECT_SIZE} byte limit");
        }

        if self.forwarding == Forwarding::Datagram && max_size as usize > MAX_DATAGRAM_PAYLOAD {
            bail!("datagram-mode object size {max_size} exceeds budget {MAX_DATAGRAM_PAYLOAD}");
        }

        // Bound the materialized ranges: the scoreboard eagerly allocates one
        // entry per group and object, so unbounded tuple fields (a blank
        // last_group defaults to (1<<62)-1) would exhaust memory before any
        // network I/O. Generous test-scale limits. u128 arithmetic: an
        // explicit last_group/last_object near u64::MAX must not wrap the
        // +1 into passing the check.
        const MAX_GROUPS: u128 = 100_000;
        const MAX_DATA_OBJECTS: u128 = 1_000_000;
        let group_count =
            u128::from(self.last_group - self.start_group) / u128::from(self.group_increment) + 1;
        if group_count > MAX_GROUPS {
            bail!("group series of {group_count} groups exceeds the {MAX_GROUPS} group limit");
        }
        let per_group = u128::from(self.last_object - self.start_object)
            / u128::from(self.object_increment)
            + 1;
        let total_objects = group_count * per_group;
        if total_objects > MAX_DATA_OBJECTS {
            bail!(
                "track would carry {total_objects} objects, exceeding the {MAX_DATA_OBJECTS} object limit"
            );
        }
        Ok(())
    }

    /// The group ID series, ascending.
    pub fn groups(&self) -> Vec<u64> {
        series(self.start_group, self.last_group, self.group_increment)
    }

    /// The per-group object ID series, ascending. When EOG markers are
    /// enabled the final entry is the EOG marker slot.
    pub fn objects(&self) -> Vec<u64> {
        series(self.start_object, self.last_object, self.object_increment)
    }

    /// Object IDs that carry data (i.e. excluding the EOG marker slot).
    pub fn data_objects(&self) -> Vec<u64> {
        let mut objects = self.objects();
        if self.eog_markers {
            objects.pop();
        }
        objects
    }

    pub fn object_size(&self, object_id: u64) -> u64 {
        if object_id == self.start_object {
            self.size_object_zero
        } else {
            self.size_object_rest
        }
    }

    pub fn int_extension_id(&self) -> Option<u64> {
        self.int_extension.map(|v| 2 * v)
    }

    pub fn var_extension_id(&self) -> Option<u64> {
        self.var_extension.map(|v| 2 * v + 1)
    }

    /// Number of data objects expected across the whole track.
    pub fn expected_data_objects(&self) -> u64 {
        self.groups().len() as u64 * self.data_objects().len() as u64
    }

    /// Number of data streams the subscription should receive (PUBLISH_DONE
    /// Stream Count).
    pub fn expected_stream_count(&self) -> u64 {
        let groups = self.groups().len() as u64;
        match self.forwarding {
            Forwarding::SubgroupPerGroup => groups,
            Forwarding::SubgroupPerObject => groups * self.objects().len() as u64,
            Forwarding::TwoSubgroupsPerGroup => 2 * groups,
            Forwarding::Datagram => 0,
        }
    }

    /// Subgroup ID an object belongs to (subgroup modes only).
    fn subgroup_for(&self, object_id: u64) -> Option<u64> {
        match self.forwarding {
            Forwarding::SubgroupPerGroup => Some(0),
            Forwarding::SubgroupPerObject => Some(object_id),
            // Parity of the offset from start_object:
            // (object_id - start_object) % 2 — with start_object == 0 this
            // is plain parity.
            Forwarding::TwoSubgroupsPerGroup => Some((object_id - self.start_object) % 2),
            Forwarding::Datagram => None,
        }
    }
}

fn series(start: u64, last: u64, increment: u64) -> Vec<u64> {
    let mut values = Vec::new();
    let mut current = start;
    while current <= last {
        values.push(current);
        let Some(next) = current.checked_add(increment) else {
            break;
        };
        current = next;
    }
    values
}

/// The largest valid `last_object`: `objects_per_group` data objects plus
/// one EOG marker slot, offset by `start_object`. Also the field-5 default.
fn default_last_object(
    start_object: u64,
    objects_per_group: u64,
    eog_markers: bool,
    object_increment: u64,
) -> Result<u64> {
    let slots = objects_per_group
        .checked_add(u64::from(eog_markers))
        .context("object series overflow")?;
    start_object
        .checked_add(
            slots
                .saturating_sub(1)
                .checked_mul(object_increment)
                .context("object series overflow")?,
        )
        .context("object series overflow")
}

fn next_u64<'a>(rest: &mut impl Iterator<Item = &'a str>, default: u64) -> Result<u64> {
    match rest.next() {
        Some("") => Ok(default),
        Some(field) => field
            .parse()
            .with_context(|| format!("invalid tuple integer: {field:?}")),
        None => bail!("namespace ended early"),
    }
}

fn optional_u64<'a>(rest: &mut impl Iterator<Item = &'a str>) -> Result<Option<u64>> {
    match rest.next() {
        Some("") | None => Ok(None),
        Some(field) => field
            .parse()
            .map(Some)
            .with_context(|| format!("invalid tuple integer: {field:?}")),
    }
}

/// Fields 13/14: blank or a negative value (the de-facto wire encoding of
/// a disabled extension) both mean "no extension".
fn optional_extension<'a>(rest: &mut impl Iterator<Item = &'a str>) -> Result<Option<u64>> {
    match rest.next() {
        Some("") | None => Ok(None),
        Some(field) => {
            let value: i64 = field
                .parse()
                .with_context(|| format!("invalid tuple integer: {field:?}"))?;
            Ok((value >= 0).then_some(value as u64))
        }
    }
}

/// Field 15: blank or 0 means "no delivery timeout".
fn optional_timeout<'a>(rest: &mut impl Iterator<Item = &'a str>) -> Result<Option<u64>> {
    match rest.next() {
        Some("") | None => Ok(None),
        Some(field) => {
            let value: u64 = field
                .parse()
                .with_context(|| format!("invalid tuple integer: {field:?}"))?;
            Ok((value != 0).then_some(value))
        }
    }
}

/// xorshift64* PRNG: deterministic-quality random values for extension
/// payloads without taking a dependency on a rand crate.
struct Prng(u64);

impl Prng {
    fn from_entropy() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e3779b97f4a7c15);
        // Avoid the all-zero fixed point; mix in the process ID so parallel
        // runs differ even at equal clock readings.
        Self(nanos ^ (u64::from(std::process::id()) << 32) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next() >> 32) as u8).collect()
    }
}

/// A per-run unique track name. Any unique name is valid for the same
/// tuple-derived content, so a fresh name per run acts as a cache buster:
/// the relay cannot serve stale objects from an earlier run.
pub fn run_track_name() -> String {
    let mut prng = Prng::from_entropy();
    format!("moq-test-{:016x}", prng.next())
}

/// Build the extension headers for one object, per tuple fields 13/14.
fn object_extensions(params: &MoqTestParams, prng: &mut Prng) -> Option<ExtensionHeaders> {
    let mut extensions = ExtensionHeaders::new();
    if let Some(id) = params.int_extension_id() {
        extensions.set_intvalue(id, prng.next());
    }
    if let Some(id) = params.var_extension_id() {
        let len = (prng.next() % 20 + 1) as usize;
        extensions.set_bytesvalue(id, prng.bytes(len));
    }
    if extensions.is_empty() {
        None
    } else {
        Some(extensions)
    }
}

fn object_payload(params: &MoqTestParams, object_id: u64) -> Bytes {
    Bytes::from(vec![b't'; params.object_size(object_id) as usize])
}

/// Generate the object set into the track, per the tuple.
///
/// Returns when every object has been handed to the transport; the caller
/// then completes the publish (PUBLISH_DONE) via `Published::serve`.
pub async fn generate(params: &MoqTestParams, track: TrackWriter) -> Result<()> {
    let mut prng = Prng::from_entropy();
    match params.forwarding {
        Forwarding::SubgroupPerGroup => {
            let mut subgroups = track.subgroups().context("failed to enter subgroup mode")?;
            for group in params.groups() {
                let mut subgroup = subgroups
                    .create(Subgroup {
                        group_id: group,
                        subgroup_id: 0,
                        priority: PRIORITY,
                    })
                    .context("failed to create subgroup")?;
                write_group_objects(params, &mut subgroup, &mut prng).await?;
                subgroup.finish().context("failed to finish subgroup")?;
            }
        }
        Forwarding::SubgroupPerObject => {
            let mut subgroups = track.subgroups().context("failed to enter subgroup mode")?;
            for group in params.groups() {
                for object in params.objects() {
                    let mut subgroup = subgroups
                        .create(Subgroup {
                            group_id: group,
                            subgroup_id: object,
                            priority: PRIORITY,
                        })
                        .context("failed to create subgroup")?;
                    write_object(params, &mut subgroup, object, &mut prng)?;
                    subgroup.finish().context("failed to finish subgroup")?;
                    pace(params).await;
                }
            }
        }
        Forwarding::TwoSubgroupsPerGroup => {
            let mut subgroups = track.subgroups().context("failed to enter subgroup mode")?;
            for group in params.groups() {
                let mut even = subgroups
                    .create(Subgroup {
                        group_id: group,
                        subgroup_id: 0,
                        priority: PRIORITY,
                    })
                    .context("failed to create subgroup 0")?;
                let mut odd = subgroups
                    .create(Subgroup {
                        group_id: group,
                        subgroup_id: 1,
                        priority: PRIORITY,
                    })
                    .context("failed to create subgroup 1")?;
                for object in params.objects() {
                    let subgroup = if (object - params.start_object) % 2 == 0 {
                        &mut even
                    } else {
                        &mut odd
                    };
                    write_object(params, subgroup, object, &mut prng)?;
                    pace(params).await;
                }
                even.finish().context("failed to finish subgroup 0")?;
                odd.finish().context("failed to finish subgroup 1")?;
            }
        }
        Forwarding::Datagram => {
            let mut datagrams = track.datagrams().context("failed to enter datagram mode")?;
            for group in params.groups() {
                for object in params.objects() {
                    let datagram = if params.eog_markers && object == params.last_object {
                        // Status datagram: the EndOfGroup status conveys the
                        // marker; the end-of-group type bit is only valid on
                        // payload datagrams.
                        Datagram {
                            group_id: group,
                            object_id: object,
                            priority: PRIORITY,
                            status: ObjectStatus::EndOfGroup,
                            end_of_group: false,
                            payload: Bytes::new(),
                            extension_headers: ExtensionHeaders::new(),
                        }
                    } else {
                        Datagram {
                            group_id: group,
                            object_id: object,
                            priority: PRIORITY,
                            status: ObjectStatus::NormalObject,
                            end_of_group: false,
                            payload: object_payload(params, object),
                            extension_headers: object_extensions(params, &mut prng)
                                .unwrap_or_default(),
                        }
                    };
                    datagrams
                        .write(datagram)
                        .context("failed to write datagram")?;
                    pace(params).await;
                }
            }
        }
    }
    Ok(())
}

/// Write every object of one group to a single subgroup (mode 0).
async fn write_group_objects(
    params: &MoqTestParams,
    subgroup: &mut moq_transport::serve::SubgroupWriter,
    prng: &mut Prng,
) -> Result<()> {
    for object in params.objects() {
        write_object(params, subgroup, object, prng)?;
        pace(params).await;
    }
    Ok(())
}

/// Write one object (data or EOG marker) to a subgroup.
fn write_object(
    params: &MoqTestParams,
    subgroup: &mut moq_transport::serve::SubgroupWriter,
    object: u64,
    prng: &mut Prng,
) -> Result<()> {
    if params.eog_markers && object == params.last_object {
        let marker = subgroup
            .create_with_id_and_status(object, 0, ObjectStatus::EndOfGroup, None)
            .context("failed to create EOG marker")?;
        drop(marker);
        return Ok(());
    }

    let payload = object_payload(params, object);
    let mut writer = subgroup
        .create_with_id(object, payload.len(), object_extensions(params, prng))
        .context("failed to create object")?;
    writer.write(payload).context("failed to write object")?;
    Ok(())
}

async fn pace(params: &MoqTestParams) {
    if params.frequency_ms > 0 {
        sleep(Duration::from_millis(params.frequency_ms)).await;
    }
}

/// What the verifier observed, in a form suitable for TAP diagnostics.
#[derive(Debug, Default)]
pub struct Report {
    pub failures: Vec<String>,
    pub data_objects_received: u64,
    pub data_objects_expected: u64,
    pub eog_markers_received: u64,
    pub eog_markers_expected: u64,
    pub streams_received: u64,
    pub publish_done: Option<String>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// One-line summary for TAP diagnostics.
    pub fn summary(&self) -> String {
        if self.passed() {
            format!(
                "objects {}/{}, eog {}/{}, streams {}, publish_done {}",
                self.data_objects_received,
                self.data_objects_expected,
                self.eog_markers_received,
                self.eog_markers_expected,
                self.streams_received,
                self.publish_done.as_deref().unwrap_or("none"),
            )
        } else {
            format!(
                "{} failure(s); first: {}",
                self.failures.len(),
                self.failures[0]
            )
        }
    }
}

/// The expected object set derived from the tuple, checked off as objects
/// arrive. Any deviation is recorded as a failure string immediately.
struct Scoreboard {
    params: MoqTestParams,
    expected_data: HashSet<(u64, u64)>,
    expected_eog: HashSet<(u64, u64)>,
    received: HashSet<(u64, u64)>,
    received_eog: HashSet<(u64, u64)>,
    last_object_per_stream: HashMap<(u64, u64), u64>,
    failures: Vec<String>,
    data_received: u64,
    eog_received: u64,
}

impl Scoreboard {
    fn new(params: MoqTestParams) -> Self {
        let mut expected_data = HashSet::new();
        let mut expected_eog = HashSet::new();
        for group in params.groups() {
            for object in params.data_objects() {
                expected_data.insert((group, object));
            }
            if params.eog_markers {
                expected_eog.insert((group, params.last_object));
            }
        }
        Self {
            params,
            expected_data,
            expected_eog,
            received: HashSet::new(),
            received_eog: HashSet::new(),
            last_object_per_stream: HashMap::new(),
            failures: Vec::new(),
            data_received: 0,
            eog_received: 0,
        }
    }

    fn fail(&mut self, message: String) {
        const MAX_FAILURES: usize = 50;
        if self.failures.len() < MAX_FAILURES {
            self.failures.push(message);
        }
    }

    fn check_subgroup(&mut self, group: u64, object: u64, subgroup: Option<u64>) {
        let Some(subgroup) = subgroup else {
            return; // datagram mode: no subgroup mapping to check
        };
        match self.params.subgroup_for(object) {
            Some(expected) if expected != subgroup => self.fail(format!(
                "object ({group}, {object}) arrived on subgroup {subgroup}, expected {expected}"
            )),
            None => self.fail(format!(
                "object ({group}, {object}) arrived on subgroup {subgroup} in datagram mode"
            )),
            _ => {}
        }

        // Per-stream ordering only; relays may reorder streams freely.
        let key = (group, subgroup);
        if let Some(last) = self.last_object_per_stream.insert(key, object) {
            if object <= last {
                self.fail(format!(
                    "object ({group}, {object}) arrived out of order within subgroup {subgroup} (after {last})"
                ));
            }
        }
    }

    fn check_extensions(&mut self, group: u64, object: u64, extensions: &ExtensionHeaders) {
        let mut expected = 0;
        if let Some(id) = self.params.int_extension_id() {
            expected += 1;
            if !extensions.has(id) {
                self.fail(format!(
                    "object ({group}, {object}) missing integer extension {id}"
                ));
            }
        }
        if let Some(id) = self.params.var_extension_id() {
            expected += 1;
            if !extensions.has(id) {
                self.fail(format!(
                    "object ({group}, {object}) missing variable extension {id}"
                ));
            }
        }
        // Exactly the configured set, no more, no less.
        if extensions.0.len() != expected {
            self.fail(format!(
                "object ({group}, {object}) carried {} extensions, expected {expected}",
                extensions.0.len()
            ));
        }
    }

    fn record_data(
        &mut self,
        group: u64,
        object: u64,
        subgroup: Option<u64>,
        payload: &[u8],
        extensions: &ExtensionHeaders,
    ) {
        if self.expected_eog.contains(&(group, object)) {
            self.fail(format!(
                "data object at ({group}, {object}) where an EOG marker was expected"
            ));
            return;
        }
        if !self.expected_data.contains(&(group, object)) {
            self.fail(format!("unexpected object ({group}, {object})"));
            return;
        }
        if !self.received.insert((group, object)) {
            self.fail(format!("duplicate object ({group}, {object})"));
            return;
        }
        self.data_received += 1;

        let expected_size = self.params.object_size(object) as usize;
        if payload.len() != expected_size {
            self.fail(format!(
                "object ({group}, {object}) size {} != expected {expected_size}",
                payload.len()
            ));
        } else if !payload.iter().all(|b| *b == b't') {
            self.fail(format!("object ({group}, {object}) payload corrupted"));
        }

        self.check_subgroup(group, object, subgroup);
        self.check_extensions(group, object, extensions);
    }

    /// Record a status object. Only `EndOfGroup` is an EOG marker: the
    /// generator never emits any other status, so a relay that injected
    /// e.g. `EndOfTrack` at the EOG slot must fail verification rather
    /// than silently pass.
    fn record_eog(&mut self, status: ObjectStatus, group: u64, object: u64, subgroup: Option<u64>) {
        if status != ObjectStatus::EndOfGroup {
            self.fail(format!(
                "unexpected status {status:?} at ({group}, {object}), expected EndOfGroup"
            ));
            return;
        }
        if !self.params.eog_markers {
            self.fail(format!("unexpected EOG marker at ({group}, {object})"));
            return;
        }
        if !self.expected_eog.contains(&(group, object)) {
            self.fail(format!(
                "EOG marker at ({group}, {object}), expected only at object {}",
                self.params.last_object
            ));
            return;
        }
        if !self.received_eog.insert((group, object)) {
            self.fail(format!("duplicate EOG marker at ({group}, {object})"));
            return;
        }
        self.eog_received += 1;
        self.check_subgroup(group, object, subgroup);
    }

    fn finish(mut self, publish_done: Option<&PublishDoneInfo>, forward_zero: bool) -> Report {
        let mut report = Report {
            data_objects_expected: if forward_zero {
                0
            } else {
                self.expected_data.len() as u64
            },
            data_objects_received: self.data_received,
            eog_markers_expected: if forward_zero {
                0
            } else {
                self.expected_eog.len() as u64
            },
            eog_markers_received: self.eog_received,
            failures: std::mem::take(&mut self.failures),
            ..Report::default()
        };

        if !forward_zero {
            let missing_count = self.expected_data.difference(&self.received).count();
            let mut missing: Vec<_> = self
                .expected_data
                .difference(&self.received)
                .map(|(g, o)| format!("({g}, {o})"))
                .collect();
            missing.sort();
            missing.truncate(5);
            if missing_count > 0 {
                report.failures.push(format!(
                    "missing {missing_count} objects (first 5): {}",
                    missing.join(", ")
                ));
            }
            for (group, object) in self.expected_eog.difference(&self.received_eog) {
                report
                    .failures
                    .push(format!("missing EOG marker at ({group}, {object})"));
            }
        }

        match publish_done {
            Some(done) => {
                report.publish_done = Some(format!(
                    "status={} stream_count={} reason={:?}",
                    done.status_code, done.stream_count, done.reason
                ));
                let expected_status = PublishDoneCode::TrackEnded as u64;
                if done.status_code != expected_status {
                    report.failures.push(format!(
                        "PUBLISH_DONE status {} != TRACK_ENDED ({expected_status})",
                        done.status_code
                    ));
                }
                let expected_streams = if forward_zero {
                    0
                } else {
                    self.params.expected_stream_count()
                };
                if done.stream_count != expected_streams {
                    report.failures.push(format!(
                        "PUBLISH_DONE stream count {} != expected {expected_streams}",
                        done.stream_count
                    ));
                }
            }
            None => report
                .failures
                .push("PUBLISH_DONE never arrived".to_string()),
        }

        report
    }
}

/// Verify a subscription against the scoreboard derived from the tuple.
///
/// Consumes the track until the publisher ends it, then checks the object
/// set, structure, and the PUBLISH_DONE terminal signal.
pub async fn verify(
    params: MoqTestParams,
    track: TrackReader,
    subscribe: Subscribe,
    forward_zero: bool,
) -> Result<Report> {
    let mut scoreboard = Scoreboard::new(params.clone());

    if forward_zero {
        // FORWARD=0: no data plane at all. Setup completing, PUBLISH_DONE
        // with stream count 0, and zero objects is the pass condition.
        match subscribe.closed().await {
            Err(err) if is_normal_subscription_end(&err) => {}
            Err(err) => bail!("subscription ended abnormally: {err}"),
            Ok(()) => {}
        }
        let mut report = scoreboard.finish(subscribe.publish_done().as_ref(), true);
        // If the relay forwarded data anyway, the session would already have
        // resolved the track mode; a resolved mode here is a failure. A
        // no-data track leaves mode() pending or closed.
        if let Ok(Ok(_mode)) = tokio::time::timeout(Duration::from_millis(10), track.mode()).await {
            report
                .failures
                .push("FORWARD=0 but the relay forwarded data".to_string());
        }
        return Ok(report);
    }

    let mode = track.mode().await.context("track mode never resolved")?;
    let mut streams_received = 0u64;

    match mode {
        TrackReaderMode::Subgroups(mut subgroups) => loop {
            match subgroups.next().await {
                Ok(Some(mut subgroup)) => {
                    streams_received += 1;
                    let group = subgroup.info.group_id;
                    let subgroup_id = subgroup.info.subgroup_id;
                    loop {
                        match subgroup.next().await {
                            Ok(Some(mut object)) => {
                                let object_id = object.info.object_id;
                                if object.info.status == ObjectStatus::NormalObject {
                                    let payload =
                                        object.read_all().await.context("object read failed")?;
                                    scoreboard.record_data(
                                        group,
                                        object_id,
                                        Some(subgroup_id),
                                        &payload,
                                        &object.info.extension_headers,
                                    );
                                } else {
                                    scoreboard.record_eog(
                                        object.info.status,
                                        group,
                                        object_id,
                                        Some(subgroup_id),
                                    );
                                }
                            }
                            Ok(None) => break,
                            Err(err) if is_normal_subscription_end(&err) => break,
                            Err(err) => bail!("subgroup read failed: {err}"),
                        }
                    }
                }
                Ok(None) => break,
                Err(err) if is_normal_subscription_end(&err) => break,
                Err(err) => bail!("subgroup stream failed: {err}"),
            }
        },
        TrackReaderMode::Datagrams(mut datagrams) => loop {
            match datagrams.read().await {
                Ok(Some(datagram)) => {
                    if datagram.status == ObjectStatus::NormalObject {
                        scoreboard.record_data(
                            datagram.group_id,
                            datagram.object_id,
                            None,
                            &datagram.payload,
                            &datagram.extension_headers,
                        );
                    } else {
                        scoreboard.record_eog(
                            datagram.status,
                            datagram.group_id,
                            datagram.object_id,
                            None,
                        );
                    }
                }
                Ok(None) => break,
                Err(err) if is_normal_subscription_end(&err) => break,
                Err(err) => bail!("datagram read failed: {err}"),
            }
        },
        _ => bail!("unexpected track mode (expected subgroups or datagrams)"),
    }

    // Wait for the terminal signal; the data plane draining first is normal.
    match subscribe.closed().await {
        Err(err) if is_normal_subscription_end(&err) => {}
        Err(err) => bail!("subscription ended abnormally: {err}"),
        Ok(()) => {}
    }

    let mut report = scoreboard.finish(subscribe.publish_done().as_ref(), false);
    report.streams_received = streams_received;
    Ok(report)
}

/// A named moq-test scenario: a tuple plus choreography flags.
pub struct Scenario {
    pub params: MoqTestParams,
    /// Run the SUBSCRIBE with FORWARD=0 (expect setup + PUBLISH_DONE, no data).
    pub forward_zero: bool,
}

/// Base tuple for the built-in scenarios: subgroup-per-group, group 0 / object
/// 0 starts, unit increments, 2ms pacing, no EOG, no extensions. Scenarios
/// override fields with struct-update syntax and are validated once when the
/// built-in table is first used.
fn base() -> MoqTestParams {
    MoqTestParams {
        forwarding: Forwarding::SubgroupPerGroup,
        start_group: 0,
        start_object: 0,
        last_group: 0,
        last_object: 0,
        objects_per_group: 1,
        size_object_zero: 64,
        size_object_rest: 32,
        frequency_ms: 2,
        group_increment: 1,
        object_increment: 1,
        eog_markers: false,
        int_extension: None,
        var_extension: None,
        delivery_timeout_ms: None,
    }
}

/// The built-in scenarios. Small and finite: every run is well under a
/// second of object pacing plus round-trips. Validated at lookup (see
/// [`scenario`]) and by the all-scenarios unit test.
static SCENARIOS: LazyLock<Vec<(&'static str, Scenario)>> = LazyLock::new(|| {
    vec![
        (
            "moq-test-subgroup-per-group",
            Scenario {
                params: MoqTestParams {
                    last_group: 2,
                    last_object: 4,
                    objects_per_group: 5,
                    ..base()
                },
                forward_zero: false,
            },
        ),
        (
            "moq-test-subgroup-per-group-eog",
            Scenario {
                params: MoqTestParams {
                    last_group: 2,
                    last_object: 5,
                    objects_per_group: 5,
                    eog_markers: true,
                    ..base()
                },
                forward_zero: false,
            },
        ),
        (
            "moq-test-subgroup-per-object",
            Scenario {
                params: MoqTestParams {
                    forwarding: Forwarding::SubgroupPerObject,
                    last_group: 1,
                    last_object: 3,
                    objects_per_group: 4,
                    ..base()
                },
                forward_zero: false,
            },
        ),
        (
            "moq-test-two-subgroups-eog",
            Scenario {
                params: MoqTestParams {
                    forwarding: Forwarding::TwoSubgroupsPerGroup,
                    last_group: 2,
                    last_object: 6,
                    objects_per_group: 6,
                    eog_markers: true,
                    ..base()
                },
                forward_zero: false,
            },
        ),
        (
            "moq-test-datagram",
            Scenario {
                // Datagram delivery through the serve API is latest-value,
                // not a queue: a receiver that falls behind loses objects.
                // 20ms pacing keeps every hop's reader ahead of the writer.
                params: MoqTestParams {
                    forwarding: Forwarding::Datagram,
                    last_group: 2,
                    last_object: 4,
                    objects_per_group: 5,
                    frequency_ms: 20,
                    ..base()
                },
                forward_zero: false,
            },
        ),
        (
            "moq-test-datagram-eog",
            Scenario {
                params: MoqTestParams {
                    forwarding: Forwarding::Datagram,
                    last_group: 2,
                    last_object: 5,
                    objects_per_group: 5,
                    frequency_ms: 20,
                    eog_markers: true,
                    ..base()
                },
                forward_zero: false,
            },
        ),
        (
            "moq-test-extensions",
            Scenario {
                params: MoqTestParams {
                    last_group: 2,
                    last_object: 5,
                    objects_per_group: 5,
                    eog_markers: true,
                    int_extension: Some(1),
                    var_extension: Some(2),
                    ..base()
                },
                forward_zero: false,
            },
        ),
        (
            "moq-test-increments",
            Scenario {
                params: MoqTestParams {
                    start_group: 10,
                    start_object: 3,
                    last_group: 24,
                    last_object: 7,
                    objects_per_group: 3,
                    size_object_zero: 40,
                    size_object_rest: 24,
                    group_increment: 7,
                    object_increment: 2,
                    ..base()
                },
                forward_zero: false,
            },
        ),
        (
            "moq-test-forward-zero",
            Scenario {
                params: MoqTestParams {
                    last_group: 1,
                    last_object: 2,
                    objects_per_group: 3,
                    ..base()
                },
                forward_zero: true,
            },
        ),
    ]
});

/// Look up a built-in scenario by name, validating its parameters first.
/// An invalid built-in entry is a programmer error, but it is surfaced as an
/// ordinary error rather than a panic (the all-scenarios unit test fails
/// first); `Ok(None)` means no scenario is registered under `name`.
pub fn scenario(name: &str) -> Result<Option<&'static Scenario>> {
    let Some((_, scenario)) = SCENARIOS.iter().find(|(n, _)| *n == name) else {
        return Ok(None);
    };
    scenario
        .params
        .validate()
        .with_context(|| format!("built-in scenario {name} is invalid"))?;
    Ok(Some(scenario))
}

async fn connect(
    args: &Args,
) -> Result<(
    web_transport::Session,
    String,
    moq_transport::session::Transport,
)> {
    let tls = args.tls.load()?;
    let quic = moq_native_ietf::quic::Endpoint::new(moq_native_ietf::quic::Config::new(
        args.bind, None, tls,
    )?)?;
    let (session, connection_id, transport) = quic.client.connect(&args.relay, None).await?;
    Ok((session, connection_id, transport))
}

/// Run a moq-test scenario with the subscribe-first choreography and return
/// the verification report.
pub async fn run(args: &Args, scenario: &Scenario) -> Result<(TestConnectionIds, Report)> {
    let mut cids = TestConnectionIds::default();
    let namespace = scenario.params.to_namespace();
    let track_name = run_track_name();

    // 1. Subscriber session: subscribe first; the relay holds the
    //    subscription pending (RENDEZVOUS_TIMEOUT) until the publisher
    //    appears.
    let (sub_session, sub_cid, sub_transport) = connect(args)
        .await
        .context("subscriber failed to connect")?;
    cids.add(sub_cid);
    let (sub_session, _sub_publisher, mut subscriber) =
        Session::connect(sub_session, None, sub_transport)
            .await
            .context("subscriber SETUP failed")?;

    let (sub_writer, sub_reader) = Track::new(namespace.clone(), track_name.clone()).produce();

    let mut sub_params = KeyValuePairs::default();
    sub_params.set_rendezvous_timeout(RENDEZVOUS_TIMEOUT_MS);
    if scenario.forward_zero {
        sub_params.set_forward(false);
    }

    // Datagrams are fire-and-forget: objects the publisher sends before the
    // relay has bound the subscription have nowhere to go and are silently
    // dropped (the serve API's datagram delivery is latest-value, not a
    // queue). The publisher therefore waits until the subscriber's SUBSCRIBE
    // has been answered — at that point the relay has bound the pair and
    // forwarding is live. The channel also fails the publisher promptly if
    // the subscriber's setup errors.
    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel::<()>();

    let params = scenario.params.clone();
    let forward_zero = scenario.forward_zero;
    let sub_handle = tokio::spawn(async move {
        let work = async move {
            let subscribe = subscriber
                .subscribe_open_with_params(sub_writer, sub_params)
                .await
                .context("SUBSCRIBE failed")?;
            let _ = setup_tx.send(());
            verify(params, sub_reader, subscribe, forward_zero).await
        };
        tokio::select! {
            res = work => res,
            res = sub_session.run() => {
                res.context("subscriber session error")?;
                Err(anyhow!("subscriber session ended before verification completed"))
            }
        }
    });

    // Give the subscriber a head start so the SUBSCRIBE genuinely arrives
    // first at the relay. This is a bias, not a guarantee: if the publisher
    // wins the race under load the relay simply runs a publish-first
    // choreography, which passes identically for the data scenarios (the
    // scoreboard is order-agnostic across the two connection roles). The one
    // scenario where ordering is semantic — FORWARD=0 — fails loudly on
    // inversion, because the relay then grants forward=1 and data flows.
    sleep(Duration::from_millis(200)).await;

    // 2. Publisher session: direct PUBLISH of the same track, then generate.
    let (pub_session, pub_cid, pub_transport) =
        connect(args).await.context("publisher failed to connect")?;
    cids.add(pub_cid);
    let (pub_session, mut publisher, _pub_subscriber) =
        Session::connect(pub_session, None, pub_transport)
            .await
            .context("publisher SETUP failed")?;

    let (pub_writer, pub_reader) = Track::new(namespace.clone(), track_name.clone()).produce();

    let mut pub_params = KeyValuePairs::default();
    if let Some(timeout_ms) = scenario.params.delivery_timeout_ms {
        pub_params.set_delivery_timeout(timeout_ms);
    }

    let publish = async move {
        let mut published = publisher
            .publish(pub_reader, pub_params)
            .await
            .context("failed to send PUBLISH")?;
        published.ok().await.context("PUBLISH was rejected")?;
        setup_rx
            .await
            .map_err(|_| anyhow!("subscriber setup failed before generation started"))?;
        tracing::info!(
            objects = scenario.params.expected_data_objects(),
            streams = scenario.params.expected_stream_count(),
            "PUBLISH accepted and subscription bound; generating objects"
        );
        // Generation and serving must run concurrently: the session reads
        // datagrams from the track through the serve API's latest-value
        // reader, so anything written before `serve` starts is coalesced
        // away. (Subgroup streams buffer and would survive sequential
        // ordering, but datagrams do not.)
        let params = &scenario.params;
        let generate = async move { generate(params, pub_writer).await };
        let serve = async move { published.serve().await };
        let (gen_result, serve_result) = tokio::join!(generate, serve);
        gen_result?;
        serve_result.context("failed serving PUBLISH track")?;
        tracing::info!("PUBLISH completed");
        Ok::<_, anyhow::Error>(())
    };

    tokio::select! {
        res = publish => res?,
        res = pub_session.run() => {
            res.context("publisher session error")?;
            bail!("publisher session ended before generation completed");
        }
    }

    let report = sub_handle
        .await
        .context("subscriber task panicked")?
        .context("subscriber verification failed")?;

    Ok((cids, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(fields: &[&str]) -> Result<MoqTestParams> {
        let fields: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        MoqTestParams::from_namespace_fields(&fields)
    }

    #[test]
    fn round_trip_explicit_tuple() {
        let p = MoqTestParams {
            forwarding: Forwarding::TwoSubgroupsPerGroup,
            last_group: 2,
            last_object: 6,
            objects_per_group: 6,
            eog_markers: true,
            int_extension: Some(1),
            var_extension: Some(2),
            ..base()
        };
        let fields = p.to_namespace_fields();
        assert_eq!(fields.len(), NUM_TUPLE_FIELDS);
        let parsed = MoqTestParams::from_namespace_fields(&fields).unwrap();
        assert_eq!(parsed, p);
    }

    #[test]
    fn blank_fields_select_defaults() {
        // Only fields 4 (last group) and 5 (last object) are explicit.
        let p = parse(&[
            "moq-test-00",
            "",
            "",
            "",
            "3",
            "4",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ])
        .unwrap();
        assert_eq!(p.forwarding, Forwarding::SubgroupPerGroup);
        assert_eq!(p.start_group, 0);
        assert_eq!(p.start_object, 0);
        assert_eq!(p.last_group, 3);
        assert_eq!(p.last_object, 4);
        assert_eq!(p.objects_per_group, 10); // default: cap check only (max 9 >= 4)
        assert_eq!(p.int_extension, None);

        // Field 5 blank: defaults to the computed maximum.
        let p = parse(&[
            "moq-test-00",
            "",
            "",
            "",
            "3",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ])
        .unwrap();
        assert_eq!(p.last_object, 9); // 0 + (10-1)*1
    }

    #[test]
    fn rejects_wrong_field_count_and_tag() {
        assert!(parse(&["moq-test-00", "0"]).is_err());
        let mut fields = vec!["wrong-00".to_string()];
        fields.extend((0..15).map(|_| "0".to_string()));
        assert!(MoqTestParams::from_namespace_fields(&fields).is_err());
    }

    #[test]
    fn rejects_unbounded_default_last_group() {
        // A blank field 4 defaults last_group to (1<<62)-1; validation must
        // reject the tuple before the scoreboard materializes the series.
        let fields: Vec<String> = "moq-test-00/1/0/0//4/10/64/32/100/1/1/0///"
            .split('/')
            .map(str::to_string)
            .collect();
        assert_eq!(
            MoqTestParams::from_namespace_fields(&fields)
                .unwrap_err()
                .to_string(),
            "group series of 4611686018427387904 groups exceeds the 100000 group limit"
        );

        // An explicit last_group at u64::MAX must be rejected by the same
        // bound, not wrap the count arithmetic.
        let fields: Vec<String> = "moq-test-00/1/0/0/18446744073709551615/0/1/64/32/100/1/1/0///"
            .split('/')
            .map(str::to_string)
            .collect();
        assert_eq!(
            MoqTestParams::from_namespace_fields(&fields)
                .unwrap_err()
                .to_string(),
            "group series of 18446744073709551616 groups exceeds the 100000 group limit"
        );
    }

    #[test]
    fn rejects_excessive_total_object_count() {
        // 1000 groups of 1100 objects = 1.1M objects, over the cap.
        let params = MoqTestParams {
            last_group: 999,
            last_object: 1099,
            objects_per_group: 1100,
            ..base()
        };
        assert!(params.validate().is_err());

        // 999 groups of 1000 objects = 999k objects, just under the cap.
        let params = MoqTestParams {
            last_group: 998,
            last_object: 999,
            objects_per_group: 1000,
            ..base()
        };
        assert!(params.validate().is_ok());
    }

    #[test]
    fn object_series_and_eog_slot() {
        // 3 groups (0,1,2), objects 0..=5, EOG at 5: data objects 0..=4.
        let p = MoqTestParams {
            last_group: 2,
            last_object: 5,
            objects_per_group: 5,
            eog_markers: true,
            ..base()
        };
        assert_eq!(p.groups(), vec![0, 1, 2]);
        assert_eq!(p.objects(), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(p.data_objects(), vec![0, 1, 2, 3, 4]);
        assert_eq!(p.expected_data_objects(), 15);
        assert_eq!(p.expected_stream_count(), 3);
    }

    #[test]
    fn increments_scenario_math() {
        let p = MoqTestParams {
            start_group: 10,
            start_object: 3,
            last_group: 24,
            last_object: 7,
            objects_per_group: 3,
            size_object_zero: 40,
            size_object_rest: 24,
            group_increment: 7,
            object_increment: 2,
            ..base()
        };
        p.validate().unwrap();
        assert_eq!(p.groups(), vec![10, 17, 24]);
        assert_eq!(p.objects(), vec![3, 5, 7]);
        assert_eq!(p.expected_data_objects(), 9);
        assert_eq!(p.object_size(3), 40);
        assert_eq!(p.object_size(5), 24);
    }

    #[test]
    fn field5_cap_offsets_by_start_object() {
        // The naive formula (objects_per_group + eog) * object_increment
        // assumes start_object == 0; ours offsets. 3 objects starting at 3
        // with increment 2: last valid slot is 3 + 2*2 = 7.
        let p = MoqTestParams {
            start_object: 3,
            last_object: 7,
            objects_per_group: 3,
            object_increment: 2,
            ..base()
        };
        p.validate().unwrap();

        // 8 would require a fourth object slot: invalid.
        let mut bad = p.clone();
        bad.last_object = 8;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn stream_counts_by_mode() {
        assert_eq!(
            MoqTestParams {
                last_group: 2,
                last_object: 4,
                objects_per_group: 5,
                ..base()
            }
            .expected_stream_count(),
            3
        );
        assert_eq!(
            MoqTestParams {
                forwarding: Forwarding::SubgroupPerObject,
                last_group: 1,
                last_object: 3,
                objects_per_group: 4,
                ..base()
            }
            .expected_stream_count(),
            8
        );
        assert_eq!(
            MoqTestParams {
                forwarding: Forwarding::TwoSubgroupsPerGroup,
                last_group: 2,
                last_object: 6,
                objects_per_group: 6,
                eog_markers: true,
                ..base()
            }
            .expected_stream_count(),
            6
        );
        assert_eq!(
            MoqTestParams {
                forwarding: Forwarding::Datagram,
                last_group: 2,
                last_object: 4,
                objects_per_group: 5,
                ..base()
            }
            .expected_stream_count(),
            0
        );
    }

    #[test]
    fn extension_ids() {
        let p = MoqTestParams {
            int_extension: Some(1),
            var_extension: Some(2),
            ..base()
        };
        assert_eq!(p.int_extension_id(), Some(2));
        assert_eq!(p.var_extension_id(), Some(5));
    }

    #[test]
    fn datagram_size_budget() {
        assert!(MoqTestParams {
            forwarding: Forwarding::Datagram,
            last_object: 4,
            objects_per_group: 5,
            ..base()
        }
        .validate()
        .is_ok());
        assert!(MoqTestParams {
            forwarding: Forwarding::Datagram,
            last_object: 4,
            objects_per_group: 5,
            size_object_zero: 2000,
            ..base()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn subgroup_size_bound() {
        // Subgroup modes stream over QUIC, so the tight datagram budget
        // does not apply — but payloads are still materialized eagerly, so
        // an absurd operator-supplied size is a validation error rather
        // than an allocator failure.
        assert!(MoqTestParams {
            last_object: 4,
            objects_per_group: 5,
            size_object_rest: MAX_OBJECT_SIZE,
            ..base()
        }
        .validate()
        .is_ok());
        assert!(MoqTestParams {
            last_object: 4,
            objects_per_group: 5,
            size_object_zero: u64::MAX,
            ..base()
        }
        .validate()
        .is_err());
    }

    fn full_scoreboard() -> Scoreboard {
        // One group, objects 0,1 data + EOG at 2.
        let p = MoqTestParams {
            last_object: 2,
            objects_per_group: 2,
            size_object_zero: 4,
            size_object_rest: 2,
            eog_markers: true,
            ..base()
        };
        Scoreboard::new(p)
    }

    fn done(status_code: u64, stream_count: u64) -> PublishDoneInfo {
        PublishDoneInfo {
            status_code,
            stream_count,
            reason: moq_transport::coding::ReasonPhrase("publish ended".to_string()),
        }
    }

    #[test]
    fn scoreboard_passes_on_exact_delivery() {
        let mut sb = full_scoreboard();
        // Group 0: data at 0,1; EOG at 2.
        sb.record_data(0, 0, Some(0), b"tttt", &ExtensionHeaders::new());
        sb.record_data(0, 1, Some(0), b"tt", &ExtensionHeaders::new());
        sb.record_eog(ObjectStatus::EndOfGroup, 0, 2, Some(0));
        let report = sb.finish(Some(&done(PublishDoneCode::TrackEnded as u64, 1)), false);
        assert!(report.passed(), "failures: {:?}", report.failures);
    }

    #[test]
    fn scoreboard_catches_missing_object() {
        let mut sb = full_scoreboard();
        sb.record_data(0, 0, Some(0), b"tttt", &ExtensionHeaders::new());
        sb.record_eog(ObjectStatus::EndOfGroup, 0, 2, Some(0));
        let report = sb.finish(Some(&done(PublishDoneCode::TrackEnded as u64, 1)), false);
        assert!(!report.passed());
        assert!(report.failures.iter().any(|f| f.contains("missing")));
    }

    #[test]
    fn scoreboard_rejects_non_eog_status() {
        // A relay that injects e.g. EndOfTrack at the EOG slot must not
        // pass as an EOG marker.
        let mut sb = full_scoreboard();
        sb.record_data(0, 0, Some(0), b"tttt", &ExtensionHeaders::new());
        sb.record_data(0, 1, Some(0), b"tt", &ExtensionHeaders::new());
        sb.record_eog(ObjectStatus::EndOfTrack, 0, 2, Some(0));
        let report = sb.finish(Some(&done(PublishDoneCode::TrackEnded as u64, 1)), false);
        assert!(!report.passed());
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.contains("unexpected status")),
            "failures: {:?}",
            report.failures
        );
    }

    #[test]
    fn scoreboard_catches_extra_and_duplicate() {
        let mut sb = full_scoreboard();
        sb.record_data(0, 0, Some(0), b"tttt", &ExtensionHeaders::new());
        sb.record_data(0, 1, Some(0), b"tt", &ExtensionHeaders::new());
        sb.record_eog(ObjectStatus::EndOfGroup, 0, 2, Some(0));
        sb.record_data(0, 1, Some(0), b"tt", &ExtensionHeaders::new()); // duplicate
        sb.record_data(0, 9, Some(0), b"tt", &ExtensionHeaders::new()); // unexpected
        let report = sb.finish(Some(&done(PublishDoneCode::TrackEnded as u64, 1)), false);
        assert!(report.failures.iter().any(|f| f.contains("duplicate")));
        assert!(report.failures.iter().any(|f| f.contains("unexpected")));
    }

    #[test]
    fn scoreboard_catches_corruption_and_size() {
        let mut sb = full_scoreboard();
        sb.record_data(0, 0, Some(0), b"txtt", &ExtensionHeaders::new());
        sb.record_data(0, 1, Some(0), b"t", &ExtensionHeaders::new());
        sb.record_eog(ObjectStatus::EndOfGroup, 0, 2, Some(0));
        let report = sb.finish(Some(&done(PublishDoneCode::TrackEnded as u64, 1)), false);
        assert!(report.failures.iter().any(|f| f.contains("size")));
        assert!(report.failures.iter().any(|f| f.contains("corrupted")));
    }

    #[test]
    fn subgroup_mapping_is_offset_parity() {
        // Two-subgroup mode maps (object_id - start_object) % 2: with
        // start_object = 3, object 3 lands on subgroup 0 even though its
        // raw ID is odd.
        let p = MoqTestParams {
            forwarding: Forwarding::TwoSubgroupsPerGroup,
            start_object: 3,
            last_object: 6,
            objects_per_group: 4,
            size_object_zero: 2,
            size_object_rest: 2,
            ..base()
        };
        let mut sb = Scoreboard::new(p);
        sb.record_data(0, 3, Some(1), b"tt", &ExtensionHeaders::new()); // raw parity would allow this
        sb.record_data(0, 4, Some(1), b"tt", &ExtensionHeaders::new());
        sb.record_data(0, 5, Some(0), b"tt", &ExtensionHeaders::new());
        sb.record_data(0, 6, Some(1), b"tt", &ExtensionHeaders::new());
        let report = sb.finish(Some(&done(PublishDoneCode::TrackEnded as u64, 2)), false);
        assert!(report
            .failures
            .iter()
            .any(|f| f.contains("arrived on subgroup 1, expected 0")));
    }

    #[test]
    fn scoreboard_catches_wrong_subgroup_and_order() {
        // Objects 0..=3 in one group; sizes: object 0 -> 4 bytes, rest -> 2.
        let p = MoqTestParams {
            forwarding: Forwarding::TwoSubgroupsPerGroup,
            last_object: 3,
            objects_per_group: 4,
            size_object_zero: 4,
            size_object_rest: 2,
            ..base()
        };
        let mut sb = Scoreboard::new(p);
        sb.record_data(0, 0, Some(1), b"tttt", &ExtensionHeaders::new()); // even on odd subgroup
        sb.record_data(0, 2, Some(0), b"tt", &ExtensionHeaders::new());
        sb.record_data(0, 1, Some(1), b"tt", &ExtensionHeaders::new());
        sb.record_data(0, 3, Some(1), b"tt", &ExtensionHeaders::new());
        let report = sb.finish(Some(&done(PublishDoneCode::TrackEnded as u64, 2)), false);
        assert!(report
            .failures
            .iter()
            .any(|f| f.contains("arrived on subgroup 1, expected 0")));

        // Out-of-order within a stream: object 0 after object 2 on subgroup 0.
        let p = MoqTestParams {
            last_object: 3,
            objects_per_group: 4,
            size_object_zero: 4,
            size_object_rest: 2,
            ..base()
        };
        let mut sb = Scoreboard::new(p);
        sb.record_data(0, 2, Some(0), b"tt", &ExtensionHeaders::new());
        sb.record_data(0, 0, Some(0), b"tttt", &ExtensionHeaders::new());
        let report = sb.finish(Some(&done(PublishDoneCode::TrackEnded as u64, 1)), false);
        assert!(report.failures.iter().any(|f| f.contains("out of order")));
    }

    #[test]
    fn scoreboard_checks_publish_done() {
        let mut sb = full_scoreboard();
        sb.record_data(0, 0, Some(0), b"tttt", &ExtensionHeaders::new());
        sb.record_data(0, 1, Some(0), b"tt", &ExtensionHeaders::new());
        sb.record_eog(ObjectStatus::EndOfGroup, 0, 2, Some(0));

        let report = sb.finish(Some(&done(PublishDoneCode::TrackEnded as u64, 7)), false);
        assert!(report.failures.iter().any(|f| f.contains("stream count 7")));

        let sb = full_scoreboard();
        let report = sb.finish(None, false);
        assert!(report.failures.iter().any(|f| f.contains("never arrived")));
    }

    #[test]
    fn scoreboard_forward_zero_expects_nothing() {
        let p = MoqTestParams {
            last_object: 2,
            objects_per_group: 2,
            eog_markers: true,
            ..base()
        };
        let sb = Scoreboard::new(p.clone());
        let report = sb.finish(Some(&done(PublishDoneCode::TrackEnded as u64, 0)), true);
        assert!(report.passed(), "failures: {:?}", report.failures);

        let sb = Scoreboard::new(p);
        let report = sb.finish(Some(&done(PublishDoneCode::TrackEnded as u64, 3)), true);
        assert!(report.failures.iter().any(|f| f.contains("stream count 3")));
    }

    #[test]
    fn all_builtin_scenarios_are_valid() {
        // An invalid built-in entry is a programmer error; fail here rather
        // than at lookup (which surfaces it as an ordinary error, not a
        // panic).
        let invalid: Vec<&str> = SCENARIOS
            .iter()
            .filter(|(_, s)| s.params.validate().is_err())
            .map(|(n, _)| *n)
            .collect();
        assert!(
            invalid.is_empty(),
            "invalid built-in scenarios: {invalid:?}"
        );
    }
}
