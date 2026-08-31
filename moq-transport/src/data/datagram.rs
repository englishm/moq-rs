// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::coding::{Decode, DecodeError, Encode, EncodeError};
use crate::data::{ExtensionHeaders, ObjectStatus};

const PROPERTIES: u64 = 0x01;
const END_OF_GROUP: u64 = 0x02;
const ZERO_OBJECT_ID: u64 = 0x04;
const DEFAULT_PRIORITY: u64 = 0x08;
const STATUS: u64 = 0x20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatagramType {
    ObjectIdPayload = 0x00,
    ObjectIdPayloadExt = 0x01,
    ObjectIdPayloadEndOfGroup = 0x02,
    ObjectIdPayloadExtEndOfGroup = 0x03,
    Payload = 0x04,
    PayloadExt = 0x05,
    PayloadEndOfGroup = 0x06,
    PayloadExtEndOfGroup = 0x07,
    ObjectIdPayloadDefaultPriority = 0x08,
    ObjectIdPayloadExtDefaultPriority = 0x09,
    ObjectIdPayloadEndOfGroupDefaultPriority = 0x0a,
    ObjectIdPayloadExtEndOfGroupDefaultPriority = 0x0b,
    PayloadDefaultPriority = 0x0c,
    PayloadExtDefaultPriority = 0x0d,
    PayloadEndOfGroupDefaultPriority = 0x0e,
    PayloadExtEndOfGroupDefaultPriority = 0x0f,
    ObjectIdStatus = 0x20,
    ObjectIdStatusExt = 0x21,
    Status = 0x24,
    StatusExt = 0x25,
    ObjectIdStatusDefaultPriority = 0x28,
    ObjectIdStatusExtDefaultPriority = 0x29,
    StatusDefaultPriority = 0x2c,
    StatusExtDefaultPriority = 0x2d,
}

impl DatagramType {
    pub fn has_properties(self) -> bool {
        self as u64 & PROPERTIES != 0
    }

    pub fn end_of_group(self) -> bool {
        !self.has_status() && self as u64 & END_OF_GROUP != 0
    }

    pub fn has_object_id(self) -> bool {
        self as u64 & ZERO_OBJECT_ID == 0
    }

    pub fn uses_default_priority(self) -> bool {
        self as u64 & DEFAULT_PRIORITY != 0
    }

    pub fn has_status(self) -> bool {
        self as u64 & STATUS != 0
    }
}

impl Decode for DatagramType {
    fn decode<B: bytes::Buf>(r: &mut B) -> Result<Self, DecodeError> {
        match u64::decode(r)? {
            0x00 => Ok(Self::ObjectIdPayload),
            0x01 => Ok(Self::ObjectIdPayloadExt),
            0x02 => Ok(Self::ObjectIdPayloadEndOfGroup),
            0x03 => Ok(Self::ObjectIdPayloadExtEndOfGroup),
            0x04 => Ok(Self::Payload),
            0x05 => Ok(Self::PayloadExt),
            0x06 => Ok(Self::PayloadEndOfGroup),
            0x07 => Ok(Self::PayloadExtEndOfGroup),
            0x08 => Ok(Self::ObjectIdPayloadDefaultPriority),
            0x09 => Ok(Self::ObjectIdPayloadExtDefaultPriority),
            0x0a => Ok(Self::ObjectIdPayloadEndOfGroupDefaultPriority),
            0x0b => Ok(Self::ObjectIdPayloadExtEndOfGroupDefaultPriority),
            0x0c => Ok(Self::PayloadDefaultPriority),
            0x0d => Ok(Self::PayloadExtDefaultPriority),
            0x0e => Ok(Self::PayloadEndOfGroupDefaultPriority),
            0x0f => Ok(Self::PayloadExtEndOfGroupDefaultPriority),
            0x20 => Ok(Self::ObjectIdStatus),
            0x21 => Ok(Self::ObjectIdStatusExt),
            0x24 => Ok(Self::Status),
            0x25 => Ok(Self::StatusExt),
            0x28 => Ok(Self::ObjectIdStatusDefaultPriority),
            0x29 => Ok(Self::ObjectIdStatusExtDefaultPriority),
            0x2c => Ok(Self::StatusDefaultPriority),
            0x2d => Ok(Self::StatusExtDefaultPriority),
            _ => Err(DecodeError::InvalidDatagramType),
        }
    }
}

impl Encode for DatagramType {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        (*self as u64).encode(w)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Datagram {
    pub datagram_type: DatagramType,
    pub track_alias: u64,
    pub group_id: u64,

    /// `None` when ZERO_OBJECT_ID is set; the semantic Object ID is zero.
    pub object_id: Option<u64>,

    /// `None` when DEFAULT_PRIORITY is set; resolve it from Track Properties.
    pub publisher_priority: Option<u8>,
    pub extension_headers: Option<ExtensionHeaders>,
    pub status: Option<ObjectStatus>,
    pub payload: Option<bytes::Bytes>,
}

impl Decode for Datagram {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let datagram_type = DatagramType::decode(r)?;
        let track_alias = u64::decode(r)?;
        let group_id = u64::decode(r)?;
        let object_id = datagram_type
            .has_object_id()
            .then(|| u64::decode(r))
            .transpose()?;
        let publisher_priority = (!datagram_type.uses_default_priority())
            .then(|| u8::decode(r))
            .transpose()?;

        let extension_headers = if datagram_type.has_properties() {
            let headers = ExtensionHeaders::decode(r)?;
            if headers.is_empty() {
                return Err(DecodeError::InvalidValue);
            }
            Some(headers)
        } else {
            None
        };

        let (status, payload) = if datagram_type.has_status() {
            let status = ObjectStatus::decode(r)?;
            if status != ObjectStatus::NormalObject && extension_headers.is_some() {
                return Err(DecodeError::InvalidValue);
            }
            if r.has_remaining() {
                return Err(DecodeError::InvalidValue);
            }
            (Some(status), None)
        } else {
            let payload = r.copy_to_bytes(r.remaining());
            if payload.is_empty() {
                return Err(DecodeError::InvalidValue);
            }
            (None, Some(payload))
        };

        Ok(Self {
            datagram_type,
            track_alias,
            group_id,
            object_id,
            publisher_priority,
            extension_headers,
            status,
            payload,
        })
    }
}

impl Encode for Datagram {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.validate()?;
        self.datagram_type.encode(w)?;
        self.track_alias.encode(w)?;
        self.group_id.encode(w)?;

        if let Some(object_id) = self.object_id {
            object_id.encode(w)?;
        }
        if let Some(priority) = self.publisher_priority {
            priority.encode(w)?;
        }
        if let Some(properties) = &self.extension_headers {
            properties.encode(w)?;
        }
        if let Some(status) = self.status {
            status.encode(w)?;
        }
        if let Some(payload) = &self.payload {
            Self::encode_remaining(w, payload.len())?;
            w.put_slice(payload);
        }

        Ok(())
    }
}

impl Datagram {
    fn validate(&self) -> Result<(), EncodeError> {
        validate_field(
            self.object_id.is_some(),
            self.datagram_type.has_object_id(),
            "ObjectId",
        )?;
        validate_field(
            self.publisher_priority.is_some(),
            !self.datagram_type.uses_default_priority(),
            "PublisherPriority",
        )?;
        validate_field(
            self.extension_headers.is_some(),
            self.datagram_type.has_properties(),
            "Properties",
        )?;
        validate_field(
            self.status.is_some(),
            self.datagram_type.has_status(),
            "Status",
        )?;
        validate_field(
            self.payload.is_some(),
            !self.datagram_type.has_status(),
            "Payload",
        )?;
        if self
            .extension_headers
            .as_ref()
            .is_some_and(ExtensionHeaders::is_empty)
        {
            return Err(EncodeError::InvalidValue);
        }
        if self.payload.as_ref().is_some_and(bytes::Bytes::is_empty) {
            return Err(EncodeError::InvalidValue);
        }
        if self
            .status
            .is_some_and(|status| status != ObjectStatus::NormalObject)
            && self.extension_headers.is_some()
        {
            return Err(EncodeError::InvalidValue);
        }
        Ok(())
    }
}

fn validate_field(present: bool, required: bool, name: &str) -> Result<(), EncodeError> {
    match (present, required) {
        (false, true) => Err(EncodeError::MissingField(name.to_string())),
        (true, false) => Err(EncodeError::InvalidValue),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::{Bytes, BytesMut};

    const VALID_TYPES: &[(u64, DatagramType)] = &[
        (0x00, DatagramType::ObjectIdPayload),
        (0x01, DatagramType::ObjectIdPayloadExt),
        (0x02, DatagramType::ObjectIdPayloadEndOfGroup),
        (0x03, DatagramType::ObjectIdPayloadExtEndOfGroup),
        (0x04, DatagramType::Payload),
        (0x05, DatagramType::PayloadExt),
        (0x06, DatagramType::PayloadEndOfGroup),
        (0x07, DatagramType::PayloadExtEndOfGroup),
        (0x08, DatagramType::ObjectIdPayloadDefaultPriority),
        (0x09, DatagramType::ObjectIdPayloadExtDefaultPriority),
        (0x0a, DatagramType::ObjectIdPayloadEndOfGroupDefaultPriority),
        (
            0x0b,
            DatagramType::ObjectIdPayloadExtEndOfGroupDefaultPriority,
        ),
        (0x0c, DatagramType::PayloadDefaultPriority),
        (0x0d, DatagramType::PayloadExtDefaultPriority),
        (0x0e, DatagramType::PayloadEndOfGroupDefaultPriority),
        (0x0f, DatagramType::PayloadExtEndOfGroupDefaultPriority),
        (0x20, DatagramType::ObjectIdStatus),
        (0x21, DatagramType::ObjectIdStatusExt),
        (0x24, DatagramType::Status),
        (0x25, DatagramType::StatusExt),
        (0x28, DatagramType::ObjectIdStatusDefaultPriority),
        (0x29, DatagramType::ObjectIdStatusExtDefaultPriority),
        (0x2c, DatagramType::StatusDefaultPriority),
        (0x2d, DatagramType::StatusExtDefaultPriority),
    ];

    fn properties() -> ExtensionHeaders {
        let mut properties = ExtensionHeaders::new();
        properties.set_bytesvalue(3, b"property".to_vec());
        properties
    }

    fn datagram(datagram_type: DatagramType) -> Datagram {
        Datagram {
            datagram_type,
            track_alias: 12,
            group_id: 10,
            object_id: datagram_type.has_object_id().then_some(1234),
            publisher_priority: (!datagram_type.uses_default_priority()).then_some(200),
            extension_headers: datagram_type.has_properties().then(properties),
            status: datagram_type
                .has_status()
                .then_some(ObjectStatus::NormalObject),
            payload: (!datagram_type.has_status()).then(|| Bytes::from_static(b"payload")),
        }
    }

    #[test]
    fn classifies_exact_draft_18_type_registry() {
        for code in 0u8..=0x3f {
            let mut encoded = Bytes::from(vec![code]);
            let decoded = DatagramType::decode(&mut encoded);
            let expected = VALID_TYPES
                .iter()
                .find(|(valid, _)| *valid == u64::from(code));
            match expected {
                Some((_, expected)) => assert_eq!(decoded.unwrap(), *expected),
                None => assert!(matches!(decoded, Err(DecodeError::InvalidDatagramType))),
            }
        }
    }

    #[test]
    fn round_trips_every_valid_type_and_field_shape() {
        for (code, datagram_type) in VALID_TYPES {
            let expected = datagram(*datagram_type);
            let mut encoded = BytesMut::new();
            expected.encode(&mut encoded).unwrap();
            assert_eq!(encoded[0], *code as u8);
            assert_eq!(Datagram::decode(&mut encoded).unwrap(), expected);
        }
    }

    #[test]
    fn decodes_zero_id_default_priority_fixture() {
        let mut encoded = Bytes::from_static(&[
            0x0c, // payload, Object ID zero, inherited priority
            0x01, // Track Alias
            0x02, // Group ID
            b't', b'e', b's', b't',
        ]);
        let decoded = Datagram::decode(&mut encoded).unwrap();
        assert_eq!(decoded.datagram_type, DatagramType::PayloadDefaultPriority);
        assert_eq!(decoded.track_alias, 1);
        assert_eq!(decoded.group_id, 2);
        assert_eq!(decoded.object_id, None);
        assert_eq!(decoded.publisher_priority, None);
        assert_eq!(decoded.payload, Some(Bytes::from_static(b"test")));
    }

    #[test]
    fn rejects_all_status_end_of_group_types() {
        for code in [0x22, 0x23, 0x26, 0x27, 0x2a, 0x2b, 0x2e, 0x2f] {
            let mut encoded = Bytes::from(vec![code]);
            assert!(matches!(
                DatagramType::decode(&mut encoded),
                Err(DecodeError::InvalidDatagramType)
            ));
        }
    }

    #[test]
    fn rejects_trailing_bytes_after_status() {
        let mut encoded = Bytes::from_static(&[
            0x20, // status with explicit Object ID and priority
            0x01, // Track Alias
            0x02, // Group ID
            0x03, // Object ID
            0x7f, // Publisher Priority
            0x04, // End of Track
            0xff, // illegal trailing payload
        ]);
        assert!(matches!(
            Datagram::decode(&mut encoded),
            Err(DecodeError::InvalidValue)
        ));
    }

    #[test]
    fn zero_length_normal_object_requires_status_form() {
        let mut payload_form = Bytes::from_static(&[
            0x00, // payload with explicit Object ID and priority
            0x01, // Track Alias
            0x02, // Group ID
            0x03, // Object ID
            0x7f, // Publisher Priority
        ]);
        assert!(matches!(
            Datagram::decode(&mut payload_form),
            Err(DecodeError::InvalidValue)
        ));

        let mut status_form = Bytes::from_static(&[
            0x20, // status with explicit Object ID and priority
            0x01, // Track Alias
            0x02, // Group ID
            0x03, // Object ID
            0x7f, // Publisher Priority
            0x00, // Normal Object
        ]);
        let decoded = Datagram::decode(&mut status_form).unwrap();
        assert_eq!(decoded.status, Some(ObjectStatus::NormalObject));
        assert_eq!(decoded.payload, None);

        let mut empty_payload = datagram(DatagramType::ObjectIdPayloadEndOfGroup);
        empty_payload.payload = Some(Bytes::new());
        assert!(matches!(
            empty_payload.encode(&mut BytesMut::new()),
            Err(EncodeError::InvalidValue)
        ));
    }

    #[test]
    fn rejects_non_normal_status_with_properties() {
        let mut invalid = datagram(DatagramType::ObjectIdStatusExt);
        invalid.status = Some(ObjectStatus::EndOfTrack);
        let mut encoded = BytesMut::new();
        assert!(matches!(
            invalid.encode(&mut encoded),
            Err(EncodeError::InvalidValue)
        ));

        let mut wire = BytesMut::new();
        DatagramType::ObjectIdStatusExt.encode(&mut wire).unwrap();
        1u64.encode(&mut wire).unwrap();
        2u64.encode(&mut wire).unwrap();
        3u64.encode(&mut wire).unwrap();
        127u8.encode(&mut wire).unwrap();
        properties().encode(&mut wire).unwrap();
        ObjectStatus::EndOfTrack.encode(&mut wire).unwrap();
        assert!(matches!(
            Datagram::decode(&mut wire),
            Err(DecodeError::InvalidValue)
        ));
    }

    #[test]
    fn rejects_properties_bit_with_empty_properties() {
        let mut encoded = Bytes::from_static(&[
            0x01, // payload with Properties
            0x01, // Track Alias
            0x02, // Group ID
            0x03, // Object ID
            0x7f, // Publisher Priority
            0x00, // Properties Length
            b'x', // payload
        ]);
        assert!(matches!(
            Datagram::decode(&mut encoded),
            Err(DecodeError::InvalidValue)
        ));
    }

    #[test]
    fn rejects_fields_that_disagree_with_type_bits() {
        let mut encoded = BytesMut::new();

        let mut missing_priority = datagram(DatagramType::ObjectIdPayload);
        missing_priority.publisher_priority = None;
        assert!(matches!(
            missing_priority.encode(&mut encoded),
            Err(EncodeError::MissingField(_))
        ));

        let mut unexpected_priority = datagram(DatagramType::PayloadDefaultPriority);
        unexpected_priority.publisher_priority = Some(1);
        assert!(matches!(
            unexpected_priority.encode(&mut encoded),
            Err(EncodeError::InvalidValue)
        ));

        let mut missing_properties = datagram(DatagramType::ObjectIdPayloadExt);
        missing_properties.extension_headers = None;
        assert!(matches!(
            missing_properties.encode(&mut encoded),
            Err(EncodeError::MissingField(_))
        ));

        let mut empty_properties = datagram(DatagramType::ObjectIdPayloadExt);
        empty_properties.extension_headers = Some(ExtensionHeaders::default());
        assert!(matches!(
            empty_properties.encode(&mut encoded),
            Err(EncodeError::InvalidValue)
        ));
    }
}
