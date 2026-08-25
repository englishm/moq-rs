// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Request ID flow control per draft-ietf-moq-transport-16 §9.1.
//!
//! This module intentionally exposes one cloneable session-level handle,
//! [`RequestId`]. Internally it owns two independent states under one shared
//! allocation:
//!
//! - `send`: outbound request ID allocation and peer-advertised max.
//! - `recv`: inbound request ID validation and our advertised max.
//!
//! The two states use separate mutexes. That keeps outbound allocation from
//! blocking inbound validation while still keeping all request-ID state tied to
//! one session handle.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use crate::coding::KeyValuePairs;
use crate::message::{MaxRequestId, RequestsBlocked};
use crate::session::{SessionError, SessionId};

#[derive(Clone, Debug)]
pub struct RequestId {
    inner: Arc<RequestIdInner>,
}

#[derive(Debug)]
struct RequestIdInner {
    session_id: SessionId,
    send: Mutex<SendState>,
    recv: Mutex<RecvState>,
}

#[derive(Debug)]
struct SendState {
    next: u64,
    peer_max: u64,
    blocked_sent_for: Option<u64>,
}

#[derive(Debug)]
struct RecvState {
    first: u64,
    low_water: u64,
    /// Out-of-order request IDs received at or above `low_water` but not yet
    /// contiguous with it. This is a sliding reassembly window, not an unbounded
    /// set: `validate_incoming` rejects any `id >= our_max`, so every stored ID
    /// lies in `[low_water, our_max)` and the set size is bounded by that window
    /// (`our_max - low_water`, i.e. the peer's outstanding request lookahead).
    received_above_low_water: HashSet<u64>,
    our_max: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub enum RequestIdAllocation {
    Allocated(u64),
    Blocked {
        max_request_id: u64,
        should_send_requests_blocked: bool,
    },
}

impl RequestIdAllocation {
    /// Return a REQUESTS_BLOCKED message if this allocation should emit one.
    pub fn requests_blocked(&self) -> Option<RequestsBlocked> {
        match self {
            Self::Blocked {
                max_request_id,
                should_send_requests_blocked: true,
            } => Some(RequestsBlocked {
                max_request_id: *max_request_id,
            }),
            _ => None,
        }
    }
}

impl RequestId {
    /// Create a request-ID manager for one endpoint in a session.
    ///
    /// `local_first_id` is 0 for clients and 1 for servers.
    /// `peer_max` is the peer-advertised MAX_REQUEST_ID from setup.
    /// `our_max` is the MAX_REQUEST_ID we advertised in setup.
    /// `peer_first_id` is 0 if the peer is client, 1 if the peer is server.
    pub fn new(local_first_id: u64, peer_max: u64, our_max: u64, peer_first_id: u64) -> Self {
        // TODO(itzmanish): When SessionId becomes mandatory in the next breaking API, make
        // `new` accept it and remove `new_with_session_id`.
        Self::new_with_session_id(
            SessionId::generate(),
            local_first_id,
            peer_max,
            our_max,
            peer_first_id,
        )
    }

    /// Create a request-ID manager with an explicit session correlation ID.
    ///
    /// `session_id` comes first, followed by the same endpoint ID and limit arguments as
    /// [`Self::new`]. Transport sessions should pass their peer-observed QUIC connection ID.
    pub fn new_with_session_id(
        session_id: SessionId,
        local_first_id: u64,
        peer_max: u64,
        our_max: u64,
        peer_first_id: u64,
    ) -> Self {
        Self {
            inner: Arc::new(RequestIdInner {
                session_id,
                send: Mutex::new(SendState {
                    next: local_first_id,
                    peer_max,
                    blocked_sent_for: None,
                }),
                recv: Mutex::new(RecvState {
                    first: peer_first_id,
                    low_water: peer_first_id,
                    received_above_low_water: HashSet::new(),
                    our_max,
                }),
            }),
        }
    }

    /// Allocate the next outbound request ID.
    ///
    /// If the peer-advertised budget is exhausted, returns `Blocked` with
    /// `should_send_requests_blocked=true` once per max value.
    pub fn allocate(&self) -> Result<RequestIdAllocation, SessionError> {
        let mut send = self.inner.send.lock().map_err(|_| SessionError::Internal)?;

        if send.next >= send.peer_max {
            let should_send_requests_blocked = if send.blocked_sent_for == Some(send.peer_max) {
                false
            } else {
                send.blocked_sent_for = Some(send.peer_max);
                true
            };

            return Ok(RequestIdAllocation::Blocked {
                max_request_id: send.peer_max,
                should_send_requests_blocked,
            });
        }

        let id = send.next;
        send.next = send
            .next
            .checked_add(2)
            .ok_or(SessionError::TooManyRequests)?;
        send.blocked_sent_for = None;
        Ok(RequestIdAllocation::Allocated(id))
    }

    /// Apply a peer MAX_REQUEST_ID update to the outbound allocator.
    pub fn apply_max_request_id(&self, msg: &MaxRequestId) -> Result<(), SessionError> {
        let mut send = self.inner.send.lock().map_err(|_| SessionError::Internal)?;

        if msg.request_id <= send.peer_max {
            return Err(SessionError::ProtocolViolation(
                "MAX_REQUEST_ID must be strictly increasing".to_string(),
            ));
        }

        send.peer_max = msg.request_id;
        send.blocked_sent_for = None;
        Ok(())
    }

    /// Validate an incoming new request ID from the peer.
    pub fn validate_incoming(&self, id: u64) -> Result<(), SessionError> {
        let mut recv = self.inner.recv.lock().map_err(|_| SessionError::Internal)?;

        if id >= recv.our_max {
            return Err(SessionError::TooManyRequests);
        }

        if id < recv.first || id % 2 != recv.first % 2 || id < recv.low_water {
            return Err(SessionError::InvalidRequestId);
        }

        if !recv.received_above_low_water.insert(id) {
            return Err(SessionError::InvalidRequestId);
        }

        while {
            let low_water = recv.low_water;
            recv.received_above_low_water.remove(&low_water)
        } {
            recv.low_water = recv
                .low_water
                .checked_add(2)
                .ok_or(SessionError::InvalidRequestId)?;
        }

        Ok(())
    }

    /// Handle REQUESTS_BLOCKED from the peer.
    ///
    /// If the peer has consumed our current advertised maximum and reports that
    /// same maximum as blocked, we currently ignore this. In the future, we may
    /// advertise new incremented MAX_REQUEST_ID.
    pub fn handle_requests_blocked(&self, msg: &RequestsBlocked) -> Result<(), SessionError> {
        let recv = self.inner.recv.lock().map_err(|_| SessionError::Internal)?;
        tracing::warn!(
            session_id = %self.inner.session_id,
            "got requests blocked, peer max: {}, configured limit: {}, limit hit: {}, ignoring it",
            msg.max_request_id,
            recv.our_max,
            msg.max_request_id == recv.our_max
        );

        Ok(())
    }
}

/// Extract the MAX_REQUEST_ID value from setup parameters (0 if absent).
pub fn max_request_id_from_params(params: &KeyValuePairs) -> u64 {
    use crate::coding::Value;
    use crate::setup::ParameterType;

    params
        .get(ParameterType::MaxRequestId.into())
        .and_then(|kvp| match &kvp.value {
            Value::IntValue(v) => Some(*v),
            _ => None,
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::ParameterType;

    fn client_ids(peer_max: u64, our_max: u64) -> RequestId {
        // local client sends even, peer server sends odd
        RequestId::new(0, peer_max, our_max, 1)
    }

    fn server_ids(peer_max: u64, our_max: u64) -> RequestId {
        // local server sends odd, peer client sends even
        RequestId::new(1, peer_max, our_max, 0)
    }

    #[test]
    fn max_request_id_from_params_uses_draft_default_zero() {
        let params = KeyValuePairs::default();

        assert_eq!(max_request_id_from_params(&params), 0);
    }

    #[test]
    fn max_request_id_from_params_reads_setup_value() {
        let mut params = KeyValuePairs::default();
        params.set_intvalue(ParameterType::MaxRequestId.into(), 42);

        assert_eq!(max_request_id_from_params(&params), 42);
    }

    #[test]
    fn client_allocates_even_ids() {
        let ids = client_ids(10, 10);
        assert_eq!(ids.allocate().unwrap(), RequestIdAllocation::Allocated(0));
        assert_eq!(ids.allocate().unwrap(), RequestIdAllocation::Allocated(2));
        assert_eq!(ids.allocate().unwrap(), RequestIdAllocation::Allocated(4));
    }

    #[test]
    fn server_allocates_odd_ids() {
        let ids = server_ids(10, 10);
        assert_eq!(ids.allocate().unwrap(), RequestIdAllocation::Allocated(1));
        assert_eq!(ids.allocate().unwrap(), RequestIdAllocation::Allocated(3));
        assert_eq!(ids.allocate().unwrap(), RequestIdAllocation::Allocated(5));
    }

    #[test]
    fn allocation_blocks_at_peer_max() {
        let ids = client_ids(4, 10);
        assert_eq!(ids.allocate().unwrap(), RequestIdAllocation::Allocated(0));
        assert_eq!(ids.allocate().unwrap(), RequestIdAllocation::Allocated(2));
        assert_eq!(
            ids.allocate().unwrap(),
            RequestIdAllocation::Blocked {
                max_request_id: 4,
                should_send_requests_blocked: true,
            }
        );
    }

    #[test]
    fn requests_blocked_is_stable_for_same_limit() {
        let ids = client_ids(2, 10);
        assert_eq!(ids.allocate().unwrap(), RequestIdAllocation::Allocated(0));
        let first_block = ids.allocate().unwrap();
        assert_eq!(
            first_block,
            RequestIdAllocation::Blocked {
                max_request_id: 2,
                should_send_requests_blocked: true,
            }
        );
        assert_eq!(first_block.requests_blocked().unwrap().max_request_id, 2);

        let second_block = ids.allocate().unwrap();
        assert_eq!(
            second_block,
            RequestIdAllocation::Blocked {
                max_request_id: 2,
                should_send_requests_blocked: false,
            }
        );
        assert!(second_block.requests_blocked().is_none());
    }

    #[test]
    fn max_request_id_must_increase() {
        let ids = client_ids(10, 10);
        assert!(matches!(
            ids.apply_max_request_id(&MaxRequestId { request_id: 10 })
                .unwrap_err(),
            SessionError::ProtocolViolation(_)
        ));
        assert!(matches!(
            ids.apply_max_request_id(&MaxRequestId { request_id: 9 })
                .unwrap_err(),
            SessionError::ProtocolViolation(_)
        ));
    }

    #[test]
    fn max_request_id_increases_allocation_budget() {
        let ids = client_ids(2, 10);
        assert_eq!(ids.allocate().unwrap(), RequestIdAllocation::Allocated(0));
        assert!(matches!(
            ids.allocate().unwrap(),
            RequestIdAllocation::Blocked { .. }
        ));
        ids.apply_max_request_id(&MaxRequestId { request_id: 10 })
            .unwrap();
        assert_eq!(ids.allocate().unwrap(), RequestIdAllocation::Allocated(2));
        assert_eq!(ids.allocate().unwrap(), RequestIdAllocation::Allocated(4));
    }

    #[test]
    fn validates_client_peer_sequence() {
        let ids = server_ids(10, 10);
        ids.validate_incoming(0).unwrap();
        ids.validate_incoming(2).unwrap();
        ids.validate_incoming(4).unwrap();
    }

    #[test]
    fn validates_server_peer_sequence() {
        let ids = client_ids(10, 10);
        ids.validate_incoming(1).unwrap();
        ids.validate_incoming(3).unwrap();
        ids.validate_incoming(5).unwrap();
    }

    #[test]
    fn rejects_wrong_first_id() {
        let ids = server_ids(10, 10);
        assert!(matches!(
            ids.validate_incoming(1).unwrap_err(),
            SessionError::InvalidRequestId
        ));
    }

    #[test]
    fn accepts_cross_stream_out_of_order_ids() {
        let ids = server_ids(10, 10);
        ids.validate_incoming(4).unwrap();
        ids.validate_incoming(0).unwrap();
        ids.validate_incoming(2).unwrap();
        ids.validate_incoming(6).unwrap();
    }

    #[test]
    fn rejects_repeated_id() {
        let ids = server_ids(10, 10);
        ids.validate_incoming(4).unwrap();
        ids.validate_incoming(0).unwrap();
        ids.validate_incoming(2).unwrap();
        assert!(matches!(
            ids.validate_incoming(4).unwrap_err(),
            SessionError::InvalidRequestId
        ));
    }

    #[test]
    fn rejects_id_at_our_max() {
        let ids = server_ids(10, 4);
        ids.validate_incoming(0).unwrap();
        ids.validate_incoming(2).unwrap();
        assert!(matches!(
            ids.validate_incoming(4).unwrap_err(),
            SessionError::TooManyRequests
        ));
    }

    #[test]
    fn send_and_receive_state_do_not_block_each_other() {
        let ids = client_ids(10, 10);
        let send_ids = ids.clone();
        let recv_ids = ids.clone();

        assert_eq!(
            send_ids.allocate().unwrap(),
            RequestIdAllocation::Allocated(0)
        );
        recv_ids.validate_incoming(1).unwrap();
        assert_eq!(
            send_ids.allocate().unwrap(),
            RequestIdAllocation::Allocated(2)
        );
        recv_ids.validate_incoming(3).unwrap();
    }
}
