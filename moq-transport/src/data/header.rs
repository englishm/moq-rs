// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::coding::{Decode, DecodeError, Encode, EncodeError};
use crate::data::{FetchHeader, SubgroupHeader};
use std::fmt;

/// Priority inherited when DEFAULT_PRIORITY is set and the Track Property is absent (§12.4).
pub const DEFAULT_PUBLISHER_PRIORITY: u8 = 128;

/// Encoding of the Subgroup ID in a draft-18 SUBGROUP_HEADER (§11.4.2).
#[derive(Copy, Debug, Clone, Eq, PartialEq)]
pub enum SubgroupIdMode {
    /// The Subgroup ID field is absent and the value is zero.
    Zero,
    /// The field is absent and the first transmitted Object ID supplies the value.
    FirstObject,
    /// The Subgroup ID is carried explicitly in the header.
    Explicit,
}

macro_rules! stream_header_types {
    (
        Fetch = $fetch_value:literal;
        $(
            $name:ident = $value:literal =>
                ($mode:ident, $properties:literal, $end_of_group:literal,
                 $default_priority:literal, $first_object:literal)
        ),+ $(,)?
    ) => {
        /// A validated draft-18 stream header type.
        #[repr(u64)]
        #[derive(Copy, Debug, Clone, Eq, PartialEq)]
        pub enum StreamHeaderType {
            Fetch = $fetch_value,
            $($name = $value),+
        }

        impl StreamHeaderType {
            fn from_value(value: u64) -> Option<Self> {
                match value {
                    $fetch_value => Some(Self::Fetch),
                    $($value => Some(Self::$name),)+
                    _ => None,
                }
            }

            /// Construct a valid draft-18 SUBGROUP_HEADER type from its semantic fields (§11.4.2).
            pub const fn subgroup(
                subgroup_id_mode: SubgroupIdMode,
                has_properties: bool,
                end_of_group: bool,
                default_priority: bool,
                first_object: bool,
            ) -> Self {
                match (
                    subgroup_id_mode,
                    has_properties,
                    end_of_group,
                    default_priority,
                    first_object,
                ) {
                    $(
                        (
                            SubgroupIdMode::$mode,
                            $properties,
                            $end_of_group,
                            $default_priority,
                            $first_object,
                        ) => Self::$name,
                    )+
                }
            }
        }
    };
}

stream_header_types! {
    Fetch = 0x05;
    SubgroupZeroId = 0x10 => (Zero, false, false, false, false),
    SubgroupZeroIdExt = 0x11 => (Zero, true, false, false, false),
    SubgroupFirstObjectId = 0x12 => (FirstObject, false, false, false, false),
    SubgroupFirstObjectIdExt = 0x13 => (FirstObject, true, false, false, false),
    SubgroupId = 0x14 => (Explicit, false, false, false, false),
    SubgroupIdExt = 0x15 => (Explicit, true, false, false, false),
    SubgroupZeroIdEndOfGroup = 0x18 => (Zero, false, true, false, false),
    SubgroupZeroIdExtEndOfGroup = 0x19 => (Zero, true, true, false, false),
    SubgroupFirstObjectIdEndOfGroup = 0x1a => (FirstObject, false, true, false, false),
    SubgroupFirstObjectIdExtEndOfGroup = 0x1b => (FirstObject, true, true, false, false),
    SubgroupIdEndOfGroup = 0x1c => (Explicit, false, true, false, false),
    SubgroupIdExtEndOfGroup = 0x1d => (Explicit, true, true, false, false),
    SubgroupZeroIdDefaultPriority = 0x30 => (Zero, false, false, true, false),
    SubgroupZeroIdExtDefaultPriority = 0x31 => (Zero, true, false, true, false),
    SubgroupFirstObjectIdDefaultPriority = 0x32 => (FirstObject, false, false, true, false),
    SubgroupFirstObjectIdExtDefaultPriority = 0x33 => (FirstObject, true, false, true, false),
    SubgroupIdDefaultPriority = 0x34 => (Explicit, false, false, true, false),
    SubgroupIdExtDefaultPriority = 0x35 => (Explicit, true, false, true, false),
    SubgroupZeroIdEndOfGroupDefaultPriority = 0x38 => (Zero, false, true, true, false),
    SubgroupZeroIdExtEndOfGroupDefaultPriority = 0x39 => (Zero, true, true, true, false),
    SubgroupFirstObjectIdEndOfGroupDefaultPriority = 0x3a => (FirstObject, false, true, true, false),
    SubgroupFirstObjectIdExtEndOfGroupDefaultPriority = 0x3b => (FirstObject, true, true, true, false),
    SubgroupIdEndOfGroupDefaultPriority = 0x3c => (Explicit, false, true, true, false),
    SubgroupIdExtEndOfGroupDefaultPriority = 0x3d => (Explicit, true, true, true, false),
    SubgroupZeroIdFirstObject = 0x50 => (Zero, false, false, false, true),
    SubgroupZeroIdExtFirstObject = 0x51 => (Zero, true, false, false, true),
    SubgroupFirstObjectIdFirstObject = 0x52 => (FirstObject, false, false, false, true),
    SubgroupFirstObjectIdExtFirstObject = 0x53 => (FirstObject, true, false, false, true),
    SubgroupIdFirstObject = 0x54 => (Explicit, false, false, false, true),
    SubgroupIdExtFirstObject = 0x55 => (Explicit, true, false, false, true),
    SubgroupZeroIdEndOfGroupFirstObject = 0x58 => (Zero, false, true, false, true),
    SubgroupZeroIdExtEndOfGroupFirstObject = 0x59 => (Zero, true, true, false, true),
    SubgroupFirstObjectIdEndOfGroupFirstObject = 0x5a => (FirstObject, false, true, false, true),
    SubgroupFirstObjectIdExtEndOfGroupFirstObject = 0x5b => (FirstObject, true, true, false, true),
    SubgroupIdEndOfGroupFirstObject = 0x5c => (Explicit, false, true, false, true),
    SubgroupIdExtEndOfGroupFirstObject = 0x5d => (Explicit, true, true, false, true),
    SubgroupZeroIdDefaultPriorityFirstObject = 0x70 => (Zero, false, false, true, true),
    SubgroupZeroIdExtDefaultPriorityFirstObject = 0x71 => (Zero, true, false, true, true),
    SubgroupFirstObjectIdDefaultPriorityFirstObject = 0x72 => (FirstObject, false, false, true, true),
    SubgroupFirstObjectIdExtDefaultPriorityFirstObject = 0x73 => (FirstObject, true, false, true, true),
    SubgroupIdDefaultPriorityFirstObject = 0x74 => (Explicit, false, false, true, true),
    SubgroupIdExtDefaultPriorityFirstObject = 0x75 => (Explicit, true, false, true, true),
    SubgroupZeroIdEndOfGroupDefaultPriorityFirstObject = 0x78 => (Zero, false, true, true, true),
    SubgroupZeroIdExtEndOfGroupDefaultPriorityFirstObject = 0x79 => (Zero, true, true, true, true),
    SubgroupFirstObjectIdEndOfGroupDefaultPriorityFirstObject = 0x7a => (FirstObject, false, true, true, true),
    SubgroupFirstObjectIdExtEndOfGroupDefaultPriorityFirstObject = 0x7b => (FirstObject, true, true, true, true),
    SubgroupIdEndOfGroupDefaultPriorityFirstObject = 0x7c => (Explicit, false, true, true, true),
    SubgroupIdExtEndOfGroupDefaultPriorityFirstObject = 0x7d => (Explicit, true, true, true, true),
}

impl StreamHeaderType {
    const PROPERTIES: u64 = 0x01;
    const SUBGROUP_ID_MODE: u64 = 0x06;
    const END_OF_GROUP: u64 = 0x08;
    const DEFAULT_PRIORITY: u64 = 0x20;
    const FIRST_OBJECT: u64 = 0x40;

    /// Return the stream header's wire value.
    pub const fn value(self) -> u64 {
        self as u64
    }

    /// Returns whether this is a SUBGROUP_HEADER type.
    pub fn is_subgroup(&self) -> bool {
        *self != Self::Fetch
    }

    /// Returns whether this is a FETCH_HEADER type.
    pub fn is_fetch(&self) -> bool {
        *self == Self::Fetch
    }

    /// Returns whether every Object on the stream carries a Properties field.
    pub fn has_properties(&self) -> bool {
        self.is_subgroup() && self.value() & Self::PROPERTIES != 0
    }

    /// Compatibility alias for the properties-bearing Object grammar.
    pub fn has_extension_headers(&self) -> bool {
        self.is_fetch() || self.has_properties()
    }

    /// Returns the encoded Subgroup ID mode, or `None` for a FETCH header.
    pub fn subgroup_id_mode(&self) -> Option<SubgroupIdMode> {
        if !self.is_subgroup() {
            return None;
        }

        match self.value() & Self::SUBGROUP_ID_MODE {
            0x00 => Some(SubgroupIdMode::Zero),
            0x02 => Some(SubgroupIdMode::FirstObject),
            0x04 => Some(SubgroupIdMode::Explicit),
            _ => None,
        }
    }

    /// Returns whether the header carries an explicit Subgroup ID field.
    pub fn has_subgroup_id(&self) -> bool {
        self.subgroup_id_mode() == Some(SubgroupIdMode::Explicit)
    }

    /// Returns whether the first transmitted Object ID supplies the Subgroup ID.
    pub fn uses_first_object_id_as_subgroup_id(&self) -> bool {
        self.subgroup_id_mode() == Some(SubgroupIdMode::FirstObject)
    }

    /// Returns whether FIN identifies the stream's last Object as the Group's largest.
    pub fn end_of_group(&self) -> bool {
        self.is_subgroup() && self.value() & Self::END_OF_GROUP != 0
    }

    /// Returns whether the Publisher Priority field is omitted and inherited.
    pub fn uses_default_priority(&self) -> bool {
        self.is_subgroup() && self.value() & Self::DEFAULT_PRIORITY != 0
    }

    /// Returns whether the stream begins with the first Object published in the Subgroup.
    pub fn begins_with_first_object(&self) -> bool {
        self.is_subgroup() && self.value() & Self::FIRST_OBJECT != 0
    }
}

impl Encode for StreamHeaderType {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        let val = self.value();
        tracing::trace!(
            "[ENCODE] StreamHeaderType: encoding {:?} as {:#x}",
            self,
            val
        );
        val.encode(w)?;
        tracing::trace!("[ENCODE] StreamHeaderType: encoded successfully");
        Ok(())
    }
}

impl Decode for StreamHeaderType {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        tracing::trace!(
            "[DECODE] StreamHeaderType: starting decode, buffer_remaining={} bytes",
            r.remaining()
        );

        let type_value = u64::decode(r)?;
        tracing::trace!(
            "[DECODE] StreamHeaderType: decoded type value={:#x}",
            type_value
        );

        let header_type = if let Some(header_type) = Self::from_value(type_value) {
            Ok(header_type)
        } else {
            tracing::error!(
                "[DECODE] StreamHeaderType: INVALID type value={:#x}",
                type_value
            );
            Err(DecodeError::InvalidHeaderType)
        };

        if let Ok(header_type_inner) = &header_type {
            tracing::trace!(
                "[DECODE] StreamHeaderType: {}, has_subgroup_id={}, has_extension_headers={}",
                header_type_inner,
                header_type_inner.has_subgroup_id(),
                header_type_inner.has_extension_headers()
            );
        }

        header_type
    }
}

impl fmt::Display for StreamHeaderType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} ({:#x})", self, self.value())
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StreamHeader {
    /// Subgroup Header Type
    pub header_type: StreamHeaderType,

    /// Subgroup Header for StreamHeaderTypes that are Subgroup header types
    pub subgroup_header: Option<SubgroupHeader>,

    /// Fetch Header for StreamHeaderTypes that are Fetch header types
    pub fetch_header: Option<FetchHeader>,
}

impl Decode for StreamHeader {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        tracing::trace!(
            "[DECODE] StreamHeader: starting decode, buffer_remaining={} bytes",
            r.remaining()
        );

        let header_type = StreamHeaderType::decode(r)?;
        tracing::trace!(
            "[DECODE] StreamHeader: decoded header_type={:?}",
            header_type
        );

        let subgroup_header = match header_type.is_subgroup() {
            true => {
                tracing::trace!("[DECODE] StreamHeader: decoding subgroup header");
                Some(SubgroupHeader::decode(header_type, r)?)
            }
            false => {
                tracing::trace!("[DECODE] StreamHeader: no subgroup header (not a subgroup type)");
                None
            }
        };

        let fetch_header = match header_type.is_fetch() {
            true => {
                tracing::trace!("[DECODE] StreamHeader: decoding fetch header");
                Some(FetchHeader::decode(header_type, r)?)
            }
            false => {
                tracing::trace!("[DECODE] StreamHeader: no fetch header (not a fetch type)");
                None
            }
        };

        tracing::trace!(
            "[DECODE] StreamHeader complete: type={:?}, has_subgroup={}, has_fetch={}, buffer_remaining={} bytes",
            header_type,
            subgroup_header.is_some(),
            fetch_header.is_some(),
            r.remaining()
        );

        Ok(Self {
            header_type,
            subgroup_header,
            fetch_header,
        })
    }
}

impl Encode for StreamHeader {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        tracing::trace!(
            "[ENCODE] StreamHeader: starting encode for type={:?}, has_subgroup={}, has_fetch={}",
            self.header_type,
            self.subgroup_header.is_some(),
            self.fetch_header.is_some()
        );

        // Note: we are intentionally not encoding the header_type here, it will be encoded in the
        //       appropriate substructures.
        //self.header_type.encode(w)?;
        if self.header_type.is_subgroup() {
            if let Some(subgroup_header) = &self.subgroup_header {
                tracing::trace!("[ENCODE] StreamHeader: encoding subgroup header");
                subgroup_header.encode(w)?;
            } else {
                tracing::error!(
                    "[ENCODE] StreamHeader: MISSING subgroup header for subgroup type={:?}",
                    self.header_type
                );
                return Err(EncodeError::MissingField("SubgroupHeader".to_string()));
            }
        } else if let Some(fetch_header) = &self.fetch_header {
            tracing::trace!("[ENCODE] StreamHeader: encoding fetch header");
            fetch_header.encode(w)?;
        } else {
            tracing::error!(
                "[ENCODE] StreamHeader: MISSING fetch header for fetch type={:?}",
                self.header_type
            );
            return Err(EncodeError::MissingField("FetchHeader".to_string()));
        }

        tracing::trace!("[ENCODE] StreamHeader complete");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use bytes::BytesMut;

    #[test]
    fn encode_decode_stream_header_type() {
        let mut buf = BytesMut::new();

        let ht = StreamHeaderType::Fetch;
        ht.encode(&mut buf).unwrap();
        assert_eq!(buf.to_vec(), vec![0x05]);
        let decoded = StreamHeaderType::decode(&mut buf).unwrap();
        assert_eq!(decoded, ht);
        assert!(ht.is_fetch());
        assert!(!ht.is_subgroup());
        assert!(!ht.has_subgroup_id());

        let ht = StreamHeaderType::SubgroupZeroId;
        ht.encode(&mut buf).unwrap();
        assert_eq!(buf.to_vec(), vec![0x10]);
        let decoded = StreamHeaderType::decode(&mut buf).unwrap();
        assert_eq!(decoded, ht);
        assert!(ht.is_subgroup());
        assert!(!ht.is_fetch());
        assert!(!ht.has_subgroup_id());

        let ht = StreamHeaderType::SubgroupFirstObjectId;
        assert!(ht.uses_first_object_id_as_subgroup_id());

        let ht = StreamHeaderType::SubgroupId;
        assert!(!ht.uses_first_object_id_as_subgroup_id());
    }

    #[test]
    fn decode_bad_stream_header_type() {
        let data: Vec<u8> = vec![0x00]; // Invalid filter type
        let mut buf: Bytes = data.into();
        let result = StreamHeaderType::decode(&mut buf);
        assert!(matches!(result, Err(DecodeError::InvalidHeaderType)));
    }

    #[test]
    fn decodes_every_valid_draft_18_subgroup_header_type() {
        for first_object in [false, true] {
            for default_priority in [false, true] {
                for end_of_group in [false, true] {
                    for has_properties in [false, true] {
                        for subgroup_id_mode in [
                            SubgroupIdMode::Zero,
                            SubgroupIdMode::FirstObject,
                            SubgroupIdMode::Explicit,
                        ] {
                            let expected = StreamHeaderType::subgroup(
                                subgroup_id_mode,
                                has_properties,
                                end_of_group,
                                default_priority,
                                first_object,
                            );
                            let mut buf = BytesMut::new();
                            expected.encode(&mut buf).unwrap();
                            let actual = StreamHeaderType::decode(&mut buf).unwrap();

                            assert_eq!(actual, expected);
                            assert_eq!(actual.subgroup_id_mode(), Some(subgroup_id_mode));
                            assert_eq!(actual.has_properties(), has_properties);
                            assert_eq!(actual.end_of_group(), end_of_group);
                            assert_eq!(actual.uses_default_priority(), default_priority);
                            assert_eq!(actual.begins_with_first_object(), first_object);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_reserved_and_out_of_form_subgroup_header_types() {
        for value in 0_u64..=0x7f {
            let mut buf = BytesMut::new();
            value.encode(&mut buf).unwrap();
            let valid = value == StreamHeaderType::Fetch.value()
                || value & 0x10 != 0 && value & 0x06 != 0x06;
            assert_eq!(
                StreamHeaderType::decode(&mut buf).is_ok(),
                valid,
                "{value:#x}"
            );
        }

        for value in [0x80_u64, 0x90, u64::MAX] {
            let mut buf = BytesMut::new();
            value.encode(&mut buf).unwrap();
            assert!(matches!(
                StreamHeaderType::decode(&mut buf),
                Err(DecodeError::InvalidHeaderType)
            ));
        }
    }

    #[test]
    fn encode_decode_stream_header() {
        let mut buf = BytesMut::new();

        let sh = StreamHeader {
            header_type: StreamHeaderType::Fetch,
            subgroup_header: None,
            fetch_header: Some(FetchHeader {
                header_type: StreamHeaderType::Fetch,
                request_id: 10,
            }),
        };
        sh.encode(&mut buf).unwrap();
        let decoded = StreamHeader::decode(&mut buf).unwrap();
        assert_eq!(decoded, sh);
        assert!(sh.header_type.is_fetch());
        assert!(!sh.header_type.is_subgroup());
        assert!(!sh.header_type.has_subgroup_id());

        let sh = StreamHeader {
            header_type: StreamHeaderType::SubgroupId,
            subgroup_header: Some(SubgroupHeader {
                header_type: StreamHeaderType::SubgroupId,
                track_alias: 10,
                group_id: 0,
                subgroup_id: Some(1),
                publisher_priority: 100,
            }),
            fetch_header: None,
        };
        sh.encode(&mut buf).unwrap();
        let decoded = StreamHeader::decode(&mut buf).unwrap();
        assert_eq!(decoded, sh);
        assert!(sh.header_type.is_subgroup());
        assert!(!sh.header_type.is_fetch());
        assert!(sh.header_type.has_subgroup_id());
    }
}
