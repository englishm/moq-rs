// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Key-Value-Pair (KVP) encoding as defined in draft-ietf-moq-transport-16 §1.4.2.
//!
//! KVPs encode a Type value as a **delta from the previous Type** (or from 0 if
//! there is no previous Type). On the wire:
//!
//! ```text
//! Key-Value-Pair {
//!   Delta Type (i),       -- varint delta from previous absolute type
//!   [Length (i),]         -- only present when absolute Type is odd
//!   Value (..)            -- varint when Type is even, bytes when Type is odd
//! }
//! ```
//!
//! The previous-Type-plus-Delta MUST NOT exceed 2^64-1.  If it would, the
//! session MUST be closed with PROTOCOL_VIOLATION.
//!
//! A sequence of KVPs is prefixed by a varint count of the number of pairs.

use crate::coding::{Decode, DecodeError, Encode, EncodeError};
use bytes::Buf as _;
use std::fmt;

/// Maximum byte-value length for a bytes-typed KVP (2^16 − 1).
const MAX_BYTES_VALUE_LEN: usize = u16::MAX as usize;

/// Smallest possible encoded KVP: a one-byte delta plus a one-byte varint value.
const MIN_KVP_WIRE_LEN: usize = 2;

// ─── Value ────────────────────────────────────────────────────────────────────

#[derive(Clone, Eq, PartialEq)]
pub enum Value {
    IntValue(u64),
    BytesValue(Vec<u8>),
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::IntValue(v) => write!(f, "{}", v),
            Value::BytesValue(bytes) => {
                let preview: Vec<String> = bytes
                    .iter()
                    .take(16)
                    .map(|b| format!("{:02X}", b))
                    .collect();
                write!(f, "[{}]", preview.join(" "))
            }
        }
    }
}

// ─── KeyValuePair ─────────────────────────────────────────────────────────────

/// A single Key-Value-Pair with an absolute (resolved) key.
///
/// The delta encoding is handled by [`KeyValuePairs`]; individual pairs always
/// carry their absolute key so they can be compared and looked up without
/// needing ordering context.
#[derive(Clone, Eq, PartialEq)]
pub struct KeyValuePair {
    pub key: u64,
    pub value: Value,
}

impl KeyValuePair {
    pub fn new(key: u64, value: Value) -> Self {
        Self { key, value }
    }

    pub fn new_int(key: u64, value: u64) -> Self {
        Self {
            key,
            value: Value::IntValue(value),
        }
    }

    pub fn new_bytes(key: u64, value: Vec<u8>) -> Self {
        Self {
            key,
            value: Value::BytesValue(value),
        }
    }

    /// Decode a single KVP from the wire given the previous absolute type.
    ///
    /// Returns `(pair, new_absolute_type)`.
    pub(crate) fn decode_with_prev<R: bytes::Buf>(
        r: &mut R,
        prev: u64,
    ) -> Result<(Self, u64), DecodeError> {
        Self::decode_with_prev_typed(r, prev, &[], &[])
    }

    /// Like [`decode_with_prev`](KeyValuePair::decode_with_prev), but the
    /// absolute types listed in `u8_value_types` carry a single raw `uint8`
    /// value instead of the default varint. Draft-17 (§10.2) replaced the
    /// generic even=varint rule for certain message parameters (e.g.
    /// SUBSCRIBER_PRIORITY, whose implicit default 128 does not fit in one
    /// varint byte) with a fixed one-byte value; see
    /// [`crate::message::U8_VALUE_PARAMETER_TYPES`].
    pub(crate) fn decode_with_prev_typed<R: bytes::Buf>(
        r: &mut R,
        prev: u64,
        u8_value_types: &[u64],
        two_varint_value_types: &[u64],
    ) -> Result<(Self, u64), DecodeError> {
        let delta = u64::decode(r)?;

        // Draft-16 §1.4.2: prev + delta MUST NOT overflow u64.
        let abs_type = prev
            .checked_add(delta)
            .ok_or(DecodeError::KvpTypeOverflow)?;

        let pair = if u8_value_types.contains(&abs_type) {
            // Draft-17+ typed parameter → single raw uint8 value.
            let value = u8::decode(r)?;
            KeyValuePair::new_int(abs_type, value.into())
        } else if two_varint_value_types.contains(&abs_type) {
            let first = u64::decode(r)?;
            let second = u64::decode(r)?;
            let mut value = Vec::new();
            first
                .encode(&mut value)
                .map_err(|_| DecodeError::InvalidParameter)?;
            second
                .encode(&mut value)
                .map_err(|_| DecodeError::InvalidParameter)?;
            KeyValuePair::new_bytes(abs_type, value)
        } else if abs_type % 2 == 0 {
            // Even type → varint value.
            let value = u64::decode(r)?;
            KeyValuePair::new_int(abs_type, value)
        } else {
            // Odd type → length-prefixed bytes value.
            let length = usize::decode(r)?;
            if length > MAX_BYTES_VALUE_LEN {
                return Err(DecodeError::KeyValuePairLengthExceeded());
            }
            <u64 as Decode>::decode_remaining(r, length)?;
            let mut buf = vec![0u8; length];
            r.copy_to_slice(&mut buf);
            KeyValuePair::new_bytes(abs_type, buf)
        };

        Ok((pair, abs_type))
    }

    /// Encode a single KVP onto the wire given the previous absolute type.
    ///
    /// Writes the delta and value; returns the new absolute type (== `self.key`).
    pub(crate) fn encode_with_prev<W: bytes::BufMut>(
        &self,
        w: &mut W,
        prev: u64,
    ) -> Result<u64, EncodeError> {
        self.encode_with_prev_typed(w, prev, &[], &[])
    }

    /// Like [`encode_with_prev`](KeyValuePair::encode_with_prev), but the keys
    /// listed in `u8_value_types` are written as a single raw `uint8` value
    /// instead of the default varint. Values exceeding 255 for such a key are
    /// rejected. See
    /// [`decode_with_prev_typed`](KeyValuePair::decode_with_prev_typed) and
    /// [`crate::message::U8_VALUE_PARAMETER_TYPES`].
    pub(crate) fn encode_with_prev_typed<W: bytes::BufMut>(
        &self,
        w: &mut W,
        prev: u64,
        u8_value_types: &[u64],
        two_varint_value_types: &[u64],
    ) -> Result<u64, EncodeError> {
        // Keys must be consistent with their value parity.
        match &self.value {
            Value::IntValue(_) if !self.key.is_multiple_of(2) => {
                return Err(EncodeError::InvalidValue);
            }
            Value::BytesValue(_) if self.key.is_multiple_of(2) => {
                return Err(EncodeError::InvalidValue);
            }
            _ => {}
        }

        // Delta MUST NOT underflow (keys must be in ascending order within a sequence).
        let delta = self.key.checked_sub(prev).ok_or(EncodeError::KvpKeyOrder)?;
        delta.encode(w)?;

        match &self.value {
            Value::IntValue(v) => {
                if u8_value_types.contains(&self.key) {
                    // Draft-17+ typed parameter → single raw uint8 value.
                    let byte = u8::try_from(*v).map_err(|_| EncodeError::InvalidValue)?;
                    byte.encode(w)?;
                } else {
                    (*v).encode(w)?;
                }
            }
            Value::BytesValue(v) => {
                if two_varint_value_types.contains(&self.key) {
                    let mut value = bytes::Bytes::copy_from_slice(v);
                    let first = u64::decode(&mut value).map_err(|_| EncodeError::InvalidValue)?;
                    let second = u64::decode(&mut value).map_err(|_| EncodeError::InvalidValue)?;
                    if value.has_remaining() {
                        return Err(EncodeError::InvalidValue);
                    }
                    first.encode(w)?;
                    second.encode(w)?;
                    return Ok(self.key);
                }
                if v.len() > MAX_BYTES_VALUE_LEN {
                    return Err(EncodeError::KeyValuePairLengthExceeded);
                }
                v.len().encode(w)?;
                <u64 as Encode>::encode_remaining(w, v.len())?;
                w.put_slice(v);
            }
        }

        Ok(self.key)
    }
}

// Note: `KeyValuePair` intentionally does NOT implement `Decode`/`Encode`
// directly, because a single pair is always decoded/encoded with a `prev`
// context.  Use `decode_with_prev` / `encode_with_prev` (or go through
// `KeyValuePairs` / `ExtensionHeaders`) so the delta state is always correct.

impl fmt::Debug for KeyValuePair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{{}: {:?}}}", self.key, self.value)
    }
}

// ─── KeyValuePairs ────────────────────────────────────────────────────────────

/// An ordered, count-prefixed sequence of [`KeyValuePair`] entries.
///
/// On the wire the keys are delta-encoded from 0, so they MUST be in
/// non-decreasing order.  Internally keys are stored as absolute values.
#[derive(Default, Clone, Eq, PartialEq)]
pub struct KeyValuePairs(pub Vec<KeyValuePair>);

impl KeyValuePairs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace the pair with matching key.
    pub fn set(&mut self, kvp: KeyValuePair) {
        if let Some(existing) = self.0.iter_mut().find(|k| k.key == kvp.key) {
            *existing = kvp;
        } else {
            self.0.push(kvp);
        }
    }

    pub fn set_intvalue(&mut self, key: u64, value: u64) {
        self.set(KeyValuePair::new_int(key, value));
    }

    pub fn set_bytesvalue(&mut self, key: u64, value: Vec<u8>) {
        self.set(KeyValuePair::new_bytes(key, value));
    }

    pub fn has(&self, key: u64) -> bool {
        self.0.iter().any(|k| k.key == key)
    }

    pub fn get(&self, key: u64) -> Option<&KeyValuePair> {
        self.0.iter().find(|k| k.key == key)
    }

    /// Return `true` if any key appears more than once.
    pub fn has_duplicate_keys(&self) -> bool {
        let mut seen = std::collections::HashSet::new();
        self.0.iter().any(|k| !seen.insert(k.key))
    }
}

impl KeyValuePairs {
    /// Decode a count-prefixed KVP sequence, treating the absolute types listed
    /// in `u8_value_types` as single raw `uint8` values (draft-17+ typed message
    /// parameters). Pass `&[]` for the generic draft-16 even=varint behaviour;
    /// the [`Decode`] impl does exactly that.
    pub fn decode_with_u8_types<R: bytes::Buf>(
        r: &mut R,
        u8_value_types: &[u64],
    ) -> Result<Self, DecodeError> {
        Self::decode_with_value_types(r, u8_value_types, &[])
    }

    pub(crate) fn decode_with_value_types<R: bytes::Buf>(
        r: &mut R,
        u8_value_types: &[u64],
        two_varint_value_types: &[u64],
    ) -> Result<Self, DecodeError> {
        let count = u64::decode(r)?;

        // `count` is peer-controlled, so do not allocate directly from it.
        // This is only a capacity hint: with the bytes currently buffered, at
        // most `remaining / MIN_KVP_WIRE_LEN` pairs can be decoded before the
        // normal decode loop asks for more bytes via `DecodeError::More`.
        let count_capacity = usize::try_from(count).unwrap_or(usize::MAX);
        let payload_capacity = r.remaining() / MIN_KVP_WIRE_LEN;
        let mut kvps = Vec::with_capacity(count_capacity.min(payload_capacity));
        let mut prev = 0u64;

        for _ in 0..count {
            let (pair, new_prev) = KeyValuePair::decode_with_prev_typed(
                r,
                prev,
                u8_value_types,
                two_varint_value_types,
            )?;
            prev = new_prev;
            kvps.push(pair);
        }

        Ok(KeyValuePairs(kvps))
    }

    /// Encode a count-prefixed KVP sequence, writing the keys listed in
    /// `u8_value_types` as single raw `uint8` values (draft-17+ typed message
    /// parameters). Pass `&[]` for the generic draft-16 even=varint behaviour;
    /// the [`Encode`] impl does exactly that.
    pub fn encode_with_u8_types<W: bytes::BufMut>(
        &self,
        w: &mut W,
        u8_value_types: &[u64],
    ) -> Result<(), EncodeError> {
        self.encode_with_value_types(w, u8_value_types, &[])
    }

    pub(crate) fn encode_with_value_types<W: bytes::BufMut>(
        &self,
        w: &mut W,
        u8_value_types: &[u64],
        two_varint_value_types: &[u64],
    ) -> Result<(), EncodeError> {
        // Sort a working copy by ascending key before encoding so the delta is
        // always non-negative.  The internal Vec is not required to be sorted.
        let mut sorted: Vec<&KeyValuePair> = self.0.iter().collect();
        sorted.sort_by_key(|k| k.key);

        sorted.len().encode(w)?;

        let mut prev = 0u64;
        for kvp in &sorted {
            prev = kvp.encode_with_prev_typed(w, prev, u8_value_types, two_varint_value_types)?;
        }

        Ok(())
    }
}

impl Decode for KeyValuePairs {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        Self::decode_with_u8_types(r, &[])
    }
}

impl Encode for KeyValuePairs {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.encode_with_u8_types(w, &[])
    }
}

impl fmt::Debug for KeyValuePairs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{ ")?;
        for (i, kv) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{:?}", kv)?;
        }
        write!(f, " }}")
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    // ── single pair helpers ───────────────────────────────────────────────────

    fn round_trip_pair(pairs: &[(u64, Value)]) -> Vec<u8> {
        // Build the wire bytes by hand for a sequence of pairs (delta-encoded).
        // This is the canonical wire format we expect encode/decode to match.
        let mut buf = BytesMut::new();
        let mut prev = 0u64;
        for (key, value) in pairs {
            let delta = key - prev;
            delta.encode(&mut buf).unwrap();
            match value {
                Value::IntValue(v) => v.encode(&mut buf).unwrap(),
                Value::BytesValue(b) => {
                    b.len().encode(&mut buf).unwrap();
                    buf.extend_from_slice(b);
                }
            }
            prev = *key;
        }
        buf.to_vec()
    }

    // ── encode / decode single pair ───────────────────────────────────────────

    #[test]
    fn single_int_pair_roundtrip() {
        // key=0 (even, int), value=42  →  delta=0, value=42
        let mut buf = BytesMut::new();
        let kvps = KeyValuePairs(vec![KeyValuePair::new_int(0, 42)]);
        kvps.encode(&mut buf).unwrap();
        // wire: count=1, delta=0, value=42
        assert_eq!(buf.to_vec(), vec![0x01, 0x00, 0x2a]);
        let decoded = KeyValuePairs::decode(&mut buf).unwrap();
        assert_eq!(decoded, kvps);
    }

    #[test]
    fn single_bytes_pair_roundtrip() {
        // key=1 (odd, bytes), value=[0xAB, 0xCD]
        let mut buf = BytesMut::new();
        let kvps = KeyValuePairs(vec![KeyValuePair::new_bytes(1, vec![0xAB, 0xCD])]);
        kvps.encode(&mut buf).unwrap();
        // wire: count=1, delta=1, length=2, 0xAB, 0xCD
        assert_eq!(buf.to_vec(), vec![0x01, 0x01, 0x02, 0xAB, 0xCD]);
        let decoded = KeyValuePairs::decode(&mut buf).unwrap();
        assert_eq!(decoded, kvps);
    }

    // ── delta encoding ────────────────────────────────────────────────────────

    #[test]
    fn delta_encoding_multiple_pairs() {
        // Three pairs: key=0 (int=1), key=2 (int=2), key=100 (int=3)
        // Deltas on wire: 0, 2, 98
        let mut buf = BytesMut::new();
        let mut kvps = KeyValuePairs::new();
        kvps.set_intvalue(0, 1);
        kvps.set_intvalue(2, 2);
        kvps.set_intvalue(100, 3);
        kvps.encode(&mut buf).unwrap();

        let expected_wire = round_trip_pair(&[
            (0, Value::IntValue(1)),
            (2, Value::IntValue(2)),
            (100, Value::IntValue(3)),
        ]);
        // count prefix
        assert_eq!(buf[1..], expected_wire[..]);
        assert_eq!(buf[0], 0x03); // 3 pairs

        let decoded = KeyValuePairs::decode(&mut buf).unwrap();
        assert_eq!(decoded.0.len(), 3);
        assert_eq!(decoded.get(0).unwrap().value, Value::IntValue(1));
        assert_eq!(decoded.get(2).unwrap().value, Value::IntValue(2));
        assert_eq!(decoded.get(100).unwrap().value, Value::IntValue(3));
    }

    #[test]
    fn encode_sorts_before_delta() {
        // Insert out of order; encode must sort them so deltas are non-negative.
        let mut kvps = KeyValuePairs::new();
        kvps.set_intvalue(100, 99);
        kvps.set_intvalue(0, 1);
        kvps.set_intvalue(2, 2);

        let mut buf = BytesMut::new();
        kvps.encode(&mut buf).unwrap();
        let decoded = KeyValuePairs::decode(&mut buf).unwrap();

        // All three keys must survive the round-trip regardless of insertion order.
        assert_eq!(decoded.0.len(), 3);
        assert_eq!(decoded.get(0).unwrap().value, Value::IntValue(1));
        assert_eq!(decoded.get(2).unwrap().value, Value::IntValue(2));
        assert_eq!(decoded.get(100).unwrap().value, Value::IntValue(99));
    }

    // ── parity enforcement ────────────────────────────────────────────────────

    #[test]
    fn encode_rejects_odd_key_with_int_value() {
        let kvp = KeyValuePair::new(1, Value::IntValue(0)); // odd key, int value → invalid
        let mut buf = BytesMut::new();
        // Wrap in a collection to exercise the encode_with_prev path.
        let kvps = KeyValuePairs(vec![kvp]);
        assert!(matches!(
            kvps.encode(&mut buf).unwrap_err(),
            EncodeError::InvalidValue
        ));
    }

    #[test]
    fn encode_rejects_even_key_with_bytes_value() {
        let kvp = KeyValuePair::new(0, Value::BytesValue(vec![0x01])); // even key, bytes value → invalid
        let kvps = KeyValuePairs(vec![kvp]);
        let mut buf = BytesMut::new();
        assert!(matches!(
            kvps.encode(&mut buf).unwrap_err(),
            EncodeError::InvalidValue
        ));
    }

    // ── overflow / bounds ─────────────────────────────────────────────────────

    #[test]
    fn decode_detects_delta_overflow() {
        // A single QUIC varint delta is at most 2^62-1. To overflow u64::MAX via
        // cumulative addition we need 5 max-delta pairs:
        //   5 * (2^62-1) = 23058430092136939515 > u64::MAX (18446744073709551615)
        //
        // Abs-type parity after each pair (k * max_delta):
        //   k=1: 4611686018427387903  (odd  → bytes, length=0)
        //   k=2: 9223372036854775806  (even → int,   value=0)
        //   k=3: 13835058055282163709 (odd  → bytes, length=0)
        //   k=4: 18446744073709551612 (even → int,   value=0)
        //   k=5: overflow → KvpTypeOverflow
        let max_delta: u64 = (1u64 << 62) - 1;
        let mut buf = BytesMut::new();
        (5u64).encode(&mut buf).unwrap(); // count = 5

        // Pair 1: abs = max_delta (odd → bytes, length=0)
        max_delta.encode(&mut buf).unwrap();
        (0usize).encode(&mut buf).unwrap();

        // Pair 2: abs = 2*max_delta (even → int, value=0)
        max_delta.encode(&mut buf).unwrap();
        (0u64).encode(&mut buf).unwrap();

        // Pair 3: abs = 3*max_delta (odd → bytes, length=0)
        max_delta.encode(&mut buf).unwrap();
        (0usize).encode(&mut buf).unwrap();

        // Pair 4: abs = 4*max_delta (even → int, value=0)
        max_delta.encode(&mut buf).unwrap();
        (0u64).encode(&mut buf).unwrap();

        // Pair 5: abs = 5*max_delta → overflow
        max_delta.encode(&mut buf).unwrap();

        let err = KeyValuePairs::decode(&mut buf).unwrap_err();
        assert!(
            matches!(err, DecodeError::KvpTypeOverflow),
            "expected KvpTypeOverflow, got {:?}",
            err
        );
    }

    #[test]
    fn decode_rejects_bytes_value_too_long() {
        // Craft: count=1, delta=1 (abs=1, odd), length = u16::MAX + 1
        let mut buf = BytesMut::new();
        (1u64).encode(&mut buf).unwrap(); // count
        (1u64).encode(&mut buf).unwrap(); // delta → abs_type = 1 (odd)
        let too_long = MAX_BYTES_VALUE_LEN + 1;
        too_long.encode(&mut buf).unwrap(); // length field

        let err = KeyValuePairs::decode(&mut buf).unwrap_err();
        assert!(
            matches!(err, DecodeError::KeyValuePairLengthExceeded()),
            "expected KeyValuePairLengthExceeded, got {:?}",
            err
        );
    }

    #[test]
    fn decode_large_count_does_not_allocate_count_capacity() {
        let mut buf = BytesMut::new();
        ((1u64 << 62) - 1).encode(&mut buf).unwrap();

        let err = KeyValuePairs::decode(&mut buf).unwrap_err();
        assert!(matches!(err, DecodeError::More(_)));
    }

    // ── duplicate key detection ───────────────────────────────────────────────

    #[test]
    fn has_duplicate_keys_detects_duplicates() {
        let mut kvps = KeyValuePairs::new();
        kvps.0.push(KeyValuePair::new_int(0, 1));
        kvps.0.push(KeyValuePair::new_int(0, 2)); // duplicate key
        assert!(kvps.has_duplicate_keys());
    }

    #[test]
    fn has_duplicate_keys_no_false_positive() {
        let mut kvps = KeyValuePairs::new();
        kvps.set_intvalue(0, 1);
        kvps.set_intvalue(2, 2);
        assert!(!kvps.has_duplicate_keys());
    }

    // ── empty collection ──────────────────────────────────────────────────────

    #[test]
    fn empty_kvps_roundtrip() {
        let kvps = KeyValuePairs::new();
        let mut buf = BytesMut::new();
        kvps.encode(&mut buf).unwrap();
        assert_eq!(buf.to_vec(), vec![0x00]); // count = 0
        let decoded = KeyValuePairs::decode(&mut buf).unwrap();
        assert_eq!(decoded, kvps);
    }

    // ── legacy byte-vector compatibility ──────────────────────────────────────
    // These verify the existing test expectations from the pre-draft-16 code
    // still hold for common patterns.

    #[test]
    fn existing_single_bytes_compat() {
        let mut buf = BytesMut::new();
        let mut kvps = KeyValuePairs::new();
        kvps.set_bytesvalue(1, vec![0x01, 0x02, 0x03, 0x04, 0x05]);
        kvps.encode(&mut buf).unwrap();
        assert_eq!(
            buf.to_vec(),
            vec![
                0x01, // count=1
                0x01, // delta=1 → abs_type=1 (odd, bytes)
                0x05, 0x01, 0x02, 0x03, 0x04, 0x05, // length=5, value
            ]
        );
        let decoded = KeyValuePairs::decode(&mut buf).unwrap();
        assert_eq!(decoded, kvps);
    }

    #[test]
    fn existing_multi_compat() {
        let mut buf = BytesMut::new();
        let mut kvps = KeyValuePairs::new();
        kvps.set_intvalue(0, 0);
        kvps.set_intvalue(100, 100);
        kvps.set_bytesvalue(1, vec![0x01, 0x02, 0x03, 0x04, 0x05]);
        kvps.encode(&mut buf).unwrap();
        let decoded = KeyValuePairs::decode(&mut buf).unwrap();
        assert_eq!(decoded.0.len(), 3);
        assert_eq!(decoded.get(0).unwrap().value, Value::IntValue(0));
        assert_eq!(decoded.get(100).unwrap().value, Value::IntValue(100));
        assert_eq!(
            decoded.get(1).unwrap().value,
            Value::BytesValue(vec![0x01, 0x02, 0x03, 0x04, 0x05])
        );
    }

    // ── draft-17+ typed uint8 values ──────────────────────────────────────────

    #[test]
    fn u8_typed_value_encodes_as_single_byte() {
        // Even type 0x20 with value 200 (≥ 64): the generic even=varint rule
        // needs two bytes, but the u8-typed override must emit exactly one.
        let u8_types: &[u64] = &[0x20];
        let kvps = KeyValuePairs(vec![KeyValuePair::new_int(0x20, 200)]);

        let mut typed = BytesMut::new();
        kvps.encode_with_u8_types(&mut typed, u8_types).unwrap();
        // count=1, delta=0x20, value=0xC8 (single byte)
        assert_eq!(typed.to_vec(), vec![0x01, 0x20, 0xC8]);

        let decoded = KeyValuePairs::decode_with_u8_types(&mut typed, u8_types).unwrap();
        assert_eq!(decoded, kvps);
    }

    #[test]
    fn u8_typed_value_rejects_out_of_range() {
        let u8_types: &[u64] = &[0x20];
        let kvps = KeyValuePairs(vec![KeyValuePair::new_int(0x20, 256)]);
        let mut buf = BytesMut::new();
        assert!(matches!(
            kvps.encode_with_u8_types(&mut buf, u8_types).unwrap_err(),
            EncodeError::InvalidValue
        ));
    }

    #[test]
    fn generic_kvp_still_varint_for_even_types() {
        // Without a u8-type override, even type 0x20 = 200 must use the two-byte
        // varint form (unchanged draft-16 behaviour, e.g. SETUP parameters).
        let kvps = KeyValuePairs(vec![KeyValuePair::new_int(0x20, 200)]);
        let mut buf = BytesMut::new();
        kvps.encode(&mut buf).unwrap();
        // count=1, delta=0x20, draft-17 leading-1-bits varint(200)=0x80 0xC8
        assert_eq!(buf.to_vec(), vec![0x01, 0x20, 0x80, 0xC8]);
        let decoded = KeyValuePairs::decode(&mut buf).unwrap();
        assert_eq!(decoded, kvps);
    }

    #[test]
    fn empty_u8_types_matches_generic_for_mixed_content() {
        let kvps = KeyValuePairs(vec![
            KeyValuePair::new_int(0, 5),
            KeyValuePair::new_bytes(1, vec![0xAA]),
            KeyValuePair::new_int(2, 300),
        ]);
        let mut buf = BytesMut::new();
        kvps.encode(&mut buf).unwrap();

        let via_typed = KeyValuePairs::decode_with_u8_types(&mut buf.clone(), &[]).unwrap();
        let via_generic = KeyValuePairs::decode(&mut buf).unwrap();
        assert_eq!(via_typed, via_generic);
        assert_eq!(via_typed, kvps);
    }
}

impl KeyValuePairs {
    /// Decode KVPs from a length-bounded byte slice (draft-18+).
    ///
    /// Unlike the count-prefixed [`Decode`] impl, this reads KVPs until
    /// exactly `byte_len` bytes have been consumed. No leading count varint.
    pub fn decode_bounded(r: &mut impl bytes::Buf, byte_len: usize) -> Result<Self, DecodeError> {
        if byte_len == 0 {
            return Ok(Self::default());
        }

        <u64 as Decode>::decode_remaining(r, byte_len)?;
        let mut payload = r.copy_to_bytes(byte_len);

        let mut kvps = Vec::new();
        let mut prev = 0u64;

        while payload.has_remaining() {
            let (pair, new_prev) = KeyValuePair::decode_with_prev(&mut payload, prev)?;
            prev = new_prev;
            kvps.push(pair);
        }

        Ok(KeyValuePairs(kvps))
    }

    /// Encode KVPs without a leading count prefix (draft-18+ length-bounded).
    ///
    /// Returns the encoded bytes. The caller writes the length separately.
    pub fn encode_bounded(&self) -> Result<Vec<u8>, EncodeError> {
        let mut buf = Vec::new();
        let mut sorted = self.0.clone();
        sorted.sort_by_key(|k| k.key);

        let mut prev = 0u64;
        for kvp in &sorted {
            prev = kvp.encode_with_prev(&mut buf, prev)?;
        }
        Ok(buf)
    }
}
