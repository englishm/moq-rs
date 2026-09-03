// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{coding, serve};

#[derive(thiserror::Error, Debug, Clone)]
pub enum SessionError {
    #[error("webtransport error: {0}")]
    WebTransport(#[from] web_transport::Error),

    #[error("encode error: {0}")]
    Encode(#[from] coding::EncodeError),

    #[error("decode error: {0}")]
    Decode(#[from] coding::DecodeError),

    /// TODO SLG - eventually remove or morph into error for incorrect control message for publisher/subscriber
    /// The role negiotiated in the handshake was violated. For example, a publisher sent a SUBSCRIBE, or a subscriber sent an OBJECT.
    #[error("role violation")]
    RoleViolation,

    /// Some VarInt was too large and we were too lazy to handle it
    #[error("varint bounds exceeded")]
    BoundsExceeded(#[from] coding::BoundsExceeded),

    /// A duplicate ID was used
    #[error("duplicate")]
    Duplicate,

    #[error("internal error")]
    Internal,

    #[error("serve error: {0}")]
    Serve(#[from] serve::ServeError),

    #[error("wrong size")]
    WrongSize,

    #[error("invalid connection path: {0}")]
    InvalidPath(String),

    /// Draft-16 §3.4 INVALID_REQUEST_ID (0x4): peer used an invalid request ID.
    #[error("invalid request ID")]
    InvalidRequestId,

    /// Draft-16 §3.4 TOO_MANY_REQUESTS (0x7): request ID meets or exceeds the maximum.
    #[error("too many requests")]
    TooManyRequests,

    /// Draft-16 §3.4 PROTOCOL_VIOLATION (0x3): peer violated a MUST rule.
    #[error("protocol violation: {0}")]
    ProtocolViolation(String),
}

// Session Termination Error Codes from draft-ietf-moq-transport-14 Section 13.1.1
impl SessionError {
    /// An integer code that is sent over the wire.
    /// Returns Session Termination Error Codes per draft-14.
    pub fn code(&self) -> u64 {
        match self {
            // PROTOCOL_VIOLATION (0x3) - The role negotiated in the handshake was violated
            Self::RoleViolation => 0x3,
            // INTERNAL_ERROR (0x1) - Generic internal errors
            Self::WebTransport(_) => 0x1,
            Self::Encode(_) => 0x1,
            Self::BoundsExceeded(_) => 0x1,
            Self::Internal => 0x1,
            // PROTOCOL_VIOLATION (0x3) - Malformed messages
            Self::Decode(_) => 0x3,
            Self::WrongSize => 0x3,
            Self::InvalidPath(_) => 0x3,
            // DUPLICATE_TRACK_ALIAS (0x5)
            Self::Duplicate => 0x5,
            // INVALID_REQUEST_ID (0x4)
            Self::InvalidRequestId => 0x4,
            // TOO_MANY_REQUESTS (0x7)
            Self::TooManyRequests => 0x7,
            // PROTOCOL_VIOLATION (0x3)
            Self::ProtocolViolation(_) => 0x3,
            // Delegate to ServeError for per-request error codes
            Self::Serve(err) => err.code(),
        }
    }

    /// Helper for unimplemented protocol features
    /// Logs a warning and returns a NotImplemented error instead of panicking
    pub fn unimplemented(feature: &str) -> Self {
        Self::Serve(serve::ServeError::not_implemented_ctx(feature))
    }

    /// Returns true if this error represents a graceful connection close.
    ///
    /// For WebTransport, a graceful close is a `CLOSE_WEBTRANSPORT_SESSION` capsule
    /// with code 0. For raw QUIC, it's `APPLICATION_CLOSE` with code 0 (NO_ERROR).
    /// Both are normal session termination, not error conditions.
    ///
    /// This method checks for:
    /// - WebTransport `Closed(0, _)` — web-transport-quinn v0.11+ typically converts
    ///   HTTP/3-encoded `ApplicationClosed` codes into `WebTransportError::Closed(code, reason)`
    ///   during `SessionError` conversion when decoding via `error_from_http3` succeeds
    /// - Raw QUIC `ApplicationClosed` with code 0
    /// - The local side closing the connection (`LocallyClosed`)
    ///
    /// ## Implementation Notes
    ///
    /// We pattern match on `web_transport_quinn::SessionError` variants. In v0.11+,
    /// WebTransport graceful closes arrive as `WebTransportError::Closed(0, _)` because
    /// the crate decodes HTTP/3 error codes at the `SessionError` level. For raw QUIC
    /// connections, the close code is checked directly on `ConnectionError::ApplicationClosed`.
    ///
    /// **Coupling note**: This implementation is coupled to `web-transport-quinn` and
    /// `quinn`. When transitioning to a different WebTransport backend (e.g., tokio-quiche),
    /// ensure the replacement provides equivalent error introspection, or update this
    /// method to handle the new error types.
    pub fn is_graceful_close(&self) -> bool {
        match self {
            Self::WebTransport(wt_err) => match wt_err {
                web_transport::Error::Session(session_err) => {
                    is_session_error_graceful(session_err)
                }
                web_transport::Error::Read(read_err) => {
                    if let web_transport::quinn::ReadError::SessionError(session_err) = read_err {
                        return is_session_error_graceful(session_err);
                    }
                    false
                }
                web_transport::Error::Write(write_err) => {
                    if let web_transport::quinn::WriteError::SessionError(session_err) = write_err {
                        return is_session_error_graceful(session_err);
                    }
                    false
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// Returns true when only one WebTransport stream was closed or reset.
    /// True when this error means the peer's send half ended, cleanly or
    /// otherwise, rather than that a message was malformed.
    ///
    /// Callers reading an open-ended stream use this to tell "nothing more is
    /// coming" from "that frame was bad but the stream is still in sync".
    pub(crate) fn is_stream_ended(&self) -> bool {
        self.is_stream_error() || matches!(self, Self::Decode(crate::coding::DecodeError::More(_)))
    }

    /// Whether this error requires tearing down the whole session, rather than
    /// only ending one stream or failing the one request it arose from.
    ///
    /// Draft-18 makes session closure mandatory for protocol violations:
    /// duplicate track aliases, invalid request IDs, malformed messages. Two
    /// classes must *not* take the connection down with them, because both are
    /// ordinary events on a healthy session: a peer ending or resetting a single
    /// request stream, and a request that legitimately fails (an unknown track,
    /// say). Collapsing those into "close the session" would let any subscriber
    /// kill the connection by asking for something that does not exist.
    pub fn is_session_fatal(&self) -> bool {
        if self.is_stream_ended() {
            return false;
        }
        !matches!(self, Self::Serve(_))
    }

    pub(crate) fn is_stream_error(&self) -> bool {
        #[cfg(any(not(target_arch = "wasm32"), target_os = "wasi"))]
        {
            matches!(
                self,
                Self::WebTransport(
                    web_transport::Error::Write(
                        web_transport::quinn::WriteError::Stopped(_)
                            | web_transport::quinn::WriteError::ClosedStream
                    ) | web_transport::Error::Read(
                        web_transport::quinn::ReadError::Reset(_)
                            | web_transport::quinn::ReadError::ClosedStream
                    )
                )
            )
        }

        #[cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]
        {
            matches!(self, Self::WebTransport(web_transport::Error::Stream(_)))
        }
    }
}

impl From<SessionError> for serve::ServeError {
    fn from(err: SessionError) -> Self {
        match err {
            SessionError::Serve(err) => err,
            _ => serve::ServeError::internal_ctx(format!("session error: {}", err)),
        }
    }
}

/// Helper to check if a `web_transport_quinn::SessionError` represents a graceful close.
///
/// This handles:
/// - WebTransport connections: `WebTransportError::Closed(0, _)` — web-transport-quinn v0.11+
///   typically decodes HTTP/3-encoded close codes at this layer (when `SessionError` conversion
///   applies), so graceful closes usually arrive here rather than as a raw
///   `ConnectionError::ApplicationClosed`.
/// - Raw QUIC connections: `ConnectionError::ApplicationClosed` with code 0
/// - Local close: `ConnectionError::LocallyClosed`
fn is_session_error_graceful(err: &web_transport::quinn::SessionError) -> bool {
    use web_transport::quinn::{SessionError, WebTransportError};

    match err {
        SessionError::ConnectionError(conn_err) => is_connection_error_graceful(conn_err),
        // WebTransport graceful close: peer sent close with code 0
        SessionError::WebTransportError(WebTransportError::Closed(0, _)) => true,
        // Other WebTransport errors (UnknownSession, read/write errors, non-zero close codes)
        SessionError::WebTransportError(_) => false,
        // SendDatagramError doesn't represent connection close
        SessionError::SendDatagramError(_) => false,
    }
}

/// Helper to check if a `quinn::ConnectionError` represents a graceful close.
///
/// Note: In web-transport-quinn v0.11+, WebTransport `ApplicationClosed` with an HTTP/3-encoded
/// close code is usually converted to `WebTransportError::Closed` during `SessionError` conversion
/// when decoding succeeds. This function primarily handles raw QUIC (moqt:// ALPN) connections
/// or non-decodable cases where the close code is not HTTP/3 encoded.
fn is_connection_error_graceful(err: &web_transport::quinn::quinn::ConnectionError) -> bool {
    use web_transport::quinn::quinn::ConnectionError;

    match err {
        ConnectionError::ApplicationClosed(close) => {
            let code = close.error_code.into_inner();

            // Check for raw QUIC code 0 (direct MoQ-over-QUIC)
            if code == 0 {
                return true;
            }

            // Check for WebTransport code 0 (HTTP/3 encoded)
            // This is a fallback — in v0.11+, WebTransport closes are typically caught
            // by is_session_error_graceful's WebTransportError::Closed branch.
            if let Some(wt_code) = web_transport::quinn::proto::error_from_http3(code) {
                return wt_code == 0;
            }

            false
        }
        // LocallyClosed means we closed the connection ourselves
        ConnectionError::LocallyClosed => true,
        // Other errors are not graceful closes
        _ => false,
    }
}

#[cfg(all(test, any(not(target_arch = "wasm32"), target_os = "wasi")))]
mod tests {
    use super::*;

    #[test]
    fn stream_errors_are_not_session_errors() {
        let reset = SessionError::WebTransport(web_transport::Error::Read(
            web_transport::quinn::ReadError::Reset(42),
        ));
        let stopped = SessionError::WebTransport(web_transport::Error::Write(
            web_transport::quinn::WriteError::Stopped(42),
        ));

        assert!(reset.is_stream_error());
        assert!(stopped.is_stream_error());
        assert!(!SessionError::Internal.is_stream_error());
    }

    #[test]
    fn protocol_violations_are_session_fatal() {
        for err in [
            SessionError::Duplicate,
            SessionError::InvalidRequestId,
            SessionError::ProtocolViolation("bad".into()),
            SessionError::RoleViolation,
            SessionError::WrongSize,
        ] {
            assert!(err.is_session_fatal(), "{err} should close the session");
        }
    }

    #[test]
    fn one_stream_ending_does_not_close_the_session() {
        let reset = SessionError::WebTransport(web_transport::Error::Read(
            web_transport::quinn::ReadError::Reset(42),
        ));
        let ended = SessionError::Decode(crate::coding::DecodeError::More(1));

        assert!(!reset.is_session_fatal());
        assert!(!ended.is_session_fatal());
    }

    #[test]
    fn a_failed_request_does_not_close_the_session() {
        // Otherwise any subscriber could drop the connection for everyone by
        // asking for a track that does not exist.
        let not_found = SessionError::Serve(serve::ServeError::NotFound);
        assert!(!not_found.is_session_fatal());
    }
}
