// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ops;

use crate::coding::{ReasonPhrase, TrackNamespace};
use crate::message::RequestErrorCode;
use crate::watch::State;
use crate::{message, serve::ServeError};

use super::{PublishNamespaceInfo, Subscriber};

// Tracks whether the publisher has cleanly completed this namespace publish.
#[derive(Default)]
struct PublishedNamespaceState {
    done: bool,
}

/// Represents an inbound PUBLISH_NAMESPACE received by a subscriber.
///
/// On drop, revokes an accepted namespace with PUBLISH_NAMESPACE_CANCEL, or
/// rejects an unaccepted namespace with REQUEST_ERROR.
pub struct PublishedNamespace {
    session: Subscriber,
    state: State<PublishedNamespaceState>,

    pub info: PublishNamespaceInfo,

    ok: bool,
    error: Option<ServeError>,

    /// REQUEST_ERROR code to send on drop when the publish was never accepted.
    /// `None` falls back to UNINTERESTED, the default for a relay that simply
    /// declined the namespace. Set by [`reject`](Self::reject) so callers can
    /// state a specific reason such as UNAUTHORIZED.
    error_code: Option<u64>,
}

impl PublishedNamespace {
    pub(super) fn new(
        session: Subscriber,
        request_id: u64,
        namespace: TrackNamespace,
    ) -> (PublishedNamespace, PublishedNamespaceRecv) {
        let info = PublishNamespaceInfo {
            request_id,
            namespace,
        };

        let (send, recv) = State::default().split();
        let send = Self {
            session,
            info,
            ok: false,
            error: None,
            error_code: None,
            state: send,
        };
        let recv = PublishedNamespaceRecv {
            state: recv,
            request_id,
        };

        (send, recv)
    }

    /// Accept the PUBLISH_NAMESPACE by sending REQUEST_OK (draft-16 §9.7).
    pub fn ok(&mut self) -> Result<(), ServeError> {
        if self.ok {
            return Err(ServeError::Duplicate);
        }

        // Draft-16 §6.2: acceptance is signalled with REQUEST_OK, not the
        // legacy PUBLISH_NAMESPACE_OK.
        self.session.send_request_ok(
            "publish_namespace",
            message::RequestOk {
                id: self.info.request_id,
                params: Default::default(),
            },
        );

        self.ok = true;

        Ok(())
    }

    /// Wait until the peer closes the namespace publish (PUBLISH_NAMESPACE_DONE).
    pub async fn closed(&self) -> Result<(), ServeError> {
        loop {
            let Some(modified) = self.state.lock().modified() else {
                return Ok(());
            };

            modified.await;
        }
    }

    /// Reject the PUBLISH_NAMESPACE; the error is sent on drop.
    ///
    /// The REQUEST_ERROR carries UNINTERESTED. Use [`reject`](Self::reject) to
    /// send a specific error code such as UNAUTHORIZED.
    pub fn close(mut self, err: ServeError) -> Result<(), ServeError> {
        self.error = Some(err);
        Ok(())
    }

    /// Reject the PUBLISH_NAMESPACE with an explicit REQUEST_ERROR code
    /// (draft-16 §9.8); the error is sent on drop.
    ///
    /// Mirrors [`SubscribedNamespace::reject`], letting a caller distinguish
    /// "not interested" from "not permitted". Once the publish has been
    /// accepted with [`ok`](Self::ok) the code is ignored: acceptance is
    /// revoked with PUBLISH_NAMESPACE_CANCEL, which carries the error code
    /// derived from `reason` instead.
    ///
    /// [`SubscribedNamespace::reject`]: super::SubscribedNamespace::reject
    pub fn reject(mut self, error_code: u64, reason: &str) -> Result<(), ServeError> {
        self.error = Some(ServeError::Closed(error_code));
        self.error_code = Some(error_code);
        tracing::debug!(
            namespace = %self.info.namespace,
            request_id = self.info.request_id,
            error_code,
            reason,
            "rejecting PUBLISH_NAMESPACE"
        );
        Ok(())
    }
}

impl ops::Deref for PublishedNamespace {
    type Target = PublishNamespaceInfo;

    fn deref(&self) -> &PublishNamespaceInfo {
        &self.info
    }
}

impl Drop for PublishedNamespace {
    fn drop(&mut self) {
        let err = self.error.clone().unwrap_or(ServeError::Done);

        if self.state.lock().done {
            return;
        }

        if self.ok {
            // Accepted: send PUBLISH_NAMESPACE_CANCEL to revoke acceptance
            // (draft-16 §9.24).  Carries Request ID, not the namespace.
            self.session.send_message(message::PublishNamespaceCancel {
                id: self.info.request_id,
                error_code: err.code(),
                reason_phrase: ReasonPhrase(err.to_string()),
            });
        } else {
            // Never accepted: send REQUEST_ERROR (draft-16 §9.8).
            self.session.send_request_error(
                "publish_namespace",
                message::RequestError {
                    id: self.info.request_id,
                    error_code: self
                        .error_code
                        .unwrap_or(RequestErrorCode::Uninterested as u64),
                    retry_interval: 0,
                    reason: ReasonPhrase(err.to_string()),
                },
            );
        }
    }
}

pub(super) struct PublishedNamespaceRecv {
    state: State<PublishedNamespaceState>,
    /// Request ID of the corresponding PUBLISH_NAMESPACE, used for O(1) lookup
    /// when PUBLISH_NAMESPACE_DONE or PUBLISH_NAMESPACE_CANCEL arrives.
    pub request_id: u64,
}

impl PublishedNamespaceRecv {
    pub fn recv_done(self) -> Result<(), ServeError> {
        if let Some(mut state) = self.state.lock_mut() {
            state.done = true;
        }

        // Dropping the state signals the PublishedNamespace that the peer is done.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recv_done_marks_namespace_done_before_drop() {
        let state = State::<PublishedNamespaceState>::default();
        let (send_state, recv_state) = state.split();
        let recv = PublishedNamespaceRecv {
            state: recv_state,
            request_id: 0,
        };

        assert!(!send_state.lock().done);

        recv.recv_done().unwrap();

        assert!(send_state.lock().done);
        assert!(send_state.lock().modified().is_none());
    }
}
