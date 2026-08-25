// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Outbound PUBLISH handling: a publisher sends PUBLISH, receives PUBLISH_OK
//! or REQUEST_ERROR, serves Objects, and terminates with PUBLISH_DONE.
//!
//! The data-plane serving path is shared with SUBSCRIBE serving through
//! `ObjectForwarder`; PUBLISH-specific state stays in `Published`.

use std::ops;

use crate::{
    coding::{Location, ReasonPhrase, TrackName, TrackNamespace},
    message,
    serve::{ServeError, TrackReader},
    watch::State,
};

use super::{DeliveryFilter, ObjectForwarder, Publisher, SessionError};

#[derive(Debug, Clone)]
pub struct PublishedInfo {
    pub id: u64,
    pub track_namespace: TrackNamespace,
    pub track_name: TrackName,
    pub track_alias: u64,
    pub forward: bool,
    pub largest_location: Option<Location>,
}

#[derive(Debug)]
pub(crate) struct PublishedState {
    ok: bool,
    forward: bool,
    unsubscribed: bool,
    closed: Result<(), ServeError>,
}

impl PublishedState {
    fn new(forward: bool) -> Self {
        Self {
            ok: false,
            forward,
            unsubscribed: false,
            closed: Ok(()),
        }
    }
}

/// Outbound PUBLISH created by [`Publisher::publish`].
///
/// Dropping without calling [`serve`] sends PUBLISH_DONE with a generic
/// terminal status. Calling [`serve`] runs the shared object forwarder; dropping
/// after the data-plane loop has stopped sends PUBLISH_DONE from this handle.
#[must_use = "serve or drop to send PUBLISH_DONE"]
pub struct Published {
    publisher: Publisher,
    state: State<PublishedState>,
    track: Option<TrackReader>,

    pub info: PublishedInfo,
}

impl Published {
    pub(super) fn new(
        publisher: Publisher,
        info: PublishedInfo,
        state: State<PublishedState>,
        track: TrackReader,
    ) -> Self {
        Self {
            publisher,
            state,
            track: Some(track),
            info,
        }
    }

    /// Wait until the subscriber accepts with PUBLISH_OK (§9.14).
    pub async fn ok(&mut self) -> Result<(), ServeError> {
        loop {
            {
                let state = self.state.lock();
                state.closed.clone()?;
                if state.ok {
                    return Ok(());
                }
                match state.modified() {
                    Some(notify) => notify,
                    None => return Err(ServeError::Done),
                }
            }
            .await;
        }
    }

    /// Wait until this PUBLISH is closed or rejected.
    pub async fn closed(&self) -> Result<(), ServeError> {
        loop {
            {
                let state = self.state.lock();
                state.closed.clone()?;
                match state.modified() {
                    Some(notify) => notify,
                    None => return Ok(()),
                }
            }
            .await;
        }
    }

    /// Serve Objects for this PUBLISH using the same data-plane path as
    /// SUBSCRIBE serving.
    ///
    /// This waits for PUBLISH_OK before serving. draft-16 allows serving before
    /// PUBLISH_OK, but does not require it; waiting keeps the first cut simple.
    pub async fn serve(mut self) -> Result<(), SessionError> {
        let res = self.serve_inner().await;
        if let Err(err) = &res {
            self.close_state(err.clone().into())?;
        }

        res
    }

    async fn serve_inner(&mut self) -> Result<(), SessionError> {
        self.ok().await?;

        let forward = self.state.lock().forward;
        if !forward {
            let track = self.track.take().ok_or(SessionError::Internal)?;
            let res = tokio::select! {
                res = track.closed() => res,
                res = self.closed() => res,
            };
            return match res {
                Ok(()) | Err(ServeError::Done | ServeError::Cancel) => Ok(()),
                Err(err) => Err(err.into()),
            };
        }

        let track = self.track.take().ok_or(SessionError::Internal)?;
        let (mut forwarder, recv) =
            ObjectForwarder::new(self.publisher.clone(), self.info.track_alias, None);
        self.publisher
            .register_published_subscription(self.info.id, recv)?;

        let largest_location = track.largest_location();
        forwarder.set_largest_location(largest_location)?;
        let delivery_filter = DeliveryFilter {
            forward,
            start_location: None,
            end_group_id: None,
        };

        match forwarder.serve(track, delivery_filter).await {
            Err(SessionError::Serve(ServeError::Cancel)) => Ok(()),
            res => res,
        }
    }

    pub fn close(self, err: ServeError) -> Result<(), ServeError> {
        self.close_state(err)
    }

    fn close_state(&self, err: ServeError) -> Result<(), ServeError> {
        let state = self
            .state
            .try_lock()
            .map_err(|_| ServeError::internal_ctx("published state lock poisoned"))?;
        state.closed.clone()?;

        let mut state = state.into_mut().ok_or(ServeError::Done)?;
        state.closed = Err(err);

        Ok(())
    }
}

impl ops::Deref for Published {
    type Target = PublishedInfo;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

impl Drop for Published {
    fn drop(&mut self) {
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(_) => {
                tracing::error!(
                    session_id = %self.publisher.session_id(),
                    request_id = self.info.id,
                    "published state lock poisoned while dropping PUBLISH"
                );
                return;
            }
        };
        let Some(err) = publish_done_error_on_drop(&state) else {
            return;
        };
        drop(state);

        if self
            .publisher
            .try_send_message(message::PublishDone {
                id: self.info.id,
                status_code: publish_done_code(&err),
                stream_count: 0,
                reason: ReasonPhrase("publish ended".to_string()),
            })
            .is_err()
        {
            tracing::error!(
                session_id = %self.publisher.session_id(),
                request_id = self.info.id,
                "failed to enqueue PUBLISH_DONE while dropping PUBLISH"
            );
        }
    }
}

pub(crate) struct PublishedRecv {
    state: State<PublishedState>,
}

impl PublishedRecv {
    pub fn new(state: State<PublishedState>) -> Self {
        Self { state }
    }

    pub fn recv_ok(&mut self, msg: &message::PublishOk) -> Result<(), ServeError> {
        let forward = msg
            .params
            .forward()
            .map_err(|_| ServeError::internal_ctx("invalid FORWARD in PUBLISH_OK"))?;

        if let Some(mut state) = self.state.lock_mut() {
            state.ok = true;
            if let Some(forward) = forward {
                state.forward = forward;
            }
        }

        Ok(())
    }

    pub fn recv_error(&mut self, err: ServeError) -> Result<(), ServeError> {
        if let Some(mut state) = self.state.lock_mut() {
            state.closed = Err(err);
        }
        Ok(())
    }

    pub fn recv_unsubscribe(&mut self) -> Result<(), ServeError> {
        let state = self.state.lock();
        state.closed.clone()?;

        if let Some(mut state) = state.into_mut() {
            state.unsubscribed = true;
            state.closed = Err(ServeError::Cancel);
        }

        Ok(())
    }
}

pub(crate) fn split_published_state(
    forward: bool,
) -> (State<PublishedState>, State<PublishedState>) {
    State::new(PublishedState::new(forward)).split()
}

fn publish_done_code(err: &ServeError) -> u64 {
    match err {
        ServeError::Done => message::PublishDoneCode::TrackEnded as u64,
        ServeError::Closed(code) => *code,
        _ => message::PublishDoneCode::InternalError as u64,
    }
}

fn publish_done_error_on_drop(state: &PublishedState) -> Option<ServeError> {
    // If the subscriber rejected the PUBLISH with REQUEST_ERROR before it
    // became established, the request is already terminal (§5.1). If the
    // subscriber sent UNSUBSCRIBE, it already terminated its side.
    if state.unsubscribed || (!state.ok && state.closed.is_err()) {
        return None;
    }

    Some(
        state
            .closed
            .as_ref()
            .err()
            .cloned()
            .unwrap_or(ServeError::Done),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding::KeyValuePairs;

    #[test]
    fn recv_ok_sets_forward_when_present() {
        let (_send, recv_state) = split_published_state(true);
        let mut recv = PublishedRecv::new(recv_state);
        let mut params = KeyValuePairs::default();
        params.set_forward(false);

        recv.recv_ok(&message::PublishOk { id: 0, params }).unwrap();

        assert!(!recv.state.lock().forward);
        assert!(recv.state.lock().ok);
    }

    #[test]
    fn publish_done_code_maps_done_to_track_ended() {
        assert_eq!(
            publish_done_code(&ServeError::Done),
            message::PublishDoneCode::TrackEnded as u64
        );
    }

    #[test]
    fn drop_terminal_error_sends_done_after_accepted_normal_completion() {
        let mut state = PublishedState::new(true);
        state.ok = true;

        assert_eq!(publish_done_error_on_drop(&state), Some(ServeError::Done));
    }

    #[test]
    fn drop_terminal_error_skips_pre_accept_rejection() {
        let mut state = PublishedState::new(true);
        state.closed = Err(ServeError::Closed(123));

        assert_eq!(publish_done_error_on_drop(&state), None);
    }

    #[test]
    fn recv_unsubscribe_marks_unsubscribed_and_closes() {
        let (_send, recv_state) = split_published_state(true);
        let mut recv = PublishedRecv::new(recv_state);

        recv.recv_unsubscribe().unwrap();

        let state = recv.state.lock();
        assert!(state.unsubscribed);
        assert!(matches!(state.closed, Err(ServeError::Cancel)));
        assert_eq!(publish_done_error_on_drop(&state), None);
    }
}
