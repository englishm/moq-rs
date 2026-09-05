// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Track namespace and track name encoding per draft-ietf-moq-transport-16 §2.4.1.
//!
//! Rules enforced here:
//!
//! - A **full** `TrackNamespace` must have 1–32 fields; each field must be ≥ 1 byte.
//! - A **prefix** `TrackNamespacePrefix` (used in `SUBSCRIBE_NAMESPACE`) may have
//!   0–32 fields; each non-empty field must also be ≥ 1 byte.
//! - The total length of a Full Track Name (sum of all namespace field lengths +
//!   track name length) MUST NOT exceed 4096 bytes.  Validated by message
//!   decoders via [`validate_full_track_name`].
//! - Track names are arbitrary bytes and may be empty.

use super::{Decode, DecodeError, Encode, EncodeError, TupleField};
use core::hash::{Hash, Hasher};
use std::borrow::Cow;
use std::convert::TryFrom;
use std::fmt;
use thiserror::Error;

/// Maximum total length of a Full Track Name (namespace fields + track name).
pub const MAX_FULL_TRACK_NAME_LEN: usize = 4096;

fn namespace_fields_byte_len(fields: &[TupleField]) -> usize {
    fields.iter().map(|f| f.value.len()).sum()
}

/// Stream a `/`-separated namespace path straight into a formatter.
///
/// Writing through the formatter keeps `Display` allocation-free, which matters
/// because namespaces are used as `tracing` fields on hot relay paths: with `%ns`
/// the path is only rendered when the subscriber actually records the event, and
/// even then nothing is allocated. `String::from_utf8_lossy` borrows for valid
/// UTF-8, so only genuinely invalid fields allocate a replacement string.
fn write_namespace_path(fields: &[TupleField], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for field in fields {
        f.write_str("/")?;
        f.write_str(&String::from_utf8_lossy(&field.value))?;
    }
    Ok(())
}

fn decode_namespace_fields<R: bytes::Buf>(
    r: &mut R,
    min_fields: usize,
    max_fields: usize,
    kind: &str,
) -> Result<Vec<TupleField>, DecodeError> {
    let count = usize::decode(r)?;
    if count < min_fields || count > max_fields {
        return Err(DecodeError::FieldBoundsExceeded(format!(
            "{kind} must have {min_fields}-{max_fields} fields, got {count}"
        )));
    }

    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        let field = TupleField::decode(r)?;
        if field.value.is_empty() {
            return Err(DecodeError::EmptyNamespaceField);
        }
        fields.push(field);
    }

    Ok(fields)
}

fn encode_namespace_fields<W: bytes::BufMut>(
    fields: &[TupleField],
    min_fields: usize,
    max_fields: usize,
    kind: &str,
    w: &mut W,
) -> Result<(), EncodeError> {
    if fields.len() < min_fields || fields.len() > max_fields {
        return Err(EncodeError::FieldBoundsExceeded(format!(
            "{kind} must have {min_fields}-{max_fields} fields"
        )));
    }

    fields.len().encode(w)?;
    for field in fields {
        if field.value.is_empty() {
            return Err(EncodeError::EmptyNamespaceField);
        }
        field.encode(w)?;
    }

    Ok(())
}

fn validate_track_namespace_fields(
    fields: &[TupleField],
    min_fields: usize,
    max_fields: usize,
) -> Result<(), TrackNamespaceError> {
    if fields.len() < min_fields {
        return Err(TrackNamespaceError::TooFewFields);
    }
    if fields.len() > max_fields {
        return Err(TrackNamespaceError::TooManyFields(fields.len(), max_fields));
    }
    for field in fields {
        if field.value.is_empty() {
            return Err(TrackNamespaceError::EmptyField);
        }
        if field.value.len() > TupleField::MAX_VALUE_SIZE {
            return Err(TrackNamespaceError::FieldTooLarge(
                field.value.len(),
                TupleField::MAX_VALUE_SIZE,
            ));
        }
    }

    Ok(())
}

/// A Track Name is arbitrary bytes and may be empty.
#[derive(Clone, Default, Eq, Hash, PartialEq)]
pub struct TrackName {
    value: Vec<u8>,
}

impl TrackName {
    pub fn new(value: Vec<u8>) -> Self {
        Self { value }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.value
    }

    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.value)
    }
}

impl AsRef<[u8]> for TrackName {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl From<Vec<u8>> for TrackName {
    fn from(value: Vec<u8>) -> Self {
        Self::new(value)
    }
}

impl From<&[u8]> for TrackName {
    fn from(value: &[u8]) -> Self {
        Self::new(value.to_vec())
    }
}

impl From<String> for TrackName {
    fn from(value: String) -> Self {
        Self::new(value.into_bytes())
    }
}

impl From<&str> for TrackName {
    fn from(value: &str) -> Self {
        Self::new(value.as_bytes().to_vec())
    }
}

impl From<&String> for TrackName {
    fn from(value: &String) -> Self {
        Self::from(value.as_str())
    }
}

impl From<&TrackName> for TrackName {
    fn from(value: &TrackName) -> Self {
        value.clone()
    }
}

impl Decode for TrackName {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let size = usize::decode(r)?;
        if size > MAX_FULL_TRACK_NAME_LEN {
            return Err(DecodeError::TrackNameTooLong);
        }
        Self::decode_remaining(r, size)?;

        let mut value = vec![0; size];
        r.copy_to_slice(&mut value);
        Ok(Self { value })
    }
}

impl Encode for TrackName {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        if self.value.len() > MAX_FULL_TRACK_NAME_LEN {
            return Err(EncodeError::FieldBoundsExceeded("TrackName".to_string()));
        }
        self.value.len().encode(w)?;
        Self::encode_remaining(w, self.value.len())?;
        w.put_slice(&self.value);
        Ok(())
    }
}

impl fmt::Debug for TrackName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl fmt::Display for TrackName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_string_lossy())
    }
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum TrackNamespaceError {
    #[error("too many fields: {0} exceeds maximum of {1}")]
    TooManyFields(usize, usize),

    #[error("too few fields: full track namespace requires at least 1 field")]
    TooFewFields,

    #[error("namespace too large: {0} bytes exceeds maximum of {1}")]
    NamespaceTooLarge(usize, usize),

    #[error("field too large: {0} bytes exceeds maximum of {1}")]
    FieldTooLarge(usize, usize),

    #[error("empty field: namespace fields must be at least 1 byte")]
    EmptyField,
}

// ─── TrackNamespace (full, 1–32 non-empty fields) ─────────────────────────────

/// A full Track Namespace: 1–32 non-empty byte fields.
///
/// Used in `SUBSCRIBE`, `PUBLISH`, `PUBLISH_NAMESPACE`, `TRACK_STATUS`, etc.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct TrackNamespace {
    pub fields: Vec<TupleField>,
}

impl TrackNamespace {
    pub const MAX_FIELDS: usize = 32;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, field: TupleField) {
        self.fields.push(field);
    }

    pub fn clear(&mut self) {
        self.fields.clear();
    }

    /// Build from a `/`-separated UTF-8 path (each segment becomes a field).
    /// Empty segments (e.g. leading `/`) are included as empty fields and will
    /// fail validation; callers producing full namespaces should ensure no
    /// empty segments.
    pub fn from_utf8_path(path: &str) -> Self {
        let mut ns = TrackNamespace::new();
        for part in path.split('/') {
            ns.add(TupleField::from_utf8(part));
        }
        ns
    }

    /// Render as a `/`-separated UTF-8 path.
    ///
    /// Allocates. Prefer the [`fmt::Display`] impl (e.g. `%namespace` in a
    /// `tracing` field) when a `String` is not actually needed.
    pub fn to_utf8_path(&self) -> String {
        self.to_string()
    }

    /// Sum of all field lengths. Used for full-track-name limit calculation.
    pub fn namespace_byte_len(&self) -> usize {
        namespace_fields_byte_len(&self.fields)
    }

    /// Return the namespace suffix after removing `prefix`, if it matches.
    pub fn strip_prefix(&self, prefix: &TrackNamespacePrefix) -> Option<TrackNamespacePrefix> {
        if !prefix.is_prefix_of(self) {
            return None;
        }

        Some(TrackNamespacePrefix {
            fields: self.fields[prefix.fields.len()..].to_vec(),
        })
    }

    fn validate_namespace_byte_len(&self) -> Result<(), DecodeError> {
        if self.namespace_byte_len() > MAX_FULL_TRACK_NAME_LEN {
            return Err(DecodeError::TrackNameTooLong);
        }

        Ok(())
    }
}

impl Hash for TrackNamespace {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fields.hash(state);
    }
}

impl Decode for TrackNamespace {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        // Draft-16 §2.4.1: full namespaces have 1-32 non-empty fields.
        let fields = decode_namespace_fields(r, 1, Self::MAX_FIELDS, "TrackNamespace")?;
        let namespace = Self { fields };
        namespace.validate_namespace_byte_len()?;
        Ok(namespace)
    }
}

impl Encode for TrackNamespace {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        if self.namespace_byte_len() > MAX_FULL_TRACK_NAME_LEN {
            return Err(EncodeError::FieldBoundsExceeded(
                "TrackNamespace".to_string(),
            ));
        }
        encode_namespace_fields(&self.fields, 1, Self::MAX_FIELDS, "TrackNamespace", w)
    }
}

impl fmt::Debug for TrackNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl fmt::Display for TrackNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_namespace_path(&self.fields, f)
    }
}

impl TryFrom<Vec<TupleField>> for TrackNamespace {
    type Error = TrackNamespaceError;

    fn try_from(fields: Vec<TupleField>) -> Result<Self, Self::Error> {
        validate_track_namespace_fields(&fields, 1, Self::MAX_FIELDS)?;
        let namespace_len = namespace_fields_byte_len(&fields);
        if namespace_len > MAX_FULL_TRACK_NAME_LEN {
            return Err(TrackNamespaceError::NamespaceTooLarge(
                namespace_len,
                MAX_FULL_TRACK_NAME_LEN,
            ));
        }
        Ok(Self { fields })
    }
}

impl TryFrom<&str> for TrackNamespace {
    type Error = TrackNamespaceError;

    fn try_from(path: &str) -> Result<Self, Self::Error> {
        let fields: Vec<TupleField> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(TupleField::from_utf8)
            .collect();
        Self::try_from(fields)
    }
}

impl TryFrom<String> for TrackNamespace {
    type Error = TrackNamespaceError;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        Self::try_from(path.as_str())
    }
}

impl TryFrom<Vec<&str>> for TrackNamespace {
    type Error = TrackNamespaceError;

    fn try_from(parts: Vec<&str>) -> Result<Self, Self::Error> {
        let fields: Vec<TupleField> = parts.into_iter().map(TupleField::from_utf8).collect();
        Self::try_from(fields)
    }
}

impl TryFrom<Vec<String>> for TrackNamespace {
    type Error = TrackNamespaceError;

    fn try_from(parts: Vec<String>) -> Result<Self, Self::Error> {
        let fields: Vec<TupleField> = parts.iter().map(|s| TupleField::from_utf8(s)).collect();
        Self::try_from(fields)
    }
}

// ─── TrackNamespacePrefix (0–32 fields, for SUBSCRIBE_NAMESPACE) ──────────────

/// A Track Namespace Prefix used in `SUBSCRIBE_NAMESPACE`.
///
/// Unlike [`TrackNamespace`], a prefix is allowed to have 0 fields (matching
/// all namespaces).  Fields that are present must still be non-empty.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct TrackNamespacePrefix {
    pub fields: Vec<TupleField>,
}

impl TrackNamespacePrefix {
    pub const MAX_FIELDS: usize = 32;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_utf8_path(path: &str) -> Self {
        let mut prefix = TrackNamespacePrefix::new();
        for part in path.split('/').filter(|s| !s.is_empty()) {
            prefix.fields.push(TupleField::from_utf8(part));
        }
        prefix
    }

    /// Render as a `/`-separated UTF-8 path.
    ///
    /// Allocates. Prefer the [`fmt::Display`] impl (e.g. `%prefix` in a
    /// `tracing` field) when a `String` is not actually needed.
    pub fn to_utf8_path(&self) -> String {
        self.to_string()
    }

    /// Return true when this prefix matches the beginning of `namespace`.
    pub fn is_prefix_of(&self, namespace: &TrackNamespace) -> bool {
        self.fields.len() <= namespace.fields.len()
            && self
                .fields
                .iter()
                .zip(namespace.fields.iter())
                .all(|(prefix, field)| prefix == field)
    }

    /// Return true when two prefixes overlap, meaning one is a prefix of the other.
    pub fn overlaps(&self, other: &Self) -> bool {
        self.fields
            .iter()
            .zip(other.fields.iter())
            .all(|(left, right)| left == right)
    }

    /// Build a full namespace from this prefix and a suffix received in NAMESPACE.
    pub fn join_suffix(
        &self,
        suffix: &TrackNamespacePrefix,
    ) -> Result<TrackNamespace, TrackNamespaceError> {
        let mut fields = Vec::with_capacity(self.fields.len() + suffix.fields.len());
        fields.extend(self.fields.iter().cloned());
        fields.extend(suffix.fields.iter().cloned());
        TrackNamespace::try_from(fields)
    }
}

impl Hash for TrackNamespacePrefix {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fields.hash(state);
    }
}

impl Decode for TrackNamespacePrefix {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        // Draft-16 §9.25: prefixes have 0-32 non-empty fields.
        let fields = decode_namespace_fields(r, 0, Self::MAX_FIELDS, "TrackNamespacePrefix")?;
        Ok(Self { fields })
    }
}

impl Encode for TrackNamespacePrefix {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        encode_namespace_fields(&self.fields, 0, Self::MAX_FIELDS, "TrackNamespacePrefix", w)
    }
}

impl fmt::Debug for TrackNamespacePrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl fmt::Display for TrackNamespacePrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_namespace_path(&self.fields, f)
    }
}

// ─── Full track name length helper ───────────────────────────────────────────

/// Compute the wire-encoded byte length of a Full Track Name.
///
/// This is the sum of each namespace field's length plus the track name length.
/// If the result exceeds [`MAX_FULL_TRACK_NAME_LEN`] the caller should close
/// the session with PROTOCOL_VIOLATION.
pub fn full_track_name_len(namespace: &TrackNamespace, track_name: &[u8]) -> usize {
    namespace.namespace_byte_len() + track_name.len()
}

/// Validate that a Full Track Name is within the draft-16 4096-byte limit.
pub fn validate_full_track_name(
    namespace: &TrackNamespace,
    track_name: &[u8],
) -> Result<(), DecodeError> {
    if full_track_name_len(namespace, track_name) > MAX_FULL_TRACK_NAME_LEN {
        return Err(DecodeError::TrackNameTooLong);
    }

    Ok(())
}

// ─── Add missing EncodeError variant ─────────────────────────────────────────
// (defined here to keep it adjacent to the validation logic)

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::{Bytes, BytesMut};
    use std::convert::TryInto;

    // ── TrackNamespace encode/decode ──────────────────────────────────────────

    #[test]
    fn encode_decode() {
        let mut buf = BytesMut::new();

        let t = TrackNamespace::from_utf8_path("test/path/to/resource");
        t.encode(&mut buf).unwrap();
        #[rustfmt::skip]
        assert_eq!(
            buf.to_vec(),
            vec![
                0x04, // 4 tuple fields
                0x04, 0x74, 0x65, 0x73, 0x74,       // "test"
                0x04, 0x70, 0x61, 0x74, 0x68,       // "path"
                0x02, 0x74, 0x6f,                   // "to"
                0x08, 0x72, 0x65, 0x73, 0x6f, 0x75, 0x72, 0x63, 0x65, // "resource"
            ]
        );
        let decoded = TrackNamespace::decode(&mut buf).unwrap();
        assert_eq!(decoded, t);
    }

    #[test]
    fn track_name_allows_non_utf8_bytes() {
        let name = TrackName::from(vec![0xff, 0x00, b'a']);
        let mut buf = BytesMut::new();

        name.encode(&mut buf).unwrap();
        let decoded = TrackName::decode(&mut buf).unwrap();

        assert_eq!(decoded.as_bytes(), &[0xff, 0x00, b'a']);
    }

    #[test]
    fn encode_single_field() {
        let mut buf = BytesMut::new();
        let mut t = TrackNamespace::new();
        t.add(TupleField::from_utf8("test"));
        t.encode(&mut buf).unwrap();
        assert_eq!(buf.to_vec(), vec![0x01, 0x04, 0x74, 0x65, 0x73, 0x74]);
        let decoded = TrackNamespace::decode(&mut buf).unwrap();
        assert_eq!(decoded, t);
    }

    // ── 0-field rejection ─────────────────────────────────────────────────────

    #[test]
    fn decode_zero_fields_is_error() {
        // wire: count = 0
        let data: Vec<u8> = vec![0x00];
        let mut buf: Bytes = data.into();
        let err = TrackNamespace::decode(&mut buf).unwrap_err();
        assert!(
            matches!(err, DecodeError::FieldBoundsExceeded(_)),
            "expected FieldBoundsExceeded, got {:?}",
            err
        );
    }

    #[test]
    fn encode_zero_fields_is_error() {
        let t = TrackNamespace::new(); // empty
        let mut buf = BytesMut::new();
        assert!(matches!(
            t.encode(&mut buf).unwrap_err(),
            EncodeError::FieldBoundsExceeded(_)
        ));
    }

    // ── >32 fields rejection ──────────────────────────────────────────────────

    #[test]
    fn encode_too_large() {
        let mut buf = BytesMut::new();
        let mut t = TrackNamespace::new();
        for i in 0..=TrackNamespace::MAX_FIELDS {
            t.add(TupleField::from_utf8(&format!("f{}", i)));
        }
        assert!(matches!(
            t.encode(&mut buf).unwrap_err(),
            EncodeError::FieldBoundsExceeded(_)
        ));
    }

    #[test]
    fn decode_too_large() {
        let mut data: Vec<u8> = vec![0x00; 256];
        data[0] = (TrackNamespace::MAX_FIELDS + 1) as u8; // count = 33
        let mut buf: Bytes = data.into();
        assert!(matches!(
            TrackNamespace::decode(&mut buf).unwrap_err(),
            DecodeError::FieldBoundsExceeded(_)
        ));
    }

    // ── empty field rejection ─────────────────────────────────────────────────

    #[test]
    fn decode_empty_field_is_error() {
        // wire: count=1, field length=0
        let data: Vec<u8> = vec![0x01, 0x00]; // count=1, field_len=0
        let mut buf: Bytes = data.into();
        assert!(matches!(
            TrackNamespace::decode(&mut buf).unwrap_err(),
            DecodeError::EmptyNamespaceField
        ));
    }

    #[test]
    fn encode_empty_field_is_error() {
        let mut t = TrackNamespace::new();
        t.add(TupleField { value: vec![] }); // empty field
        let mut buf = BytesMut::new();
        assert!(matches!(
            t.encode(&mut buf).unwrap_err(),
            EncodeError::EmptyNamespaceField
        ));
    }

    // ── TryFrom conversions ───────────────────────────────────────────────────

    #[test]
    fn try_from_str() {
        let ns: TrackNamespace = "test/path/to/resource".try_into().unwrap();
        assert_eq!(ns.fields.len(), 4);
        assert_eq!(ns.to_utf8_path(), "/test/path/to/resource");
    }

    #[test]
    fn try_from_string() {
        let path = String::from("test/path");
        let ns: TrackNamespace = path.try_into().unwrap();
        assert_eq!(ns.fields.len(), 2);
        assert_eq!(ns.to_utf8_path(), "/test/path");
    }

    #[test]
    fn try_from_vec_str() {
        let parts = vec!["test", "path", "to", "resource"];
        let ns: TrackNamespace = parts.try_into().unwrap();
        assert_eq!(ns.fields.len(), 4);
        assert_eq!(ns.to_utf8_path(), "/test/path/to/resource");
    }

    #[test]
    fn try_from_vec_string() {
        let parts = vec![String::from("test"), String::from("path")];
        let ns: TrackNamespace = parts.try_into().unwrap();
        assert_eq!(ns.fields.len(), 2);
        assert_eq!(ns.to_utf8_path(), "/test/path");
    }

    #[test]
    fn try_from_vec_tuple_field() {
        let fields = vec![TupleField::from_utf8("test"), TupleField::from_utf8("path")];
        let ns: TrackNamespace = fields.try_into().unwrap();
        assert_eq!(ns.fields.len(), 2);
        assert_eq!(ns.to_utf8_path(), "/test/path");
    }

    #[test]
    fn try_from_empty_vec_is_error() {
        let fields: Vec<TupleField> = vec![];
        let result: Result<TrackNamespace, _> = fields.try_into();
        assert!(matches!(
            result.unwrap_err(),
            TrackNamespaceError::TooFewFields
        ));
    }

    #[test]
    fn try_from_too_many_fields() {
        let fields: Vec<TupleField> = (0..=TrackNamespace::MAX_FIELDS)
            .map(|i| TupleField::from_utf8(&format!("f{}", i)))
            .collect();
        let result: Result<TrackNamespace, _> = fields.try_into();
        assert!(matches!(
            result.unwrap_err(),
            TrackNamespaceError::TooManyFields(33, 32)
        ));
    }

    #[test]
    fn try_from_field_too_large() {
        let large_value = "x".repeat(TupleField::MAX_VALUE_SIZE + 1);
        let fields = vec![TupleField {
            value: large_value.into_bytes(),
        }];
        let result: Result<TrackNamespace, _> = fields.try_into();
        assert!(matches!(
            result.unwrap_err(),
            TrackNamespaceError::FieldTooLarge(4097, 4096)
        ));
    }

    #[test]
    fn try_from_empty_field_is_error() {
        let fields = vec![TupleField { value: vec![] }];
        let result: Result<TrackNamespace, _> = fields.try_into();
        assert!(matches!(
            result.unwrap_err(),
            TrackNamespaceError::EmptyField
        ));
    }

    // ── TrackNamespacePrefix ──────────────────────────────────────────────────

    #[test]
    fn prefix_allows_zero_fields() {
        let prefix = TrackNamespacePrefix::new();
        let mut buf = BytesMut::new();
        prefix.encode(&mut buf).unwrap(); // must not error
        assert_eq!(buf.to_vec(), vec![0x00]);
        let decoded = TrackNamespacePrefix::decode(&mut buf).unwrap();
        assert_eq!(decoded.fields.len(), 0);
    }

    #[test]
    fn prefix_roundtrip() {
        let prefix = TrackNamespacePrefix::from_utf8_path("example.com/meeting=123");
        let mut buf = BytesMut::new();
        prefix.encode(&mut buf).unwrap();
        let decoded = TrackNamespacePrefix::decode(&mut buf).unwrap();
        assert_eq!(decoded.fields.len(), prefix.fields.len());
        assert_eq!(decoded.to_utf8_path(), prefix.to_utf8_path());
    }

    #[test]
    fn prefix_rejects_too_many_fields() {
        let mut prefix = TrackNamespacePrefix::new();
        for i in 0..=TrackNamespacePrefix::MAX_FIELDS {
            prefix
                .fields
                .push(TupleField::from_utf8(&format!("f{}", i)));
        }
        let mut buf = BytesMut::new();
        assert!(matches!(
            prefix.encode(&mut buf).unwrap_err(),
            EncodeError::FieldBoundsExceeded(_)
        ));
    }

    #[test]
    fn prefix_rejects_empty_field() {
        let mut prefix = TrackNamespacePrefix::new();
        prefix.fields.push(TupleField { value: vec![] });
        let mut buf = BytesMut::new();
        assert!(matches!(
            prefix.encode(&mut buf).unwrap_err(),
            EncodeError::EmptyNamespaceField
        ));
    }

    #[test]
    fn prefix_matches_namespace_start() {
        let namespace = TrackNamespace::from_utf8_path("example.com/meeting=123/participant=100");

        assert!(TrackNamespacePrefix::new().is_prefix_of(&namespace));
        assert!(
            TrackNamespacePrefix::from_utf8_path("example.com/meeting=123")
                .is_prefix_of(&namespace)
        );
        assert!(
            TrackNamespacePrefix::from_utf8_path("example.com/meeting=123/participant=100")
                .is_prefix_of(&namespace)
        );
        assert!(
            !TrackNamespacePrefix::from_utf8_path("example.com/meeting=456")
                .is_prefix_of(&namespace)
        );
        assert!(!TrackNamespacePrefix::from_utf8_path(
            "example.com/meeting=123/participant=100/video"
        )
        .is_prefix_of(&namespace));
    }

    #[test]
    fn strip_prefix_returns_namespace_suffix() {
        let namespace = TrackNamespace::from_utf8_path("example.com/meeting=123/participant=100");
        let prefix = TrackNamespacePrefix::from_utf8_path("example.com/meeting=123");

        let suffix = namespace.strip_prefix(&prefix).unwrap();

        assert_eq!(suffix.to_utf8_path(), "/participant=100");
    }

    #[test]
    fn strip_prefix_returns_empty_suffix_for_exact_match() {
        let namespace = TrackNamespace::from_utf8_path("example.com/meeting=123");
        let prefix = TrackNamespacePrefix::from_utf8_path("example.com/meeting=123");

        let suffix = namespace.strip_prefix(&prefix).unwrap();

        assert!(suffix.fields.is_empty());
    }

    #[test]
    fn strip_prefix_rejects_non_matching_prefix() {
        let namespace = TrackNamespace::from_utf8_path("example.com/meeting=123");
        let prefix = TrackNamespacePrefix::from_utf8_path("example.org");

        assert!(namespace.strip_prefix(&prefix).is_none());
    }

    #[test]
    fn join_suffix_reconstructs_namespace() {
        let prefix = TrackNamespacePrefix::from_utf8_path("example.com/meeting=123");
        let suffix = TrackNamespacePrefix::from_utf8_path("participant=100/video");

        let namespace = prefix.join_suffix(&suffix).unwrap();

        assert_eq!(
            namespace.to_utf8_path(),
            "/example.com/meeting=123/participant=100/video"
        );
    }

    #[test]
    fn join_suffix_accepts_empty_suffix_when_prefix_is_full_namespace() {
        let prefix = TrackNamespacePrefix::from_utf8_path("example.com/meeting=123");
        let suffix = TrackNamespacePrefix::new();

        let namespace = prefix.join_suffix(&suffix).unwrap();

        assert_eq!(namespace.to_utf8_path(), "/example.com/meeting=123");
    }

    #[test]
    fn join_suffix_rejects_empty_full_namespace() {
        let prefix = TrackNamespacePrefix::new();
        let suffix = TrackNamespacePrefix::new();

        let err = prefix.join_suffix(&suffix).unwrap_err();

        assert!(matches!(err, TrackNamespaceError::TooFewFields));
    }

    #[test]
    fn prefixes_overlap_when_one_contains_the_other() {
        let root = TrackNamespacePrefix::new();
        let meeting = TrackNamespacePrefix::from_utf8_path("example.com/meeting=123");
        let participant =
            TrackNamespacePrefix::from_utf8_path("example.com/meeting=123/participant=100");
        let other_meeting = TrackNamespacePrefix::from_utf8_path("example.com/meeting=456");

        assert!(root.overlaps(&meeting));
        assert!(meeting.overlaps(&root));
        assert!(meeting.overlaps(&participant));
        assert!(participant.overlaps(&meeting));
        assert!(meeting.overlaps(&meeting));
        assert!(!meeting.overlaps(&other_meeting));
        assert!(!other_meeting.overlaps(&participant));
    }

    #[test]
    fn try_from_rejects_namespace_total_over_limit() {
        let fields = vec![
            TupleField {
                value: vec![b'a'; 2049],
            },
            TupleField {
                value: vec![b'b'; 2048],
            },
        ];

        let err = TrackNamespace::try_from(fields).unwrap_err();

        assert!(matches!(
            err,
            TrackNamespaceError::NamespaceTooLarge(4097, MAX_FULL_TRACK_NAME_LEN)
        ));
    }

    // ── Display / to_utf8_path equivalence ────────────────────────────────────

    /// `to_utf8_path` delegates to `Display`, which streams into the formatter
    /// rather than building a `String`. Both must keep producing the same path,
    /// including for fields that are not valid UTF-8 (rendered lossily).
    #[test]
    fn display_matches_to_utf8_path() {
        let namespace = TrackNamespace::from_utf8_path("example.com/meeting=123");
        assert_eq!(namespace.to_string(), namespace.to_utf8_path());
        assert_eq!(namespace.to_utf8_path(), "/example.com/meeting=123");

        let prefix = TrackNamespacePrefix::from_utf8_path("example.com");
        assert_eq!(prefix.to_string(), prefix.to_utf8_path());
        assert_eq!(prefix.to_utf8_path(), "/example.com");

        let empty = TrackNamespacePrefix::new();
        assert_eq!(empty.to_string(), empty.to_utf8_path());
        assert_eq!(empty.to_utf8_path(), "");
    }

    #[test]
    fn display_renders_non_utf8_fields_lossily() {
        let namespace = TrackNamespace {
            fields: vec![
                TupleField { value: vec![0xff] },
                TupleField::from_utf8("ok"),
            ],
        };

        assert_eq!(namespace.to_string(), namespace.to_utf8_path());
        assert_eq!(namespace.to_utf8_path(), "/\u{fffd}/ok");
    }

    /// `Debug` is written in terms of `Display`, so it must not regress into
    /// printing the raw field vector.
    #[test]
    fn debug_matches_display() {
        let namespace = TrackNamespace::from_utf8_path("a/b");
        assert_eq!(format!("{namespace:?}"), format!("{namespace}"));

        let prefix = TrackNamespacePrefix::from_utf8_path("a/b");
        assert_eq!(format!("{prefix:?}"), format!("{prefix}"));
    }

    // ── full_track_name_len ───────────────────────────────────────────────────

    #[test]
    fn full_track_name_len_basic() {
        let ns = TrackNamespace::from_utf8_path("a/b"); // fields: "a" (1 byte), "b" (1 byte)
        let track_name = b"mytrack"; // 7 bytes
        assert_eq!(full_track_name_len(&ns, track_name), 1 + 1 + 7);
    }

    #[test]
    fn full_track_name_len_at_limit() {
        // Build a namespace with one field of 4088 bytes and an empty track name (0 bytes).
        // Total = 4088 ≤ 4096.
        let big_field = vec![b'x'; 4088];
        let ns = TrackNamespace {
            fields: vec![TupleField { value: big_field }],
        };
        assert!(full_track_name_len(&ns, b"") <= MAX_FULL_TRACK_NAME_LEN);
    }

    #[test]
    fn decode_rejects_namespace_over_full_track_name_limit() {
        let mut buf = BytesMut::new();
        (2usize).encode(&mut buf).unwrap();
        let field = vec![b'x'; 2049];
        field.len().encode(&mut buf).unwrap();
        buf.extend_from_slice(&field);
        field.len().encode(&mut buf).unwrap();
        buf.extend_from_slice(&field);

        let err = TrackNamespace::decode(&mut buf).unwrap_err();
        assert!(matches!(err, DecodeError::TrackNameTooLong));
    }

    #[test]
    fn full_track_name_len_over_limit() {
        // 4092 (namespace) + 5 (track name "hello") = 4097 > 4096
        let big_field = vec![b'x'; 4092];
        let ns = TrackNamespace {
            fields: vec![TupleField { value: big_field }],
        };
        assert!(full_track_name_len(&ns, b"hello") > MAX_FULL_TRACK_NAME_LEN);
    }

    #[test]
    fn validate_full_track_name_rejects_over_limit() {
        let big_field = vec![b'a'; MAX_FULL_TRACK_NAME_LEN];
        let ns = TrackNamespace {
            fields: vec![TupleField { value: big_field }],
        };

        let err = validate_full_track_name(&ns, b"x").unwrap_err();
        assert!(matches!(err, DecodeError::TrackNameTooLong));
    }
}
