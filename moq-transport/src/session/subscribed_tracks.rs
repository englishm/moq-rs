// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    collections::HashMap,
    ops,
    sync::{Arc, Mutex},
};

use crate::{
    coding::{KeyValuePairs, ReasonPhrase, TrackName, TrackNamespace, TrackNamespacePrefix},
    message::{self, Message, RequestErrorCode},
    mlog,
    serve::ServeError,
    watch::State,
};

use super::{Reader, Session, SessionError, Writer};

const OUTGOING_QUEUE_CAPACITY: usize = 1025;

enum SubscribedTracksOutput {
    Message(Message),
    Reset(u32),
}

#[derive(Debug, Clone)]
pub struct SubscribedTracksInfo {
    pub request_id: u64,
    pub namespace_prefix: TrackNamespacePrefix,
    pub forward: bool,
    pub params: KeyValuePairs,
}

struct SubscribedTracksState {
    responded: bool,
    closed: Result<(), ServeError>,
}

impl Default for SubscribedTracksState {
    fn default() -> Self {
        Self {
            responded: false,
            closed: Ok(()),
        }
    }
}

#[must_use = "rejects SUBSCRIBE_TRACKS on drop if not accepted"]
pub struct SubscribedTracks {
    state: State<SubscribedTracksState>,
    outgoing: tokio::sync::mpsc::Sender<SubscribedTracksOutput>,
    pub info: SubscribedTracksInfo,
}

impl SubscribedTracks {
    pub(super) fn new(
        info: SubscribedTracksInfo,
        active_prefixes: Arc<Mutex<HashMap<u64, TrackNamespacePrefix>>>,
    ) -> (Self, SubscribedTracksRecv) {
        let (send_state, recv_state) = State::default().split();
        let (outgoing, incoming) = tokio::sync::mpsc::channel(OUTGOING_QUEUE_CAPACITY);
        let request_id = info.request_id;
        (
            Self {
                state: send_state,
                outgoing,
                info,
            },
            SubscribedTracksRecv {
                state: recv_state,
                outgoing: incoming,
                active_prefixes,
                request_id,
            },
        )
    }

    pub fn ok(&mut self) -> Result<(), ServeError> {
        self.responded()?;
        self.outgoing
            .try_send(SubscribedTracksOutput::Message(
                message::RequestOk {
                    id: self.request_id,
                    params: Default::default(),
                }
                .into(),
            ))
            .map_err(|_| ServeError::Cancel)
    }

    pub fn reject(mut self, error_code: u64, reason: &str) -> Result<(), ServeError> {
        self.responded()?;
        self.send_error(error_code, reason)
    }

    pub fn terminate(&mut self, error_code: u32) -> Result<(), ServeError> {
        {
            let state = self.state.lock();
            if !state.responded {
                return Err(ServeError::internal_ctx(
                    "SUBSCRIBE_TRACKS terminated before a response",
                ));
            }
            state.closed.clone()?;
        }
        self.outgoing
            .try_send(SubscribedTracksOutput::Reset(error_code))
            .map_err(|_| ServeError::Cancel)
    }

    pub fn publish_blocked(
        &mut self,
        namespace: &TrackNamespace,
        track_name: &TrackName,
    ) -> Result<(), ServeError> {
        let suffix = namespace
            .strip_prefix(&self.namespace_prefix)
            .ok_or_else(|| ServeError::internal_ctx("track does not match subscribed prefix"))?;
        {
            let state = self.state.lock();
            state.closed.clone()?;
            if !state.responded {
                return Err(ServeError::internal_ctx(
                    "PUBLISH_BLOCKED before SUBSCRIBE_TRACKS accepted",
                ));
            }
        }
        self.outgoing
            .try_send(SubscribedTracksOutput::Message(
                message::PublishBlocked {
                    track_namespace_suffix: suffix,
                    track_name: track_name.clone(),
                }
                .into(),
            ))
            .map_err(|_| ServeError::Cancel)
    }

    pub async fn closed(&self) -> Result<(), ServeError> {
        loop {
            {
                let state = self.state.lock();
                state.closed.clone()?;
                match state.modified() {
                    Some(modified) => modified,
                    None => return Ok(()),
                }
            }
            .await;
        }
    }

    fn responded(&mut self) -> Result<(), ServeError> {
        let state = self.state.lock();
        if state.responded {
            return Err(ServeError::Duplicate);
        }
        state.closed.clone()?;
        let mut state = state.into_mut().ok_or(ServeError::Cancel)?;
        state.responded = true;
        Ok(())
    }

    fn send_error(&self, error_code: u64, reason: &str) -> Result<(), ServeError> {
        self.outgoing
            .try_send(SubscribedTracksOutput::Message(request_error(
                self.request_id,
                error_code,
                reason,
            )))
            .map_err(|_| ServeError::Cancel)
    }
}

impl Drop for SubscribedTracks {
    fn drop(&mut self) {
        let should_reject = {
            let state = self.state.lock();
            !state.responded && state.closed.is_ok()
        };
        if should_reject {
            let _ = self
                .outgoing
                .try_send(SubscribedTracksOutput::Message(request_error(
                    self.request_id,
                    RequestErrorCode::DoesNotExist as u64,
                    "not handled",
                )));
        }
    }
}

impl ops::Deref for SubscribedTracks {
    type Target = SubscribedTracksInfo;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

pub(super) struct SubscribedTracksRecv {
    state: State<SubscribedTracksState>,
    outgoing: tokio::sync::mpsc::Receiver<SubscribedTracksOutput>,
    active_prefixes: Arc<Mutex<HashMap<u64, TrackNamespacePrefix>>>,
    request_id: u64,
}

impl SubscribedTracksRecv {
    pub(super) fn rejected(request_id: u64, error_code: u64, reason: &str) -> Self {
        let (outgoing, incoming) = tokio::sync::mpsc::channel(1);
        let _ = outgoing.try_send(SubscribedTracksOutput::Message(request_error(
            request_id, error_code, reason,
        )));
        Self {
            state: State::default(),
            outgoing: incoming,
            active_prefixes: Default::default(),
            request_id,
        }
    }

    pub async fn run(
        mut self,
        mut writer: Writer,
        mut reader: Reader,
        _mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        loop {
            tokio::select! {
                output = self.outgoing.recv() => {
                    let Some(output) = output else {
                        return finish_response(&mut writer);
                    };
                    match output {
                        SubscribedTracksOutput::Reset(code) => {
                            writer.reset(code);
                            reader.stop(code);
                            self.recv_cancel();
                            return Ok(());
                        }
                        SubscribedTracksOutput::Message(message) => {
                            let terminal = matches!(message, Message::RequestError(_));
                            match Session::encode_bidi_response(&mut writer, &message).await {
                                Ok(()) if terminal => return finish_response(&mut writer),
                                Ok(()) => {}
                                Err(error) if error.is_stream_error() => {
                                    self.recv_cancel();
                                    return Ok(());
                                }
                                Err(error) => return Err(error),
                            }
                        }
                    }
                }
                done = reader.done() => {
                    match done {
                        Ok(true) => {
                            self.recv_cancel();
                            return finish_response(&mut writer);
                        }
                        Ok(false) => return Err(SessionError::ProtocolViolation(
                            "unexpected data after SUBSCRIBE_TRACKS request".to_string(),
                        )),
                        Err(error) if error.is_stream_error() => {
                            self.recv_cancel();
                            return Ok(());
                        }
                        Err(error) => return Err(error),
                    }
                }
                stopped = writer.closed() => {
                    let _ = stopped?;
                    reader.stop(super::CANCELLED_STREAM_CODE);
                    self.recv_cancel();
                    return Ok(());
                }
            }
        }
    }

    fn recv_cancel(&mut self) {
        if let Some(mut state) = self.state.lock_mut() {
            state.closed = Err(ServeError::Cancel);
        }
        self.outgoing.close();
    }
}

impl Drop for SubscribedTracksRecv {
    fn drop(&mut self) {
        if let Ok(mut prefixes) = self.active_prefixes.lock() {
            prefixes.remove(&self.request_id);
        }
    }
}

fn request_error(id: u64, error_code: u64, reason: &str) -> Message {
    message::RequestError {
        id,
        error_code,
        retry_interval: 0,
        reason: ReasonPhrase(reason.to_string()),
    }
    .into()
}

fn finish_response(writer: &mut Writer) -> Result<(), SessionError> {
    match writer.finish() {
        Ok(()) => Ok(()),
        Err(error) if error.is_stream_error() => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::loopback_session_pair;

    fn make() -> (
        SubscribedTracks,
        SubscribedTracksRecv,
        Arc<Mutex<HashMap<u64, TrackNamespacePrefix>>>,
    ) {
        let prefix = TrackNamespacePrefix::from_utf8_path("room/123");
        let active = Arc::new(Mutex::new(HashMap::from([(0, prefix.clone())])));
        let (send, recv) = SubscribedTracks::new(
            SubscribedTracksInfo {
                request_id: 0,
                namespace_prefix: prefix,
                forward: true,
                params: KeyValuePairs::default(),
            },
            active.clone(),
        );
        (send, recv, active)
    }

    #[tokio::test]
    async fn ok_queues_request_ok() {
        let (mut request, mut recv, _) = make();
        request.ok().unwrap();
        assert!(matches!(
            recv.outgoing.recv().await,
            Some(SubscribedTracksOutput::Message(Message::RequestOk(_)))
        ));
    }

    #[test]
    fn duplicate_response_is_rejected() {
        let (mut request, _recv, _) = make();
        request.ok().unwrap();
        assert!(matches!(request.ok(), Err(ServeError::Duplicate)));
    }

    #[tokio::test]
    async fn rejection_queues_request_error() {
        let (request, mut recv, _) = make();
        request
            .reject(RequestErrorCode::Unauthorized as u64, "denied")
            .unwrap();
        let Some(SubscribedTracksOutput::Message(Message::RequestError(error))) =
            recv.outgoing.recv().await
        else {
            panic!("expected REQUEST_ERROR");
        };
        assert_eq!(error.error_code, RequestErrorCode::Unauthorized as u64);
    }

    #[test]
    fn receiver_drop_releases_pending_prefix() {
        let (_request, recv, active) = make();
        drop(recv);
        assert!(active.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn accepted_request_termination_queues_stream_reset() {
        let (mut request, mut recv, _) = make();
        request.ok().unwrap();
        let _ = recv.outgoing.recv().await;

        request
            .terminate(RequestErrorCode::ExcessiveLoad as u32)
            .unwrap();

        assert!(matches!(
            recv.outgoing.recv().await,
            Some(SubscribedTracksOutput::Reset(code))
                if code == RequestErrorCode::ExcessiveLoad as u32
        ));
    }

    #[tokio::test]
    async fn blocked_uses_relative_namespace() {
        let (mut request, mut recv, _) = make();
        request.ok().unwrap();
        let _ = recv.outgoing.recv().await;
        request
            .publish_blocked(
                &TrackNamespace::from_utf8_path("room/123/participant/5"),
                &TrackName::from("video"),
            )
            .unwrap();
        let Some(SubscribedTracksOutput::Message(Message::PublishBlocked(blocked))) =
            recv.outgoing.recv().await
        else {
            panic!("expected PUBLISH_BLOCKED");
        };
        assert_eq!(
            blocked.track_namespace_suffix.to_utf8_path(),
            "/participant/5"
        );
        assert_eq!(blocked.track_name, TrackName::from("video"));
    }

    #[tokio::test]
    async fn requester_fin_cancels_and_releases_prefix() {
        let (request_session, response_session) = loopback_session_pair().await;
        let (request_send, request_recv) = request_session.open_bi().await.unwrap();
        let (response_send, response_recv) = response_session.accept_bi().await.unwrap();
        let mut request_writer = Writer::new(request_send);
        let mut response_reader = Reader::new(request_recv);
        let (mut request, recv, active) = make();
        let driver =
            tokio::spawn(recv.run(Writer::new(response_send), Reader::new(response_recv), None));

        request.ok().unwrap();
        assert!(matches!(
            Session::decode_bidi_response(&mut response_reader, request.request_id)
                .await
                .unwrap(),
            Message::RequestOk(_)
        ));

        request_writer.finish().unwrap();
        assert!(matches!(request.closed().await, Err(ServeError::Cancel)));
        driver.await.unwrap().unwrap();
        assert!(active.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn requester_reset_cancels_and_releases_prefix() {
        let (request_session, response_session) = loopback_session_pair().await;
        let (request_send, request_recv) = request_session.open_bi().await.unwrap();
        let (response_send, response_recv) = response_session.accept_bi().await.unwrap();
        let mut request_writer = Writer::new(request_send);
        let mut response_reader = Reader::new(request_recv);
        let (mut request, recv, active) = make();
        let driver =
            tokio::spawn(recv.run(Writer::new(response_send), Reader::new(response_recv), None));

        request.ok().unwrap();
        assert!(matches!(
            Session::decode_bidi_response(&mut response_reader, request.request_id)
                .await
                .unwrap(),
            Message::RequestOk(_)
        ));

        request_writer.reset(super::super::CANCELLED_STREAM_CODE);
        assert!(matches!(request.closed().await, Err(ServeError::Cancel)));
        driver.await.unwrap().unwrap();
        assert!(active.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn requester_stop_sending_cancels_and_releases_prefix() {
        let (request_session, response_session) = loopback_session_pair().await;
        let (_request_send, request_recv) = request_session.open_bi().await.unwrap();
        let (response_send, response_recv) = response_session.accept_bi().await.unwrap();
        let mut response_reader = Reader::new(request_recv);
        let (mut request, recv, active) = make();
        let driver =
            tokio::spawn(recv.run(Writer::new(response_send), Reader::new(response_recv), None));

        request.ok().unwrap();
        assert!(matches!(
            Session::decode_bidi_response(&mut response_reader, request.request_id)
                .await
                .unwrap(),
            Message::RequestOk(_)
        ));

        response_reader.stop(super::super::CANCELLED_STREAM_CODE);
        assert!(matches!(request.closed().await, Err(ServeError::Cancel)));
        driver.await.unwrap().unwrap();
        assert!(active.lock().unwrap().is_empty());
    }
}
