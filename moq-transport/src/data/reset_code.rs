// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Draft-16 §13.4.4 Data Stream Reset Error Codes.
///
/// Sent as the QUIC/WebTransport `RESET_STREAM` application error code when a
/// data stream is terminated before all of its objects have been delivered.
///
/// Draft-16 §10.4.3 requires a FIN only when every object in the subgroup has
/// been written to the stream; any earlier termination MUST be a `RESET_STREAM`.
/// Truncating an object and then sending FIN instead tells the receiver the
/// subgroup ended cleanly at a byte offset that lands in the middle of an
/// object whose length it was already promised, which is a protocol violation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum DataStreamResetCode {
    /// An implementation specific error occurred.
    InternalError = 0x0,
    /// The stream was cancelled: the subscription ended early (for example an
    /// UNSUBSCRIBE, or a publisher deciding to stop serving it).
    Cancelled = 0x1,
    /// An object in the subgroup exceeded its delivery timeout.
    DeliveryTimeout = 0x2,
    /// The session is going away.
    SessionClosed = 0x3,
    /// An object had a status the sender could not represent.
    UnknownObjectStatus = 0x4,
    /// The track was detected to be malformed.
    MalformedTrack = 0x12,
}

impl From<DataStreamResetCode> for u32 {
    fn from(value: DataStreamResetCode) -> Self {
        value as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_the_registry() {
        // Draft-16 §13.4.4. These go on the wire as RESET_STREAM error codes,
        // so the values are normative rather than internal labels.
        assert_eq!(u32::from(DataStreamResetCode::InternalError), 0x0);
        assert_eq!(u32::from(DataStreamResetCode::Cancelled), 0x1);
        assert_eq!(u32::from(DataStreamResetCode::DeliveryTimeout), 0x2);
        assert_eq!(u32::from(DataStreamResetCode::SessionClosed), 0x3);
        assert_eq!(u32::from(DataStreamResetCode::UnknownObjectStatus), 0x4);
        assert_eq!(u32::from(DataStreamResetCode::MalformedTrack), 0x12);
    }
}
