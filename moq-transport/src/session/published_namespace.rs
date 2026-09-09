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

    /// Reason phrase to send on drop, when the caller supplied one.
    ///
    /// Without this the peer receives the `Display` of the internal
    /// [`ServeError`] — for a coded rejection that renders as
    /// `"closed, code=N"`, which restates the code and tells the peer nothing.
    reason: Option<String>,
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
            reason: None,
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
    /// "not interested" from "not permitted".
    ///
    /// `reason` is sent to the peer in the Reason Phrase, so it must describe
    /// the rejection without disclosing anything the peer is not entitled to
    /// know. It is truncated to [`ReasonPhrase::MAX_LEN`], since a longer
    /// phrase fails to encode and would take the whole message with it.
    ///
    /// Calling this after [`ok`](Self::ok) is permitted and revokes the
    /// acceptance: the peer receives PUBLISH_NAMESPACE_CANCEL (§9.24) instead
    /// of REQUEST_ERROR, carrying this same code and reason. §9.24 draws its
    /// codes from the same registry as REQUEST_ERROR, and expiry of
    /// authorization is one of the reasons the draft names for a cancel, so
    /// UNAUTHORIZED is meaningful there.
    ///
    /// [`SubscribedNamespace::reject`]: super::SubscribedNamespace::reject
    pub fn reject(mut self, error_code: u64, reason: &str) -> Result<(), ServeError> {
        self.error = Some(ServeError::Closed(error_code));
        self.error_code = Some(error_code);
        self.reason = Some(reason.to_string());
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

        // A caller-supplied reason wins over the error's own `Display`, which
        // for a coded rejection is only `"closed, code=N"` and so tells the
        // peer nothing the code did not already.
        let reason = self.reason.clone().unwrap_or_else(|| err.to_string());

        // An explicit `reject` code wins; otherwise the namespace is simply no
        // longer wanted. Deriving the default from `err` would make a clean
        // teardown report INTERNAL_ERROR, since that is code 0 and what
        // `ServeError::Done` renders as.
        let error_code = self
            .error_code
            .unwrap_or(RequestErrorCode::Uninterested as u64);

        if self.ok {
            // Already answered with REQUEST_OK, so what is owed now is a
            // revocation of that acceptance — and only if there is still
            // something to revoke. A peer that has sent PUBLISH_NAMESPACE_DONE
            // has withdrawn already.
            if self.state.lock().done {
                return;
            }

            // Accepted: send PUBLISH_NAMESPACE_CANCEL to revoke acceptance
            // (draft-16 §9.24).  Carries Request ID, not the namespace.
            self.session.send_message(message::PublishNamespaceCancel {
                id: self.info.request_id,
                error_code,
                reason_phrase: ReasonPhrase::new(reason),
            });
        } else {
            // Never answered. Draft-16 §6.2: "A subscriber MUST send exactly
            // one REQUEST_OK or REQUEST_ERROR in response to a
            // PUBLISH_NAMESPACE." A peer withdrawing does not discharge that,
            // so this is sent even once PUBLISH_NAMESPACE_DONE has arrived —
            // otherwise a publisher that pipelines DONE behind its
            // PUBLISH_NAMESPACE, which it may do while the relay is still
            // awaiting authorization, gets no response at all.
            self.session.send_request_error(
                "publish_namespace",
                message::RequestError {
                    id: self.info.request_id,
                    error_code,
                    retry_interval: 0,
                    reason: ReasonPhrase::new(reason),
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

    use crate::coding::{Decode, Encode, TrackNamespace};
    use crate::session::{PendingRequests, RequestId, Subscriber};
    use crate::watch::Queue;

    /// Build a `PublishedNamespace` wired to a queue the test can drain, so
    /// the bytes destined for the peer are observable.
    ///
    /// The returned handle is a *clone* of the sending half rather than the
    /// receiving half: `Queue::close` needs `lock_mut`, which stops working
    /// once every drop guard is gone, and the guard the `Subscriber` holds
    /// dies with the `PublishedNamespace` whose `Drop` we are trying to
    /// observe. Cloning shares the guard, so the queue stays readable
    /// afterwards.
    fn published() -> (PublishedNamespace, Queue<message::Message>) {
        let (send, _recv, observer) = published_with_recv();
        (send, observer)
    }

    /// As [`published`], but also yields the receive half so a test can
    /// simulate the peer sending PUBLISH_NAMESPACE_DONE.
    fn published_with_recv() -> (
        PublishedNamespace,
        PublishedNamespaceRecv,
        Queue<message::Message>,
    ) {
        let outgoing = Queue::<message::Message>::default();
        let observer = outgoing.clone();
        let namespace_open = Queue::default().split();

        let subscriber = Subscriber::new(
            outgoing,
            namespace_open.0,
            None,
            RequestId::new(0, 100, 100, 1),
            PendingRequests::default(),
        );

        let (send, recv) =
            PublishedNamespace::new(subscriber, 42, TrackNamespace::from_utf8_path("sports"));

        (send, recv, observer)
    }

    /// The rejection the peer receives, as (error_code, reason_phrase).
    fn sent_request_error(outgoing: Queue<message::Message>) -> (u64, String) {
        let sent = outgoing.close();
        let error = sent
            .into_iter()
            .find_map(|msg| match msg {
                message::Message::RequestError(err) => Some(err),
                _ => None,
            })
            .expect("a REQUEST_ERROR should have been sent");

        (error.error_code, error.reason.0)
    }

    /// The revocation the peer receives, as (error_code, reason_phrase).
    ///
    /// A separate matcher because PUBLISH_NAMESPACE_CANCEL is a different
    /// message type; `sent_request_error` is structurally unable to see it,
    /// which is how the accepted-then-revoked branch went untested.
    fn sent_cancel(outgoing: Queue<message::Message>) -> (u64, String) {
        let sent = outgoing.close();
        let cancel = sent
            .into_iter()
            .find_map(|msg| match msg {
                message::Message::PublishNamespaceCancel(cancel) => Some(cancel),
                _ => None,
            })
            .expect("a PUBLISH_NAMESPACE_CANCEL should have been sent");

        (cancel.error_code, cancel.reason_phrase.0)
    }

    /// Whatever was sent, decoded from its encoded bytes.
    ///
    /// The queue assertions stop at the in-memory message; this confirms the
    /// reason survives encoding, which is what actually reaches the peer.
    fn round_trip(outgoing: Queue<message::Message>) -> message::RequestError {
        let sent = outgoing.close();
        let error = sent
            .into_iter()
            .find_map(|msg| match msg {
                message::Message::RequestError(err) => Some(err),
                _ => None,
            })
            .expect("a REQUEST_ERROR should have been sent");

        let mut buf = Vec::new();
        error.encode(&mut buf).expect("must encode");

        let mut cursor = &buf[..];
        message::RequestError::decode(&mut cursor).expect("must decode")
    }

    /// The reason has to reach the peer, not just the local log. Without this
    /// the phrase is the `Display` of `ServeError::Closed`, i.e.
    /// `"closed, code=1"`, which merely restates the code.
    #[test]
    fn reject_sends_the_reason_to_the_peer() {
        let (published, outgoing) = published();
        published.reject(0x1, "unauthorized").unwrap();

        let (code, reason) = sent_request_error(outgoing);
        assert_eq!(code, 0x1);
        assert_eq!(reason, "unauthorized");
    }

    /// The queue holds a struct; the peer receives bytes. Confirm the reason
    /// survives the encode/decode round trip.
    #[test]
    fn the_reason_survives_encoding() {
        let (published, outgoing) = published();
        published.reject(0x1, "unauthorized").unwrap();

        let decoded = round_trip(outgoing);
        assert_eq!(decoded.id, 42);
        assert_eq!(decoded.error_code, 0x1);
        assert_eq!(decoded.reason.0, "unauthorized");
    }

    /// Rejecting an already-accepted namespace revokes it with
    /// PUBLISH_NAMESPACE_CANCEL, carrying the same code and reason. §9.24
    /// draws its codes from the REQUEST_ERROR registry, so UNAUTHORIZED is
    /// meaningful here.
    #[test]
    fn rejecting_after_accepting_revokes_with_the_same_code_and_reason() {
        let (mut published, outgoing) = published();
        published.ok().unwrap();
        published.reject(0x1, "unauthorized").unwrap();

        let (code, reason) = sent_cancel(outgoing);
        assert_eq!(code, 0x1);
        assert_eq!(reason, "unauthorized");
    }

    /// A clean teardown of an accepted namespace is not an internal error.
    /// Deriving the code from `ServeError::Done` would send 0x0, which
    /// §13.4.2 defines as INTERNAL_ERROR.
    #[test]
    fn a_clean_teardown_does_not_report_an_internal_error() {
        let (mut published, outgoing) = published();
        published.ok().unwrap();
        drop(published);

        let (code, _) = sent_cancel(outgoing);
        assert_ne!(code, 0x0, "0x0 is INTERNAL_ERROR");
        assert_eq!(code, RequestErrorCode::Uninterested as u64);
    }

    /// Draft-16 §6.2: "A subscriber MUST send exactly one REQUEST_OK or
    /// REQUEST_ERROR in response to a PUBLISH_NAMESPACE."
    ///
    /// A publisher may pipeline PUBLISH_NAMESPACE_DONE behind its
    /// PUBLISH_NAMESPACE, and the relay awaits authorization before
    /// answering — so the withdrawal can land first. It does not discharge
    /// the obligation to answer.
    #[test]
    fn a_withdrawal_before_the_response_does_not_suppress_it() {
        let (published, recv, outgoing) = published_with_recv();

        // The peer withdraws before we have answered.
        recv.recv_done().unwrap();
        published.reject(0x1, "unauthorized").unwrap();

        let (code, reason) = sent_request_error(outgoing);
        assert_eq!(code, 0x1);
        assert_eq!(reason, "unauthorized");
    }

    /// Once accepted, a withdrawal *does* settle it: the response was already
    /// sent, and there is nothing left to revoke.
    #[test]
    fn a_withdrawal_after_accepting_suppresses_the_cancel() {
        let (mut published, recv, outgoing) = published_with_recv();
        published.ok().unwrap();
        recv.recv_done().unwrap();
        drop(published);

        let sent = outgoing.close();
        assert!(
            !sent
                .iter()
                .any(|msg| matches!(msg, message::Message::PublishNamespaceCancel(_))),
            "nothing to revoke once the peer has withdrawn"
        );
    }

    /// `close` has no reason of its own, so it keeps falling back to the
    /// error's own rendering and to UNINTERESTED.
    #[test]
    fn close_falls_back_to_the_error_rendering() {
        let (published, outgoing) = published();
        published.close(ServeError::NotFound).unwrap();

        let (code, reason) = sent_request_error(outgoing);
        assert_eq!(code, RequestErrorCode::Uninterested as u64);
        assert_eq!(reason, ServeError::NotFound.to_string());
    }

    /// Dropping without a verdict still tells the peer something.
    #[test]
    fn plain_drop_still_sends_an_error() {
        let (published, outgoing) = published();
        drop(published);

        let (code, _) = sent_request_error(outgoing);
        assert_eq!(code, RequestErrorCode::Uninterested as u64);
    }

    /// A phrase longer than `ReasonPhrase::MAX_LEN` fails to encode, which
    /// would drop the whole REQUEST_ERROR and leave the peer with nothing.
    #[test]
    fn an_oversized_reason_is_truncated_rather_than_dropped() {
        let (published, outgoing) = published();
        published
            .reject(0x1, &"x".repeat(ReasonPhrase::MAX_LEN * 2))
            .unwrap();

        let (_, reason) = sent_request_error(outgoing);
        assert_eq!(reason.len(), ReasonPhrase::MAX_LEN);

        // And it still encodes, which is the point of the bound.
        let mut buf = Vec::new();
        ReasonPhrase(reason).encode(&mut buf).expect("must encode");
    }

    /// An over-long *fallback* reason must be bounded too. `close` takes a
    /// `ServeError`, whose `Internal`/`NotImplemented` variants carry an
    /// arbitrary `String`, and an unencodable phrase on the control stream
    /// tears down the session rather than merely losing the message.
    #[test]
    fn an_oversized_fallback_reason_is_also_bounded() {
        let (published, outgoing) = published();
        published
            .close(ServeError::Internal("y".repeat(ReasonPhrase::MAX_LEN * 4)))
            .unwrap();

        let (_, reason) = sent_request_error(outgoing);
        assert!(reason.len() <= ReasonPhrase::MAX_LEN);

        let mut buf = Vec::new();
        ReasonPhrase(reason).encode(&mut buf).expect("must encode");
    }

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
