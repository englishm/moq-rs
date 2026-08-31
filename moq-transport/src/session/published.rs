// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Outbound PUBLISH handling: a publisher sends PUBLISH, receives PUBLISH_OK
//! or REQUEST_ERROR, serves Objects, and terminates with PUBLISH_DONE.
//!
//! The data-plane serving path is shared with SUBSCRIBE serving through
//! `ObjectForwarder`; PUBLISH-specific state stays in `Published`.
//!
//! Draft-18: the request and everything that follows it live on one bidi
//! stream, so PUBLISH_DONE goes out through `stream` (the request stream's
//! sink) rather than the session's control-stream queue.

use std::ops;

use crate::{
    coding::{Location, ReasonPhrase, TrackName, TrackNamespace},
    message,
    serve::{ServeError, TrackReader},
    watch::State,
};

use super::{
    BidiResponse, DeliveryError, DeliveryFilter, ObjectForwarder, Publisher, RequestStreamSink,
    SessionError, StreamCount,
};

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
/// Calling [`serve`](Self::serve) runs the shared object forwarder and waits
/// until PUBLISH_DONE and the request-stream FIN have been committed. Dropping
/// before that normal terminal path starts makes one guarded best-effort send.
#[must_use = "serve or drop to send PUBLISH_DONE"]
pub struct Published {
    publisher: Publisher,
    stream: RequestStreamSink,
    state: State<PublishedState>,
    track: Option<TrackReader>,
    stream_count: StreamCount,
    terminal_started: bool,

    pub info: PublishedInfo,
}

impl Published {
    pub(super) fn new(
        publisher: Publisher,
        stream: RequestStreamSink,
        info: PublishedInfo,
        state: State<PublishedState>,
        track: TrackReader,
    ) -> Self {
        Self {
            publisher,
            stream,
            state,
            track: Some(track),
            stream_count: StreamCount::default(),
            terminal_started: false,
            info,
        }
    }

    /// Wait until the subscriber accepts, which draft-18 answers with
    /// REQUEST_OK (§10.5).
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
    /// This waits for PUBLISH_OK before serving. The draft allows serving
    /// before PUBLISH_OK but does not require it; waiting keeps the first cut
    /// simple.
    pub async fn serve(mut self) -> Result<(), SessionError> {
        let res = self.serve_inner().await;
        if let Err(err) = &res {
            let _ = self.close_state(err.clone().into());
        }
        let terminal = self.finish().await;
        res.and(terminal)
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
        self.stream_count = forwarder.stream_count_handle();
        self.publisher
            .register_published_subscription(self.info.id, recv)?;

        let largest_location = track.largest_location();
        forwarder.set_largest_location(largest_location)?;
        let delivery_filter = DeliveryFilter {
            forward,
            start_location: None,
            end_group_id: None,
        };

        let result = match forwarder.serve(track, delivery_filter).await {
            Err(SessionError::Serve(ServeError::Cancel)) => Ok(()),
            res => res,
        };

        result
    }

    async fn finish(&mut self) -> Result<(), SessionError> {
        let (err, stream_count) = {
            let state = self.state.lock();
            let Some(err) = publish_done_error_on_drop(&state) else {
                return Ok(());
            };
            (err, self.stream_count.get())
        };

        self.terminal_started = true;
        let (response, receipt) = BidiResponse::with_completion(
            message::PublishDone {
                id: self.info.id,
                status_code: publish_done_code(&err),
                stream_count,
                reason: ReasonPhrase("publish ended".to_string()),
            }
            .into(),
        );
        self.stream
            .send(response)
            .map_err(|_| DeliveryError::Cancelled.into_session_error())?;
        let result = receipt
            .await
            .map_err(|_| DeliveryError::Cancelled.into_session_error())?
            .map_err(DeliveryError::into_session_error);
        result
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
        if self.terminal_started {
            return;
        }

        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(_) => {
                tracing::error!(
                    request_id = self.info.id,
                    "published state lock poisoned while dropping PUBLISH"
                );
                return;
            }
        };
        let Some(err) = publish_done_error_on_drop(&state) else {
            return;
        };
        let stream_count = self.stream_count.get();
        drop(state);

        if self
            .stream
            .send(BidiResponse::new(
                message::PublishDone {
                    id: self.info.id,
                    status_code: publish_done_code(&err),
                    stream_count,
                    reason: ReasonPhrase("publish ended".to_string()),
                }
                .into(),
            ))
            .is_err()
        {
            tracing::debug!(
                request_id = self.info.id,
                "PUBLISH request stream already closed; PUBLISH_DONE not sent"
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

    pub fn recv_ok(&mut self, msg: &message::RequestOk) -> Result<(), ServeError> {
        let forward = msg
            .params
            .forward()
            .map_err(|_| ServeError::internal_ctx("invalid FORWARD in PUBLISH acceptance"))?;

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

        recv.recv_ok(&message::RequestOk { id: 0, params }).unwrap();

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
