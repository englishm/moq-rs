// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

/// An error that causes the session to close.
#[derive(thiserror::Error, Debug)]
pub enum SessionError {
    // Official error codes
    #[error("no error")]
    NoError,

    #[error("internal error")]
    InternalError,

    #[error("unauthorized")]
    Unauthorized,

    #[error("protocol violation")]
    ProtocolViolation,

    #[error("duplicate track alias")]
    DuplicateTrackAlias,

    #[error("parameter length mismatch")]
    ParameterLengthMismatch,

    #[error("too many subscribes")]
    TooManySubscribes,

    #[error("goaway timeout")]
    GoawayTimeout,

    #[error("unknown error: code={0}")]
    Unknown(u64),
    // Unofficial error codes
}

pub trait MoqError {
    /// An integer code that is sent over the wire.
    fn code(&self) -> u64;
}

impl MoqError for SessionError {
    /// An integer code that is sent over the wire.
    fn code(&self) -> u64 {
        match self {
            // Official error codes
            Self::NoError => 0x0,
            Self::InternalError => 0x1,
            Self::Unauthorized => 0x2,
            Self::ProtocolViolation => 0x3,
            Self::DuplicateTrackAlias => 0x4,
            Self::ParameterLengthMismatch => 0x5,
            Self::TooManySubscribes => 0x6,
            Self::GoawayTimeout => 0x10,
            Self::Unknown(code) => *code,
            // Unofficial error codes
        }
    }
}

/// An error that causes the subscribe to be rejected immediately.
#[derive(thiserror::Error, Debug)]
pub enum SubscribeError {
    // Official error codes
    #[error("internal error")]
    InternalError,

    #[error("invalid range")]
    InvalidRange,

    #[error("retry track alias")]
    RetryTrackAlias,

    #[error("track does not exist")]
    TrackDoesNotExist,

    #[error("unauthorized")]
    Unauthorized,

    #[error("timeout")]
    Timeout,

    #[error("unknown error: code={0}")]
    Unknown(u64),
    // Unofficial error codes
}

impl MoqError for SubscribeError {
    /// An integer code that is sent over the wire.
    fn code(&self) -> u64 {
        match self {
            // Official error codes
            Self::InternalError => 0x0,
            Self::InvalidRange => 0x1,
            Self::RetryTrackAlias => 0x2,
            Self::TrackDoesNotExist => 0x3,
            Self::Unauthorized => 0x4,
            Self::Timeout => 0x5,
            Self::Unknown(code) => *code,
            // Unofficial error codes
        }
    }
}

/// An error that causes the subscribe to be terminated.
///
/// Unused legacy table. Its values predate draft-18 and disagree with the
/// §15.10.3 PUBLISH_DONE registry from 0x2 upwards, so converting a wire status
/// code through it yields the wrong variant. Use
/// [`PublishDoneCode`](crate::message::PublishDoneCode) instead.
#[derive(thiserror::Error, Debug)]
pub enum SubscribeDone {
    // Official error codes
    #[error("unsubscribed")]
    Unsubscribed,

    #[error("internal error")]
    InternalError,

    // TODO This should be in SubscribeError
    #[error("unauthorized")]
    Unauthorized,

    #[error("track ended")]
    TrackEnded,

    // TODO What the heck is this?
    #[error("subscription ended")]
    SubscriptionEnded,

    #[error("going away")]
    GoingAway,

    #[error("expired")]
    Expired,

    #[error("unknown error: code={0}")]
    Unknown(u64),
}

impl From<u64> for SubscribeDone {
    fn from(code: u64) -> Self {
        match code {
            0x0 => Self::Unsubscribed,
            0x1 => Self::InternalError,
            0x2 => Self::Unauthorized,
            0x3 => Self::TrackEnded,
            0x4 => Self::SubscriptionEnded,
            0x5 => Self::GoingAway,
            0x6 => Self::Expired,
            _ => Self::Unknown(code),
        }
    }
}

impl MoqError for SubscribeDone {
    /// An integer code that is sent over the wire.
    fn code(&self) -> u64 {
        match self {
            // Official error codes
            Self::Unsubscribed => 0x0,
            Self::InternalError => 0x1,
            Self::Unauthorized => 0x2,
            Self::TrackEnded => 0x3,
            Self::SubscriptionEnded => 0x4,
            Self::GoingAway => 0x5,
            Self::Expired => 0x6,
            Self::Unknown(code) => *code,
            // Unofficial error codes
        }
    }
}
