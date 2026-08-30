// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::coding::{Decode, DecodeError, Encode, EncodeError};
use crate::data::{ExtensionHeaders, ObjectStatus, StreamHeaderType};

#[derive(Debug, Clone, Copy, Eq, PartialEq, thiserror::Error)]
pub(crate) enum ObjectIdDeltaError {
    #[error("object ID delta overflow")]
    Overflow,

    #[error("object IDs must increase within a subgroup stream")]
    NonMonotonic,
}

/// Decode an Object ID Delta and advance the stream's Object ID state.
pub(crate) fn decode_object_id_delta(
    previous_object_id: &mut Option<u64>,
    object_id_delta: u64,
) -> Result<u64, ObjectIdDeltaError> {
    let object_id = match *previous_object_id {
        Some(previous) => previous
            .checked_add(object_id_delta)
            .and_then(|value| value.checked_add(1))
            .ok_or(ObjectIdDeltaError::Overflow)?,
        None => object_id_delta,
    };
    *previous_object_id = Some(object_id);
    Ok(object_id)
}

/// Encode an absolute Object ID and advance the stream's Object ID state.
pub(crate) fn encode_object_id_delta(
    previous_object_id: &mut Option<u64>,
    object_id: u64,
) -> Result<u64, ObjectIdDeltaError> {
    let object_id_delta = match *previous_object_id {
        Some(previous) => object_id
            .checked_sub(previous)
            .and_then(|difference| difference.checked_sub(1))
            .ok_or(ObjectIdDeltaError::NonMonotonic)?,
        None => object_id,
    };
    *previous_object_id = Some(object_id);
    Ok(object_id_delta)
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SubgroupHeader {
    /// Subgroup Header Type
    pub header_type: StreamHeaderType,

    /// The track alias.
    pub track_alias: u64,

    /// The group sequence number
    pub group_id: u64,

    /// The subgroup sequence number
    pub subgroup_id: Option<u64>,

    /// Publisher priority, where **smaller** values are sent first.
    pub publisher_priority: u8,
}

// Note:  Not using the Decode trait, since we need to know the header_type to properly parse this, and it
//        is read before knowing we need to decode this.
impl SubgroupHeader {
    pub fn decode<R: bytes::Buf>(
        header_type: StreamHeaderType,
        r: &mut R,
    ) -> Result<Self, DecodeError> {
        tracing::trace!(
            "[DECODE] SubgroupHeader: starting decode with header_type={:?}, buffer_remaining={} bytes",
            header_type,
            r.remaining()
        );

        let track_alias = u64::decode(r)?;
        tracing::trace!("[DECODE] SubgroupHeader: track_alias={}", track_alias);

        let group_id = u64::decode(r)?;
        tracing::trace!("[DECODE] SubgroupHeader: group_id={}", group_id);

        let subgroup_id = match header_type.has_subgroup_id() {
            true => {
                let id = u64::decode(r)?;
                tracing::trace!("[DECODE] SubgroupHeader: subgroup_id={}", id);
                Some(id)
            }
            false => {
                tracing::trace!(
                    "[DECODE] SubgroupHeader: subgroup_id=None (not present for this header type)"
                );
                None
            }
        };

        let publisher_priority = u8::decode(r)?;
        tracing::trace!(
            "[DECODE] SubgroupHeader: publisher_priority={}, buffer_remaining={} bytes",
            publisher_priority,
            r.remaining()
        );

        let result = Self {
            header_type,
            track_alias,
            group_id,
            subgroup_id,
            publisher_priority,
        };

        tracing::trace!(
            "[DECODE] SubgroupHeader complete: track_alias={}, group_id={}, subgroup_id={:?}, priority={}",
            result.track_alias,
            result.group_id,
            result.subgroup_id,
            result.publisher_priority
        );

        Ok(result)
    }
}

impl Encode for SubgroupHeader {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        tracing::trace!(
            "[ENCODE] SubgroupHeader: starting encode - track_alias={}, group_id={}, subgroup_id={:?}, priority={}, header_type={:?}",
            self.track_alias,
            self.group_id,
            self.subgroup_id,
            self.publisher_priority,
            self.header_type
        );

        let start_pos = w.remaining_mut();

        self.header_type.encode(w)?;
        tracing::trace!("[ENCODE] SubgroupHeader: encoded header_type");

        self.track_alias.encode(w)?;
        tracing::trace!(
            "[ENCODE] SubgroupHeader: encoded track_alias={}",
            self.track_alias
        );

        self.group_id.encode(w)?;
        tracing::trace!(
            "[ENCODE] SubgroupHeader: encoded group_id={}",
            self.group_id
        );

        if self.header_type.has_subgroup_id() {
            if let Some(subgroup_id) = self.subgroup_id {
                subgroup_id.encode(w)?;
                tracing::trace!(
                    "[ENCODE] SubgroupHeader: encoded subgroup_id={}",
                    subgroup_id
                );
            } else {
                tracing::error!(
                    "[ENCODE] SubgroupHeader: MISSING subgroup_id for header_type={:?}",
                    self.header_type
                );
                return Err(EncodeError::MissingField("SubgroupId".to_string()));
            }
        } else {
            tracing::trace!("[ENCODE] SubgroupHeader: subgroup_id not encoded (not required for this header type)");
        }

        self.publisher_priority.encode(w)?;
        tracing::trace!(
            "[ENCODE] SubgroupHeader: encoded publisher_priority={}",
            self.publisher_priority
        );

        let bytes_written = start_pos - w.remaining_mut();
        tracing::trace!(
            "[ENCODE] SubgroupHeader complete: wrote {} bytes",
            bytes_written
        );

        Ok(())
    }
}

// Subgroup Object without Extension headers (version with ExtensionHeaders is below)
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SubgroupObject {
    pub object_id_delta: u64,
    pub payload_length: usize,
    pub status: Option<ObjectStatus>,
    //pub payload: bytes::Bytes,  // TODO SLG - payload is sent outside this right now - decide which way to go
}

impl Decode for SubgroupObject {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        tracing::trace!(
            "[DECODE] SubgroupObject: starting decode, buffer_remaining={} bytes",
            r.remaining()
        );

        let object_id_delta = u64::decode(r)?;
        tracing::trace!(
            "[DECODE] SubgroupObject: object_id_delta={}",
            object_id_delta
        );

        let payload_length = usize::decode(r)?;
        tracing::trace!("[DECODE] SubgroupObject: payload_length={}", payload_length);

        let status = match payload_length {
            0 => {
                let s = ObjectStatus::decode(r)?;
                tracing::trace!("[DECODE] SubgroupObject: status={:?} (payload_length=0)", s);
                Some(s)
            }
            _ => {
                tracing::trace!("[DECODE] SubgroupObject: status=None (payload_length > 0)");
                None
            }
        };

        //Self::decode_remaining(r, payload_length);
        //let payload = r.copy_to_bytes(payload_length);

        tracing::trace!(
            "[DECODE] SubgroupObject complete: object_id_delta={}, payload_length={}, status={:?}, buffer_remaining={} bytes",
            object_id_delta,
            payload_length,
            status,
            r.remaining()
        );

        Ok(Self {
            object_id_delta,
            payload_length,
            status,
            //payload,
        })
    }
}

impl Encode for SubgroupObject {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        tracing::trace!(
            "[ENCODE] SubgroupObject: starting encode - object_id_delta={}, payload_length={}, status={:?}",
            self.object_id_delta,
            self.payload_length,
            self.status
        );

        self.object_id_delta.encode(w)?;
        tracing::trace!(
            "[ENCODE] SubgroupObject: encoded object_id_delta={}",
            self.object_id_delta
        );

        self.payload_length.encode(w)?;
        tracing::trace!(
            "[ENCODE] SubgroupObject: encoded payload_length={}",
            self.payload_length
        );

        if self.payload_length == 0 {
            if let Some(status) = self.status {
                status.encode(w)?;
                tracing::trace!("[ENCODE] SubgroupObject: encoded status={:?}", status);
            } else {
                tracing::error!("[ENCODE] SubgroupObject: MISSING status for payload_length=0");
                return Err(EncodeError::MissingField("Status".to_string()));
            }
        }
        //Self::encode_remaining(w, self.payload.len())?;
        //w.put_slice(&self.payload);

        tracing::trace!("[ENCODE] SubgroupObject complete");

        Ok(())
    }
}

// Subgroup Object with Extension headers
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SubgroupObjectExt {
    pub object_id_delta: u64,
    pub extension_headers: ExtensionHeaders,
    pub payload_length: usize,
    pub status: Option<ObjectStatus>,
    //pub payload: bytes::Bytes,  // TODO SLG - payload is sent outside this right now - decide which way to go
}

impl Decode for SubgroupObjectExt {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        tracing::trace!(
            "[DECODE] SubgroupObjectExt: starting decode, buffer_remaining={} bytes",
            r.remaining()
        );

        let object_id_delta = u64::decode(r)?;
        tracing::trace!(
            "[DECODE] SubgroupObjectExt: object_id_delta={}",
            object_id_delta
        );

        let extension_headers = ExtensionHeaders::decode(r)?;
        tracing::trace!(
            "[DECODE] SubgroupObjectExt: extension_headers={:?}",
            extension_headers
        );

        let payload_length = usize::decode(r)?;
        tracing::trace!(
            "[DECODE] SubgroupObjectExt: payload_length={}",
            payload_length
        );

        let status = match payload_length {
            0 => {
                let s = ObjectStatus::decode(r)?;
                tracing::trace!(
                    "[DECODE] SubgroupObjectExt: status={:?} (payload_length=0)",
                    s
                );
                Some(s)
            }
            _ => {
                tracing::trace!("[DECODE] SubgroupObjectExt: status=None (payload_length > 0)");
                None
            }
        };

        if status.is_some_and(|status| status != ObjectStatus::NormalObject)
            && !extension_headers.is_empty()
        {
            return Err(DecodeError::InvalidValue);
        }

        //Self::decode_remaining(r, payload_length);
        //let payload = r.copy_to_bytes(payload_length);

        tracing::trace!(
            "[DECODE] SubgroupObjectExt complete: object_id_delta={}, payload_length={}, status={:?}, buffer_remaining={} bytes",
            object_id_delta,
            payload_length,
            status,
            r.remaining()
        );

        Ok(Self {
            object_id_delta,
            extension_headers,
            payload_length,
            status,
            //payload,
        })
    }
}

impl Encode for SubgroupObjectExt {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        tracing::trace!(
            "[ENCODE] SubgroupObjectExt: starting encode - object_id_delta={}, payload_length={}, status={:?}, extension_headers={:?}",
            self.object_id_delta,
            self.payload_length,
            self.status,
            self.extension_headers
        );

        self.object_id_delta.encode(w)?;
        tracing::trace!(
            "[ENCODE] SubgroupObjectExt: encoded object_id_delta={}",
            self.object_id_delta
        );

        self.extension_headers.encode(w)?;
        tracing::trace!("[ENCODE] SubgroupObjectExt: encoded extension_headers");

        self.payload_length.encode(w)?;
        tracing::trace!(
            "[ENCODE] SubgroupObjectExt: encoded payload_length={}",
            self.payload_length
        );

        if self.payload_length == 0 {
            if let Some(status) = self.status {
                if status != ObjectStatus::NormalObject && !self.extension_headers.is_empty() {
                    return Err(EncodeError::InvalidValue);
                }
                status.encode(w)?;
                tracing::trace!("[ENCODE] SubgroupObjectExt: encoded status={:?}", status);
            } else {
                tracing::error!("[ENCODE] SubgroupObjectExt: MISSING status for payload_length=0");
                return Err(EncodeError::MissingField("Status".to_string()));
            }
        }
        //Self::encode_remaining(w, self.payload.len())?;
        //w.put_slice(&self.payload);

        tracing::trace!("[ENCODE] SubgroupObjectExt complete");

        Ok(())
    }
}

// TODO SLG - add more unit tests
#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use bytes::BytesMut;

    #[test]
    fn object_id_delta_round_trips_absolute_first_and_relative_followups() {
        for (object_ids, expected_deltas) in [([0, 1, 2], [0, 0, 0]), ([7, 9, 12], [7, 1, 2])] {
            let mut encode_previous = None;
            let deltas = object_ids
                .map(|object_id| encode_object_id_delta(&mut encode_previous, object_id).unwrap());
            assert_eq!(deltas, expected_deltas);

            let mut decode_previous = None;
            let decoded =
                deltas.map(|delta| decode_object_id_delta(&mut decode_previous, delta).unwrap());
            assert_eq!(decoded, object_ids);
        }
    }

    #[test]
    fn object_id_delta_rejects_overflow_and_non_monotonic_ids() {
        let mut previous = Some(u64::MAX);
        assert_eq!(
            decode_object_id_delta(&mut previous, 0),
            Err(ObjectIdDeltaError::Overflow)
        );
        assert_eq!(previous, Some(u64::MAX));

        for object_id in [9, 8] {
            let mut previous = Some(9);
            assert_eq!(
                encode_object_id_delta(&mut previous, object_id),
                Err(ObjectIdDeltaError::NonMonotonic)
            );
            assert_eq!(previous, Some(9));
        }
    }

    #[test]
    fn filtered_object_ids_use_the_previous_transmitted_object() {
        let mut previous = None;
        let deltas = [0, 1, 5, 6, 9]
            .into_iter()
            .filter(|object_id| *object_id > 4)
            .map(|object_id| encode_object_id_delta(&mut previous, object_id).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(deltas, [5, 0, 2]);
    }

    #[test]
    fn encode_decode_object() {
        let mut buf = BytesMut::new();

        let msg = SubgroupObject {
            object_id_delta: 0,
            payload_length: 7,
            status: None,
        };
        msg.encode(&mut buf).unwrap();
        let decoded = SubgroupObject::decode(&mut buf).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn encode_decode_object_ext() {
        let mut buf = BytesMut::new();

        // One ExtensionHeader for testing
        let mut ext_hdrs = ExtensionHeaders::new();
        ext_hdrs.set_bytesvalue(123, vec![0x00, 0x01, 0x02, 0x03]);

        let msg = SubgroupObjectExt {
            object_id_delta: 0,
            extension_headers: ext_hdrs,
            payload_length: 7,
            status: None,
        };
        msg.encode(&mut buf).unwrap();
        let decoded = SubgroupObjectExt::decode(&mut buf).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn decode_rejects_non_normal_status_with_extension_headers() {
        let data = vec![
            0x00, // object id delta
            0x02, // extension headers byte length
            0x00, // extension delta type
            0x01, // extension value
            0x00, // payload length
            0x04, // EndOfTrack
        ];
        let mut buf: Bytes = data.into();

        assert!(matches!(
            SubgroupObjectExt::decode(&mut buf).unwrap_err(),
            DecodeError::InvalidValue
        ));
    }

    #[test]
    fn encode_rejects_non_normal_status_with_extension_headers() {
        let mut ext_hdrs = ExtensionHeaders::new();
        ext_hdrs.set_intvalue(0, 1);
        let msg = SubgroupObjectExt {
            object_id_delta: 0,
            extension_headers: ext_hdrs,
            payload_length: 0,
            status: Some(ObjectStatus::EndOfTrack),
        };
        let mut buf = BytesMut::new();

        assert!(matches!(
            msg.encode(&mut buf).unwrap_err(),
            EncodeError::InvalidValue
        ));
    }
}
