// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A stable correlation id for one MoQ session.
//!
//! The value is normally the QUIC connection ID hex, which both peers observe and which already
//! names the per-connection qlog and mlog files. Attaching it to log records lets a session's
//! events be gathered with a single string, and cross-referenced against those files.

use std::{fmt, sync::Arc};

/// A stable, per-session correlation id, rendered as lowercase hex.
///
/// Cloning is a refcount bump: the id is cloned into [`Session`](super::Session),
/// [`Publisher`](super::Publisher), [`Subscriber`](super::Subscriber), both halves of the session
/// run loop, and the relay's per-session context.
///
/// Native callers supply lowercase hex with no separators (`^[0-9a-f]*$`), so it can be compared
/// directly against a qlog or mlog filename without stripping anything. The length is not fixed:
/// RFC 9000 permits general QUIC connection IDs from 0 to 20 bytes, so a peer-supplied id may
/// contain 0 to 40 characters. The Initial DCID captured by the in-repo native transport is
/// generated as 16 bytes; [RFC 9000 section 7.2] recommends at least 8 bytes for Initial DCIDs.
///
/// [RFC 9000 section 7.2]: https://www.rfc-editor.org/rfc/rfc9000.html#section-7.2
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionId(Arc<str>);

impl SessionId {
    /// Adopt an externally supplied id, normally the QUIC connection ID hex.
    ///
    /// The caller is responsible for supplying lowercase hex; [`quinn`]'s `Display` for a
    /// connection ID already produces exactly that.
    ///
    /// [`quinn`]: https://docs.rs/quinn
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    /// Mint a local id, for sessions built without a transport-supplied connection ID.
    ///
    /// Renders as 32 lowercase hex characters, the same textual shape as a 16-byte connection ID.
    /// A generated id correlates a session's own events, but is not guaranteed to match that
    /// session's qlog or mlog filename.
    pub fn generate() -> Self {
        let mut buf = [0u8; uuid::fmt::Simple::LENGTH];
        let hex = uuid::Uuid::new_v4().simple().encode_lower(&mut buf);
        Self(Arc::from(&*hex))
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SessionId {
    fn from(id: &str) -> Self {
        Self::new(id)
    }
}

impl From<String> for SessionId {
    fn from(id: String) -> Self {
        Self::new(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_lowercase_hex(s: &str) -> bool {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    #[test]
    fn generate_is_32_lowercase_hex_chars() {
        let id = SessionId::generate();
        assert_eq!(id.as_str().len(), 32, "expected the 16-byte CID shape");
        assert!(
            is_lowercase_hex(id.as_str()),
            "not lowercase hex: {id}, which would not compare against an mlog filename"
        );
    }

    #[test]
    fn generate_has_no_hyphens() {
        // The hyphenated 36-char UUID form must never appear: one shape only, so a reader never
        // has to strip separators before comparing.
        let id = SessionId::generate();
        assert!(!id.as_str().contains('-'), "hyphenated form leaked: {id}");
    }

    #[test]
    fn generate_is_unique_per_call() {
        let a = SessionId::generate();
        let b = SessionId::generate();
        assert_ne!(a, b);
    }

    #[test]
    fn new_preserves_a_connection_id_verbatim() {
        // A real 16-byte quinn connection ID, as rendered by its Display impl.
        let cid = "8f2a1c94d7e3b60518aa4c2f9d013e77";
        assert_eq!(SessionId::new(cid).as_str(), cid);
    }

    #[test]
    fn display_matches_as_str() {
        let id = SessionId::generate();
        assert_eq!(id.to_string(), id.as_str());
    }

    #[test]
    fn clone_shares_the_allocation() {
        // Cloning must be a refcount bump, since the id is cloned into every session-scoped type.
        let id = SessionId::generate();
        let cloned = id.clone();
        assert!(
            std::ptr::eq(id.as_str().as_ptr(), cloned.as_str().as_ptr()),
            "clone reallocated instead of sharing"
        );
    }

    #[test]
    fn accepts_the_rfc_connection_id_length_bounds() {
        // SessionId is general: RFC 9000 allows 0..=20-byte CIDs, even though the native
        // transport generates the captured Initial DCID as 16 bytes.
        let shortest = ""; // 0 bytes
        let longest = "0011223344556677889900aabbccddeeff001122"; // 20 bytes
        assert_eq!(SessionId::new(shortest).as_str().len(), 0);
        assert_eq!(SessionId::new(longest).as_str().len(), 40);
        assert!(is_lowercase_hex(longest));
    }
}
