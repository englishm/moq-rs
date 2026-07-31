// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::{Decode, DecodeError, Encode, EncodeError, TupleField};
use core::hash::{Hash, Hasher};
use std::convert::TryFrom;
use std::fmt;
use thiserror::Error;

/// Error type for TrackNamespace conversion failures
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum TrackNamespaceError {
    #[error("too many fields: {0} exceeds maximum of {1}")]
    TooManyFields(usize, usize),

    #[error("field too large: {0} bytes exceeds maximum of {1}")]
    FieldTooLarge(usize, usize),
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

/// TrackNamespace
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

    pub fn from_utf8_path(path: &str) -> Self {
        let mut tuple = TrackNamespace::new();
        for part in path.split('/') {
            tuple.add(TupleField::from_utf8(part));
        }
        tuple
    }

    /// Render as a `/`-separated UTF-8 path.
    ///
    /// Allocates. Prefer the [`fmt::Display`] impl (e.g. `%namespace` in a
    /// `tracing` field) when a `String` is not actually needed.
    pub fn to_utf8_path(&self) -> String {
        self.to_string()
    }
}

impl Hash for TrackNamespace {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fields.hash(state);
    }
}

impl Decode for TrackNamespace {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let count = usize::decode(r)?;
        if count > Self::MAX_FIELDS {
            return Err(DecodeError::FieldBoundsExceeded(
                "TrackNamespace tuples".to_string(),
            ));
        }

        let mut fields = Vec::new();
        for _ in 0..count {
            fields.push(TupleField::decode(r)?);
        }
        Ok(Self { fields })
    }
}

impl Encode for TrackNamespace {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        if self.fields.len() > Self::MAX_FIELDS {
            return Err(EncodeError::FieldBoundsExceeded(
                "TrackNamespace tuples".to_string(),
            ));
        }
        self.fields.len().encode(w)?;
        for field in &self.fields {
            field.encode(w)?;
        }
        Ok(())
    }
}

impl fmt::Debug for TrackNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Just reuse the Display formatting
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
        if fields.len() > Self::MAX_FIELDS {
            return Err(TrackNamespaceError::TooManyFields(
                fields.len(),
                Self::MAX_FIELDS,
            ));
        }
        for field in &fields {
            if field.value.len() > TupleField::MAX_VALUE_SIZE {
                return Err(TrackNamespaceError::FieldTooLarge(
                    field.value.len(),
                    TupleField::MAX_VALUE_SIZE,
                ));
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use bytes::BytesMut;
    use std::convert::TryInto;

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
                // Field 1: "test"
                0x04, 0x74, 0x65, 0x73, 0x74,
                // Field 2: "path"
                0x04, 0x70, 0x61, 0x74, 0x68,
                // Field 3: "to"
                0x02, 0x74, 0x6f,
                // Field 4: "resource"
                0x08, 0x72, 0x65, 0x73, 0x6f, 0x75, 0x72, 0x63, 0x65
            ]
        );
        let decoded = TrackNamespace::decode(&mut buf).unwrap();
        assert_eq!(decoded, t);

        // Alternate construction
        let mut t = TrackNamespace::new();
        t.add(TupleField::from_utf8("test"));
        t.encode(&mut buf).unwrap();
        assert_eq!(
            buf.to_vec(),
            vec![
                0x01, // 1 tuple field
                // Field 1: "test"
                0x04, 0x74, 0x65, 0x73, 0x74
            ]
        );
        let decoded = TrackNamespace::decode(&mut buf).unwrap();
        assert_eq!(decoded, t);
    }

    #[test]
    fn encode_too_large() {
        let mut buf = BytesMut::new();

        let mut t = TrackNamespace::new();
        for i in 0..TrackNamespace::MAX_FIELDS + 1 {
            t.add(TupleField::from_utf8(&format!("field{}", i)));
        }

        let encoded = t.encode(&mut buf);
        assert!(matches!(
            encoded.unwrap_err(),
            EncodeError::FieldBoundsExceeded(_)
        ));
    }

    #[test]
    fn decode_too_large() {
        let mut data: Vec<u8> = vec![0x00; 256]; // Create a vector with 256 bytes
        data[0] = (TrackNamespace::MAX_FIELDS + 1) as u8; // Set first byte (count) to 33 as a VarInt
        let mut buf: Bytes = data.into();
        let decoded = TrackNamespace::decode(&mut buf);
        assert!(matches!(
            decoded.unwrap_err(),
            DecodeError::FieldBoundsExceeded(_)
        ));
    }

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
    fn try_from_too_many_fields() {
        let mut fields = Vec::new();
        for i in 0..TrackNamespace::MAX_FIELDS + 1 {
            fields.push(TupleField::from_utf8(&format!("field{}", i)));
        }
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

    // ── Display / to_utf8_path equivalence ────────────────────────────────────

    /// `to_utf8_path` delegates to `Display`, which streams into the formatter
    /// rather than building a `String`. Both must keep producing the same path,
    /// including for fields that are not valid UTF-8 (rendered lossily).
    #[test]
    fn display_matches_to_utf8_path() {
        let namespace = TrackNamespace::from_utf8_path("example.com/meeting=123");
        assert_eq!(namespace.to_string(), namespace.to_utf8_path());
        assert_eq!(namespace.to_utf8_path(), "/example.com/meeting=123");

        let empty = TrackNamespace::new();
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
    }
}
