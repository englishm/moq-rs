// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

#[derive(thiserror::Error, Debug, Clone, PartialEq)]
pub enum ServeError {
    // TODO stop using?
    #[error("done")]
    Done,

    #[error("cancelled")]
    Cancel,

    #[error("closed, code={0}")]
    Closed(u64),

    #[error("not found")]
    NotFound,

    /// A request ended with draft-18 REQUEST_ERROR TIMEOUT.
    #[error("request timed out")]
    Timeout,

    #[error("not found: {0} [error:{1}]")]
    NotFoundWithId(String, uuid::Uuid),

    #[error("duplicate")]
    Duplicate,

    #[error("multiple stream modes")]
    Mode,

    #[error("wrong size")]
    Size,

    #[error("internal error: {0}")]
    Internal(String),

    #[error("internal error: {0} [error:{1}]")]
    InternalWithId(String, uuid::Uuid),

    #[error("not implemented: {0}")]
    NotImplemented(String),

    #[error("not implemented: {0} [error:{1}]")]
    NotImplementedWithId(String, uuid::Uuid),
}

impl ServeError {
    /// Legacy per-request/per-track error codes.
    ///
    /// These predate draft-18 and do **not** match its registries: `NotFound`
    /// answers 0x4 where §15.10.2 assigns DOES_NOT_EXIST 0x10, and `Done`
    /// answers 0x0 where the PUBLISH_DONE path in `session::subscribed` uses
    /// TRACK_ENDED 0x2.
    ///
    /// The only direct in-workspace wire user is PUBLISH_NAMESPACE_CANCEL. Note
    /// also that the public
    /// [`SessionError::code`](crate::session::SessionError::code) delegates here
    /// for its `Serve` variant: nothing in this workspace calls it, but an
    /// embedder can, and its result is measured against the §15.10.1
    /// session-termination registry — a third registry these values were never
    /// designed for. Both callers need addressing to retire this.
    ///
    /// New code should map to [`RequestErrorCode`](crate::message::RequestErrorCode)
    /// or [`PublishDoneCode`](crate::message::PublishDoneCode) directly.
    ///
    /// TODO: retire this in favour of the registry enums.
    pub fn code(&self) -> u64 {
        match self {
            // Special case: 0 typically means successful completion or internal error depending on context
            Self::Done => 0,
            // Cancel/Going away - maps to various contexts
            Self::Cancel => 1,
            // Pass through application-specific error codes
            Self::Closed(code) => *code,
            // TRACK_DOES_NOT_EXIST (0x4) from SUBSCRIBE_ERROR codes
            Self::NotFound | Self::NotFoundWithId(_, _) => 0x4,
            // TIMEOUT (0x2) in the REQUEST_ERROR registry
            Self::Timeout => 0x2,
            // This is more of a session-level error, but keeping a reasonable code
            Self::Duplicate => 0x5,
            // NOT_SUPPORTED (0x3) - appears in multiple error code registries
            Self::Mode => 0x3,
            Self::Size => 0x3,
            Self::NotImplemented(_) | Self::NotImplementedWithId(_, _) => 0x3,
            // INTERNAL_ERROR (0x0) - per-request error registries use 0x0
            Self::Internal(_) | Self::InternalWithId(_, _) => 0x0,
        }
    }

    /// Create NotFound error with correlation ID but no additional context.
    /// Uses generic messages for both logging and wire protocol.
    ///
    /// Example: `ServeError::not_found_id()`
    #[track_caller]
    pub fn not_found_id() -> Self {
        let id = uuid::Uuid::new_v4();
        let loc = std::panic::Location::caller();
        tracing::warn!("[{}] Not found at {}:{}", id, loc.file(), loc.line());
        Self::NotFoundWithId("Track not found".to_string(), id)
    }

    /// Create NotFound error with correlation ID and internal context.
    /// The internal context is logged but a generic message is sent on the wire.
    ///
    /// Example: `ServeError::not_found_ctx("subscribe_id=123 not in map")`
    #[track_caller]
    pub fn not_found_ctx(internal_context: impl Into<String>) -> Self {
        let context = internal_context.into();
        let id = uuid::Uuid::new_v4();
        let loc = std::panic::Location::caller();
        tracing::warn!(
            "[{}] Not found: {} at {}:{}",
            id,
            context,
            loc.file(),
            loc.line()
        );
        Self::NotFoundWithId("Track not found".to_string(), id)
    }

    /// Create NotFound error with full control over internal and external messages.
    /// The internal context is logged, and the external message is sent on the wire.
    ///
    /// Example: `ServeError::not_found_full("subscribe_id=123 not in map", "Subscription expired")`
    #[track_caller]
    pub fn not_found_full(
        internal_context: impl Into<String>,
        external_message: impl Into<String>,
    ) -> Self {
        let context = internal_context.into();
        let message = external_message.into();
        let id = uuid::Uuid::new_v4();
        let loc = std::panic::Location::caller();
        tracing::warn!(
            "[{}] Not found: {} at {}:{}",
            id,
            context,
            loc.file(),
            loc.line()
        );
        Self::NotFoundWithId(message, id)
    }

    /// Create Internal error with correlation ID and internal context.
    /// The internal context is logged but a generic message is sent on the wire.
    ///
    /// Example: `ServeError::internal_ctx("subscriber map in bad state")`
    #[track_caller]
    pub fn internal_ctx(internal_context: impl Into<String>) -> Self {
        let context = internal_context.into();
        let id = uuid::Uuid::new_v4();
        let loc = std::panic::Location::caller();
        tracing::error!(
            "[{}] Internal error: {} at {}:{}",
            id,
            context,
            loc.file(),
            loc.line()
        );
        Self::InternalWithId("Internal error".to_string(), id)
    }

    /// Create NotImplemented error with correlation ID and feature context.
    /// The feature name is logged but a generic message is sent on the wire.
    ///
    /// Example: `ServeError::not_implemented_ctx("datagrams")`
    #[track_caller]
    pub fn not_implemented_ctx(feature: impl Into<String>) -> Self {
        let feature = feature.into();
        let id = uuid::Uuid::new_v4();
        let loc = std::panic::Location::caller();
        tracing::warn!(
            "[{}] Not implemented: {} at {}:{}",
            id,
            feature,
            loc.file(),
            loc.line()
        );
        Self::NotImplementedWithId("Feature not implemented".to_string(), id)
    }
}
