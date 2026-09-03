// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

use bytes::Buf as _;

use crate::coding::{
    Decode, DecodeError, Encode, EncodeError, KeyValuePair, KeyValuePairs, Location, Value,
};
use crate::message::{wire_id, FilterType, GroupOrder};

/// Draft-16 message-parameter type IDs.
pub mod parameter_type {
    pub const DELIVERY_TIMEOUT: u64 = 0x02;
    pub const AUTHORIZATION_TOKEN: u64 = 0x03;
    /// draft-18 §10.2.6. Milliseconds a subscriber will wait for a publisher to
    /// appear. Valid on SUBSCRIBE only, unlike the delivery timeouts.
    pub const RENDEZVOUS_TIMEOUT: u64 = 0x04;
    pub const EXPIRES: u64 = 0x08;
    pub const LARGEST_OBJECT: u64 = 0x09;
    pub const FORWARD: u64 = 0x10;
    pub const SUBSCRIBER_PRIORITY: u64 = 0x20;
    pub const SUBSCRIPTION_FILTER: u64 = 0x21;
    pub const GROUP_ORDER: u64 = 0x22;
    pub const NEW_GROUP_REQUEST: u64 = 0x32;
}

/// Message-parameter type IDs whose value is a single raw `uint8` byte rather
/// than a varint (draft-18 §10.2).
///
/// Draft-17 replaced the generic even=varint / odd=length-prefixed KVP rule for
/// message parameters with per-parameter value kinds. These three are the
/// byte-valued parameters carried by SUBSCRIBE / FETCH. Notably
/// SUBSCRIBER_PRIORITY defaults to 128, which a varint encodes in two bytes —
/// desynchronising the parameter block for peers (imquic, moq-go) that expect a
/// single byte. SUBSCRIPTION_FILTER (0x21) stays length-prefixed bytes and the
/// varint-valued parameters (timeouts, EXPIRES, NEW_GROUP_REQUEST) are
/// unaffected, so they are intentionally absent here.
pub const U8_VALUE_PARAMETER_TYPES: &[u64] = &[
    parameter_type::FORWARD,             // 0x10
    parameter_type::SUBSCRIBER_PRIORITY, // 0x20
    parameter_type::GROUP_ORDER,         // 0x22
];

const TWO_VARINT_VALUE_PARAMETER_TYPES: &[u64] = &[parameter_type::LARGEST_OBJECT];

/// Message scopes enforced for supported parameters with restricted use.
/// Unknown parameter types remain available for negotiated extensions.
const MESSAGE_PARAMETER_SCOPES: &[(u64, &[u64])] =
    &[(parameter_type::RENDEZVOUS_TIMEOUT, &[wire_id::Subscribe])];

/// Draft-16 extension-header type IDs.
pub mod extension_type {
    pub const DELIVERY_TIMEOUT: u64 = 0x02;
    pub const MAX_CACHE_DURATION: u64 = 0x04;
    pub const IMMUTABLE_EXTENSIONS: u64 = 0x0B;
    pub const DEFAULT_PUBLISHER_PRIORITY: u64 = 0x0E;
    pub const DEFAULT_PUBLISHER_GROUP_ORDER: u64 = 0x22;
    pub const DYNAMIC_GROUPS: u64 = 0x30;
    pub const PRIOR_GROUP_ID_GAP: u64 = 0x3C;
    pub const PRIOR_OBJECT_ID_GAP: u64 = 0x3E;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionFilter {
    pub filter_type: FilterType,
    pub start_location: Option<Location>,
    pub end_group_id: Option<u64>,
}

/// Draft-16 Track Extensions are a trailing sequence of KVPs with no count or length prefix.
#[derive(Default, Clone, Debug, Eq, PartialEq)]
pub struct TrackExtensions(pub Vec<KeyValuePair>);

impl TrackExtensions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_extension(&mut self, kvp: KeyValuePair) {
        if let Some(existing) = self.0.iter_mut().find(|k| k.key == kvp.key) {
            *existing = kvp;
        } else {
            self.0.push(kvp);
        }
    }

    pub fn set_int_extension(&mut self, key: u64, value: u64) {
        self.set_extension(KeyValuePair::new_int(key, value));
    }

    pub fn set_bytes_extension(&mut self, key: u64, value: Vec<u8>) {
        self.set_extension(KeyValuePair::new_bytes(key, value));
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn contains_only(&self, supported_types: &[u64]) -> bool {
        self.0
            .iter()
            .all(|extension| supported_types.contains(&extension.key))
    }

    pub fn delivery_timeout(&self) -> Result<Option<u64>, DecodeError> {
        get_kvp_int(&self.0, extension_type::DELIVERY_TIMEOUT)
    }

    pub fn set_delivery_timeout(&mut self, timeout: u64) {
        self.set_int_extension(extension_type::DELIVERY_TIMEOUT, timeout);
    }

    pub fn max_cache_duration(&self) -> Result<Option<u64>, DecodeError> {
        get_kvp_int(&self.0, extension_type::MAX_CACHE_DURATION)
    }

    pub fn set_max_cache_duration(&mut self, duration: u64) {
        self.set_int_extension(extension_type::MAX_CACHE_DURATION, duration);
    }

    pub fn default_publisher_priority(&self) -> Result<Option<u8>, DecodeError> {
        get_kvp_u8(&self.0, extension_type::DEFAULT_PUBLISHER_PRIORITY)
    }

    pub fn set_default_publisher_priority(&mut self, priority: u8) {
        self.set_int_extension(extension_type::DEFAULT_PUBLISHER_PRIORITY, priority.into());
    }

    pub fn default_publisher_group_order(&self) -> Result<Option<GroupOrder>, DecodeError> {
        match get_kvp_int(&self.0, extension_type::DEFAULT_PUBLISHER_GROUP_ORDER)? {
            Some(1) => Ok(Some(GroupOrder::Ascending)),
            Some(2) => Ok(Some(GroupOrder::Descending)),
            Some(_) => Err(DecodeError::InvalidGroupOrder),
            None => Ok(None),
        }
    }

    pub fn set_default_publisher_group_order(
        &mut self,
        group_order: GroupOrder,
    ) -> Result<(), EncodeError> {
        match group_order {
            GroupOrder::Ascending | GroupOrder::Descending => {
                self.set_int_extension(
                    extension_type::DEFAULT_PUBLISHER_GROUP_ORDER,
                    group_order as u64,
                );
                Ok(())
            }
            GroupOrder::Publisher => Err(EncodeError::InvalidValue),
        }
    }

    pub fn dynamic_groups(&self) -> Result<Option<bool>, DecodeError> {
        match get_kvp_int(&self.0, extension_type::DYNAMIC_GROUPS)? {
            Some(0) => Ok(Some(false)),
            Some(1) => Ok(Some(true)),
            Some(_) => Err(DecodeError::InvalidParameter),
            None => Ok(None),
        }
    }

    pub fn set_dynamic_groups(&mut self, enabled: bool) {
        self.set_int_extension(extension_type::DYNAMIC_GROUPS, if enabled { 1 } else { 0 });
    }
}

impl Decode for TrackExtensions {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let mut extensions = Vec::new();
        let mut prev = 0u64;

        while r.has_remaining() {
            let (pair, new_prev) = KeyValuePair::decode_with_prev(r, prev)?;
            prev = new_prev;
            extensions.push(pair);
        }

        Ok(Self(extensions))
    }
}

impl Encode for TrackExtensions {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        let mut sorted: Vec<&KeyValuePair> = self.0.iter().collect();
        sorted.sort_by_key(|k| k.key);

        let mut prev = 0u64;
        for kvp in &sorted {
            prev = kvp.encode_with_prev(w, prev)?;
        }

        Ok(())
    }
}

impl SubscriptionFilter {
    pub fn largest_object() -> Self {
        Self {
            filter_type: FilterType::LargestObject,
            start_location: None,
            end_group_id: None,
        }
    }
}

impl Decode for SubscriptionFilter {
    fn decode<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        let filter_type = FilterType::decode(r)?;

        let (start_location, end_group_id) = match filter_type {
            FilterType::AbsoluteStart => (Some(Location::decode(r)?), None),
            FilterType::AbsoluteRange => (Some(Location::decode(r)?), Some(u64::decode(r)?)),
            FilterType::NextGroupStart | FilterType::LargestObject => (None, None),
        };

        Ok(Self {
            filter_type,
            start_location,
            end_group_id,
        })
    }
}

impl Encode for SubscriptionFilter {
    fn encode<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.filter_type.encode(w)?;

        match self.filter_type {
            FilterType::AbsoluteStart => {
                self.start_location
                    .ok_or_else(|| EncodeError::MissingField("StartLocation".to_string()))?
                    .encode(w)?;
            }
            FilterType::AbsoluteRange => {
                self.start_location
                    .ok_or_else(|| EncodeError::MissingField("StartLocation".to_string()))?
                    .encode(w)?;
                self.end_group_id
                    .ok_or_else(|| EncodeError::MissingField("EndGroupId".to_string()))?
                    .encode(w)?;
            }
            FilterType::NextGroupStart | FilterType::LargestObject => {}
        }

        Ok(())
    }
}

fn encode_bytes_value<T: Encode>(value: &T) -> Result<Vec<u8>, EncodeError> {
    let mut buf = Vec::new();
    value.encode(&mut buf)?;
    Ok(buf)
}

fn decode_bytes_value<T: Decode>(value: &[u8]) -> Result<T, DecodeError> {
    let mut payload = bytes::Bytes::copy_from_slice(value);
    let decoded = T::decode(&mut payload)?;
    if payload.has_remaining() {
        return Err(DecodeError::InvalidParameter);
    }
    Ok(decoded)
}

fn get_kvp_int(pairs: &[KeyValuePair], key: u64) -> Result<Option<u64>, DecodeError> {
    match pairs
        .iter()
        .find(|kvp| kvp.key == key)
        .map(|kvp| &kvp.value)
    {
        Some(Value::IntValue(value)) => Ok(Some(*value)),
        Some(Value::BytesValue(_)) => Err(DecodeError::InvalidParameter),
        None => Ok(None),
    }
}

fn get_kvp_u8(pairs: &[KeyValuePair], key: u64) -> Result<Option<u8>, DecodeError> {
    match get_kvp_int(pairs, key)? {
        Some(value) => u8::try_from(value)
            .map(Some)
            .map_err(|_| DecodeError::InvalidParameter),
        None => Ok(None),
    }
}

impl KeyValuePairs {
    /// Decode a draft-18 §10.2 message-parameter block (SUBSCRIBE, FETCH,
    /// PUBLISH, PUBLISH_OK, REQUEST_OK, REQUEST_UPDATE, …).
    ///
    /// This is the count-prefixed KVP decode with the draft-17 byte-valued
    /// parameter kinds applied uniformly (see [`U8_VALUE_PARAMETER_TYPES`]).
    /// Every message carrying §10.2 parameters MUST use this rather than the
    /// generic [`KeyValuePairs::decode`], otherwise a peer's single-byte
    /// SUBSCRIBER_PRIORITY (default 128) desynchronises the whole block.
    pub fn decode_message_params<R: bytes::Buf>(r: &mut R) -> Result<Self, DecodeError> {
        Self::decode_with_value_types(
            r,
            U8_VALUE_PARAMETER_TYPES,
            TWO_VARINT_VALUE_PARAMETER_TYPES,
        )
    }

    /// Encode a draft-18 §10.2 message-parameter block. Counterpart to
    /// [`decode_message_params`](KeyValuePairs::decode_message_params).
    pub fn encode_message_params<W: bytes::BufMut>(&self, w: &mut W) -> Result<(), EncodeError> {
        self.encode_with_value_types(
            w,
            U8_VALUE_PARAMETER_TYPES,
            TWO_VARINT_VALUE_PARAMETER_TYPES,
        )
    }

    pub(crate) fn parameters_allowed_on(&self, message_type: u64) -> bool {
        if message_type == wire_id::SubscribeTracks {
            return self.0.iter().all(|parameter| {
                matches!(
                    parameter.key,
                    parameter_type::AUTHORIZATION_TOKEN | parameter_type::FORWARD
                )
            });
        }
        self.0.iter().all(|parameter| {
            MESSAGE_PARAMETER_SCOPES
                .iter()
                .find(|(parameter_type, _)| *parameter_type == parameter.key)
                .is_none_or(|(_, message_types)| message_types.contains(&message_type))
        })
    }

    fn int_parameter(&self, key: u64) -> Result<Option<u64>, DecodeError> {
        get_kvp_int(&self.0, key)
    }

    pub fn set_forward(&mut self, forward: bool) {
        self.set_intvalue(parameter_type::FORWARD, if forward { 1 } else { 0 });
    }

    pub fn forward(&self) -> Result<Option<bool>, DecodeError> {
        match self.int_parameter(parameter_type::FORWARD)? {
            Some(0) => Ok(Some(false)),
            Some(1) => Ok(Some(true)),
            Some(_) => Err(DecodeError::InvalidParameter),
            None => Ok(None),
        }
    }

    pub fn set_subscriber_priority(&mut self, priority: u8) {
        self.set_intvalue(parameter_type::SUBSCRIBER_PRIORITY, priority.into());
    }

    pub fn subscriber_priority(&self) -> Result<Option<u8>, DecodeError> {
        get_kvp_u8(&self.0, parameter_type::SUBSCRIBER_PRIORITY)
    }

    pub fn set_group_order(&mut self, group_order: GroupOrder) {
        if group_order != GroupOrder::Publisher {
            self.set_intvalue(parameter_type::GROUP_ORDER, group_order as u64);
        }
    }

    pub fn group_order(&self) -> Result<Option<GroupOrder>, DecodeError> {
        match self.int_parameter(parameter_type::GROUP_ORDER)? {
            Some(0) => Err(DecodeError::InvalidGroupOrder),
            Some(1) => Ok(Some(GroupOrder::Ascending)),
            Some(2) => Ok(Some(GroupOrder::Descending)),
            Some(_) => Err(DecodeError::InvalidGroupOrder),
            None => Ok(None),
        }
    }

    pub fn set_subscription_filter(
        &mut self,
        filter: &SubscriptionFilter,
    ) -> Result<(), EncodeError> {
        self.set_bytesvalue(
            parameter_type::SUBSCRIPTION_FILTER,
            encode_bytes_value(filter)?,
        );
        Ok(())
    }

    pub fn subscription_filter(&self) -> Result<Option<SubscriptionFilter>, DecodeError> {
        match self
            .get(parameter_type::SUBSCRIPTION_FILTER)
            .map(|kvp| &kvp.value)
        {
            Some(Value::BytesValue(value)) => decode_bytes_value(value).map(Some),
            Some(Value::IntValue(_)) => Err(DecodeError::InvalidParameter),
            None => Ok(None),
        }
    }

    pub fn set_largest_object(&mut self, location: Location) -> Result<(), EncodeError> {
        self.set_bytesvalue(
            parameter_type::LARGEST_OBJECT,
            encode_bytes_value(&location)?,
        );
        Ok(())
    }

    pub fn largest_object(&self) -> Result<Option<Location>, DecodeError> {
        match self
            .get(parameter_type::LARGEST_OBJECT)
            .map(|kvp| &kvp.value)
        {
            Some(Value::BytesValue(value)) => decode_bytes_value(value).map(Some),
            Some(Value::IntValue(_)) => Err(DecodeError::InvalidParameter),
            None => Ok(None),
        }
    }

    pub fn set_expires(&mut self, expires: u64) {
        self.set_intvalue(parameter_type::EXPIRES, expires);
    }

    pub fn expires(&self) -> Result<Option<u64>, DecodeError> {
        self.int_parameter(parameter_type::EXPIRES)
    }

    pub fn set_delivery_timeout(&mut self, timeout: u64) {
        self.set_intvalue(parameter_type::DELIVERY_TIMEOUT, timeout);
    }

    pub fn delivery_timeout(&self) -> Result<Option<u64>, DecodeError> {
        self.int_parameter(parameter_type::DELIVERY_TIMEOUT)
    }

    pub fn set_rendezvous_timeout(&mut self, timeout_ms: u64) {
        self.set_intvalue(parameter_type::RENDEZVOUS_TIMEOUT, timeout_ms);
    }

    /// Milliseconds the subscriber will wait for a publisher to become available
    /// (draft-18 §10.2.6). `None` and `Some(0)` are equivalent in effect: the
    /// spec's default is 0, meaning respond immediately rather than hold.
    pub fn rendezvous_timeout(&self) -> Result<Option<u64>, DecodeError> {
        self.int_parameter(parameter_type::RENDEZVOUS_TIMEOUT)
    }

    pub fn set_new_group_request(&mut self, group_id: u64) {
        self.set_intvalue(parameter_type::NEW_GROUP_REQUEST, group_id);
    }

    pub fn new_group_request(&self) -> Result<Option<u64>, DecodeError> {
        self.int_parameter(parameter_type::NEW_GROUP_REQUEST)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn key_value_pairs_message_parameter_methods_round_trip_typed_values() {
        let mut params = KeyValuePairs::default();
        let filter = SubscriptionFilter {
            filter_type: FilterType::AbsoluteRange,
            start_location: Some(Location::new(1, 2)),
            end_group_id: Some(3),
        };

        params.set_forward(false);
        params.set_subscriber_priority(7);
        params.set_group_order(GroupOrder::Descending);
        params.set_subscription_filter(&filter).unwrap();
        params.set_largest_object(Location::new(4, 5)).unwrap();
        params.set_expires(6);
        params.set_delivery_timeout(8);
        params.set_new_group_request(9);
        params.set_rendezvous_timeout(30_000);

        assert_eq!(params.forward().unwrap(), Some(false));
        assert_eq!(params.subscriber_priority().unwrap(), Some(7));
        assert_eq!(params.group_order().unwrap(), Some(GroupOrder::Descending));
        assert_eq!(params.subscription_filter().unwrap(), Some(filter));
        assert_eq!(params.largest_object().unwrap(), Some(Location::new(4, 5)));
        assert_eq!(params.expires().unwrap(), Some(6));
        assert_eq!(params.delivery_timeout().unwrap(), Some(8));
        assert_eq!(params.new_group_request().unwrap(), Some(9));
        assert_eq!(params.rendezvous_timeout().unwrap(), Some(30_000));
    }

    /// 0x04 is even, so the KVP codec carries it as a varint. A peer sending a
    /// byte string under that key is malformed rather than something to coerce.
    #[test]
    fn rendezvous_timeout_rejects_a_bytes_value() {
        let mut params = KeyValuePairs::default();
        params.set_bytesvalue(parameter_type::RENDEZVOUS_TIMEOUT, vec![1, 2, 3]);

        assert!(params.rendezvous_timeout().is_err());
    }

    /// Absent means the spec default of 0: do not hold, answer immediately.
    #[test]
    fn rendezvous_timeout_absent_is_none() {
        assert_eq!(KeyValuePairs::default().rendezvous_timeout().unwrap(), None);
    }

    #[test]
    fn largest_object_uses_two_inline_varints() {
        let mut params = KeyValuePairs::default();
        params.set_largest_object(Location::new(4, 5)).unwrap();
        let mut encoded = BytesMut::new();

        params.encode_message_params(&mut encoded).unwrap();

        assert_eq!(
            encoded.as_ref(),
            &[1, parameter_type::LARGEST_OBJECT as u8, 4, 5]
        );
        assert_eq!(
            KeyValuePairs::decode_message_params(&mut encoded)
                .unwrap()
                .largest_object()
                .unwrap(),
            Some(Location::new(4, 5))
        );
    }

    #[test]
    fn track_extensions_methods_round_trip_typed_values() {
        let mut extensions = TrackExtensions::default();

        extensions.set_delivery_timeout(10);
        extensions.set_max_cache_duration(20);
        extensions.set_default_publisher_priority(30);
        extensions
            .set_default_publisher_group_order(GroupOrder::Ascending)
            .unwrap();
        extensions.set_dynamic_groups(true);

        assert_eq!(extensions.delivery_timeout().unwrap(), Some(10));
        assert_eq!(extensions.max_cache_duration().unwrap(), Some(20));
        assert_eq!(extensions.default_publisher_priority().unwrap(), Some(30));
        assert_eq!(
            extensions.default_publisher_group_order().unwrap(),
            Some(GroupOrder::Ascending)
        );
        assert_eq!(extensions.dynamic_groups().unwrap(), Some(true));
    }

    #[test]
    fn track_extensions_rejects_publisher_group_order_sentinel() {
        let mut extensions = TrackExtensions::default();

        assert!(matches!(
            extensions.set_default_publisher_group_order(GroupOrder::Publisher),
            Err(EncodeError::InvalidValue)
        ));
    }

    #[test]
    fn track_extensions_contains_only_supported_types() {
        let mut extensions = TrackExtensions::default();
        let supported = [extension_type::DEFAULT_PUBLISHER_PRIORITY];

        assert!(extensions.contains_only(&supported));
        extensions.set_default_publisher_priority(30);
        assert!(extensions.contains_only(&supported));
        extensions.set_delivery_timeout(10);
        assert!(!extensions.contains_only(&supported));
    }
}
