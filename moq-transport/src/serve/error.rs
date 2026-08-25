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
    /// Returns error codes for per-request/per-track errors.
    /// These codes are used in SUBSCRIBE_ERROR, PUBLISH_DONE, FETCH_ERROR, etc.
    /// Error codes are based on draft-ietf-moq-transport-14 Section 13.1.x
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
        tracing::warn!(error_id = %id, "Not found at {}:{}", loc.file(), loc.line());
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
            error_id = %id,
            "Not found: {} at {}:{}",
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
            error_id = %id,
            "Not found: {} at {}:{}",
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
            error_id = %id,
            "Internal error: {} at {}:{}",
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
            error_id = %id,
            "Not implemented: {} at {}:{}",
            feature,
            loc.file(),
            loc.line()
        );
        Self::NotImplementedWithId("Feature not implemented".to_string(), id)
    }

    /// The correlation id for this error, if it carries one.
    ///
    /// The same id is embedded in [`Display`](std::fmt::Display), which is what
    /// goes on the wire as the reason phrase, so a client can quote it back and
    /// an operator can find the session that produced it.
    pub fn error_id(&self) -> Option<uuid::Uuid> {
        match self {
            Self::NotFoundWithId(_, id)
            | Self::InternalWithId(_, id)
            | Self::NotImplementedWithId(_, id) => Some(*id),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire reason phrase is built from `Display`. If the id ever stops
    /// being embedded there, a client can still see an error but nobody can
    /// trace it back to a session, so pin the two together.
    #[test]
    fn the_error_id_reaches_the_wire_reason_phrase() {
        for err in [
            ServeError::not_found_ctx("ctx"),
            ServeError::internal_ctx("ctx"),
            ServeError::not_implemented_ctx("feature"),
        ] {
            let id = err.error_id().expect("constructor should mint an id");
            assert!(
                err.to_string().contains(&id.to_string()),
                "id {id} missing from wire reason {err}"
            );
        }
    }

    #[test]
    fn errors_without_an_id_report_none() {
        assert_eq!(ServeError::NotFound.error_id(), None);
        assert_eq!(ServeError::Done.error_id(), None);
        assert_eq!(ServeError::Closed(7).error_id(), None);
    }
}
