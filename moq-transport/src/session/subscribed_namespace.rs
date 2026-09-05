// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Inbound SUBSCRIBE_NAMESPACE handling.

use std::{
    collections::HashMap,
    ops,
    sync::{Arc, Mutex},
};

use crate::{
    coding::{ReasonPhrase, TrackNamespace, TrackNamespacePrefix},
    message::{self, Message, RequestErrorCode, SubscribeOptions},
    mlog,
    serve::ServeError,
    watch::State,
};

use super::{Reader, SessionError, Writer};

/// Capacity of the per-subscription outbound message queue. Bounds memory when a
/// downstream reader is slow: rather than buffering namespace updates without
/// limit under sustained churn, a full queue tears the subscription down (the peer
/// can re-subscribe and receive a fresh snapshot).
const OUTGOING_QUEUE_CAPACITY: usize = 8192;

#[derive(Debug, Clone)]
pub struct SubscribedNamespaceInfo {
    pub request_id: u64,
    pub namespace_prefix: TrackNamespacePrefix,
    pub subscribe_options: SubscribeOptions,
    pub forward: bool,
}

struct SubscribedNamespaceState {
    responded: bool,
    closed: Result<(), ServeError>,
    namespaces: std::collections::HashSet<TrackNamespacePrefix>,
}

impl Default for SubscribedNamespaceState {
    fn default() -> Self {
        Self {
            responded: false,
            closed: Ok(()),
            namespaces: Default::default(),
        }
    }
}

#[must_use = "rejects SUBSCRIBE_NAMESPACE on drop if not accepted"]
pub struct SubscribedNamespace {
    state: State<SubscribedNamespaceState>,
    outgoing: tokio::sync::mpsc::Sender<Message>,

    pub info: SubscribedNamespaceInfo,
}

impl SubscribedNamespace {
    pub(super) fn new(
        info: SubscribedNamespaceInfo,
        active_prefixes: Arc<Mutex<HashMap<u64, TrackNamespacePrefix>>>,
    ) -> (Self, SubscribedNamespaceRecv) {
        let (send_state, recv_state) = State::default().split();
        let (send_queue, recv_queue) = tokio::sync::mpsc::channel(OUTGOING_QUEUE_CAPACITY);
        let send = Self {
            state: send_state,
            outgoing: send_queue,
            info,
        };
        let recv = SubscribedNamespaceRecv {
            state: recv_state,
            outgoing: recv_queue,
            active_prefixes,
            request_id: send.info.request_id,
        };

        (send, recv)
    }

    pub fn ok(&mut self) -> Result<(), ServeError> {
        self.responded()?;
        self.outgoing
            .try_send(
                message::RequestOk {
                    id: self.info.request_id,
                    params: Default::default(),
                }
                .into(),
            )
            .map_err(|_| ServeError::Cancel)
    }

    pub fn reject(mut self, error_code: u64, reason: &str) -> Result<(), ServeError> {
        self.responded()?;
        self.outgoing
            .try_send(request_error(self.info.request_id, error_code, reason))
            .map_err(|_| ServeError::Cancel)
    }

    pub fn namespace(&mut self, namespace: &TrackNamespace) -> Result<(), ServeError> {
        let suffix = namespace
            .strip_prefix(&self.info.namespace_prefix)
            .ok_or_else(|| {
                ServeError::internal_ctx("namespace does not match subscribed prefix")
            })?;

        {
            let mut state = self.state.lock().into_mut().ok_or(ServeError::Cancel)?;
            state.closed.clone()?;
            if !state.responded {
                return Err(ServeError::internal_ctx(
                    "NAMESPACE before SUBSCRIBE_NAMESPACE accepted",
                ));
            }
            state.namespaces.insert(suffix.clone());
        }

        self.outgoing
            .try_send(
                message::Namespace {
                    track_namespace_suffix: suffix,
                }
                .into(),
            )
            .map_err(|_| ServeError::Cancel)
    }

    pub fn namespace_done(&mut self, namespace: &TrackNamespace) -> Result<(), ServeError> {
        let suffix = namespace
            .strip_prefix(&self.info.namespace_prefix)
            .ok_or_else(|| {
                ServeError::internal_ctx("namespace does not match subscribed prefix")
            })?;

        {
            let mut state = self.state.lock().into_mut().ok_or(ServeError::Cancel)?;
            state.closed.clone()?;
            if !state.responded {
                return Err(ServeError::internal_ctx(
                    "NAMESPACE_DONE before SUBSCRIBE_NAMESPACE accepted",
                ));
            }
            if !state.namespaces.remove(&suffix) {
                return Err(ServeError::internal_ctx(
                    "NAMESPACE_DONE before corresponding NAMESPACE",
                ));
            }
        }

        self.outgoing
            .try_send(
                message::NamespaceDone {
                    track_namespace_suffix: suffix,
                }
                .into(),
            )
            .map_err(|_| ServeError::Cancel)
    }

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
}

impl Drop for SubscribedNamespace {
    fn drop(&mut self) {
        let should_reject = {
            let state = self.state.lock();
            !state.responded && state.closed.is_ok()
        };

        if should_reject {
            let _ = self.outgoing.try_send(request_error(
                self.info.request_id,
                RequestErrorCode::DoesNotExist as u64,
                "not handled",
            ));
        }
    }
}

impl ops::Deref for SubscribedNamespace {
    type Target = SubscribedNamespaceInfo;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

pub(super) struct SubscribedNamespaceRecv {
    state: State<SubscribedNamespaceState>,
    outgoing: tokio::sync::mpsc::Receiver<Message>,
    active_prefixes: Arc<Mutex<HashMap<u64, TrackNamespacePrefix>>>,
    request_id: u64,
}

impl SubscribedNamespaceRecv {
    pub(super) fn rejected(request_id: u64, error_code: u64, reason: &str) -> Self {
        let (send_queue, recv_queue) = tokio::sync::mpsc::channel(OUTGOING_QUEUE_CAPACITY);
        let _ = send_queue.try_send(request_error(request_id, error_code, reason));
        Self {
            state: State::default(),
            outgoing: recv_queue,
            active_prefixes: Default::default(),
            request_id,
        }
    }

    pub async fn run(
        mut self,
        mut writer: Writer,
        mut reader: Reader,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        loop {
            tokio::select! {
                msg = self.outgoing.recv() => {
                    let Some(msg) = msg else {
                        return self.finish_response(&mut writer);
                    };
                    if !self.send_message(&mut writer, &mlog, msg).await? {
                        self.recv_cancel();
                        return Ok(());
                    }
                }
                done = reader.done() => {
                    match done {
                        Ok(true) => {
                            self.recv_cancel();
                            while let Some(msg) = self.outgoing.recv().await {
                                if !self.send_message(&mut writer, &mlog, msg).await? {
                                    return Ok(());
                                }
                            }
                            return self.finish_response(&mut writer);
                        }
                        Ok(false) => {
                            return Err(SessionError::ProtocolViolation(
                                "unexpected data after SUBSCRIBE_NAMESPACE request".to_string(),
                            ));
                        }
                        Err(err) => {
                            tracing::debug!(
                                session_id = %writer.session_id(),
                                request_id = self.request_id,
                                error = %err,
                                "SUBSCRIBE_NAMESPACE request stream closed with error"
                            );
                            self.recv_cancel();
                            if err.is_stream_error() {
                                return Ok(());
                            }
                            return Err(err);
                        }
                    }
                }
            }
        }
    }

    async fn send_message(
        &self,
        writer: &mut Writer,
        mlog: &Option<Arc<Mutex<mlog::MlogWriter>>>,
        msg: Message,
    ) -> Result<bool, SessionError> {
        self.emit_mlog(mlog, &msg);
        // This stream bypasses Session::log_control_message, so without this
        // a REQUEST_ERROR sent here would reach the peer with no log record at
        // all — nothing to correlate against the id the client is quoting.
        if let Message::RequestError(ref m) = msg {
            tracing::debug!(
                target: "moq_transport::control",
                session_id = %writer.session_id(),
                direction = "sent",
                msg_type = "REQUEST_ERROR",
                request_id = m.id,
                error_code = m.error_code,
                // The reason is peer-supplied (up to 1024 bytes, arbitrary UTF-8).
                // Log a sanitized form (control chars → '?') and the raw hex so
                // analysts can reconstruct the exact bytes without log-injection risk.
                reason_lossy = ?super::SanitizedDisplay(&m.reason.0).to_string(),
                reason_hex = %super::HexDisplay(&m.reason.0),
                "MoQT control message"
            );
        }
        match writer.encode(&msg).await {
            Ok(()) => Ok(true),
            Err(err) if err.is_stream_error() => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn finish_response(&self, writer: &mut Writer) -> Result<(), SessionError> {
        match writer.finish() {
            Ok(()) => Ok(()),
            Err(err) if err.is_stream_error() => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn emit_mlog(&self, mlog: &Option<Arc<Mutex<mlog::MlogWriter>>>, msg: &Message) {
        if let Some(mlog) = mlog {
            if let Ok(mut mlog) = mlog.lock() {
                let time = mlog.elapsed_ms();
                let event = match msg {
                    Message::RequestOk(msg) => Some(mlog::events::request_ok_created(
                        time,
                        0,
                        "subscribe_namespace",
                        msg,
                    )),
                    Message::RequestError(msg) => Some(mlog::events::request_error_created(
                        time,
                        0,
                        "subscribe_namespace",
                        msg,
                    )),
                    Message::Namespace(msg) => Some(mlog::events::namespace_created(time, 0, msg)),
                    Message::NamespaceDone(msg) => {
                        Some(mlog::events::namespace_done_created(time, 0, msg))
                    }
                    _ => None,
                };
                if let Some(event) = event {
                    let _ = mlog.add_event(event);
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

    fn remove_active_prefix(&self) {
        if let Ok(mut prefixes) = self.active_prefixes.lock() {
            prefixes.remove(&self.request_id);
        }
    }
}

impl Drop for SubscribedNamespaceRecv {
    fn drop(&mut self) {
        self.remove_active_prefix();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> SubscribedNamespaceInfo {
        SubscribedNamespaceInfo {
            request_id: 0,
            namespace_prefix: TrackNamespacePrefix::from_utf8_path("example.com/meeting=123"),
            subscribe_options: SubscribeOptions::Namespace,
            forward: true,
        }
    }

    fn make() -> (
        SubscribedNamespace,
        SubscribedNamespaceRecv,
        Arc<Mutex<HashMap<u64, TrackNamespacePrefix>>>,
    ) {
        let active = Arc::new(Mutex::new(HashMap::new()));
        active
            .lock()
            .unwrap()
            .insert(0, info().namespace_prefix.clone());
        let (send, recv) = SubscribedNamespace::new(info(), active.clone());
        (send, recv, active)
    }

    #[tokio::test]
    async fn ok_queues_request_ok() {
        let (mut send, mut recv, _active) = make();

        send.ok().unwrap();

        assert!(matches!(
            recv.outgoing.recv().await,
            Some(Message::RequestOk(_))
        ));
    }

    #[test]
    fn ok_twice_is_duplicate() {
        let (mut send, _recv, _active) = make();

        send.ok().unwrap();
        assert!(matches!(send.ok(), Err(ServeError::Duplicate)));
    }

    #[tokio::test]
    async fn namespace_queues_suffix() {
        let (mut send, mut recv, _active) = make();
        let namespace = TrackNamespace::from_utf8_path("example.com/meeting=123/participant=100");

        send.ok().unwrap();
        let _ = recv.outgoing.recv().await;
        send.namespace(&namespace).unwrap();

        let Some(Message::Namespace(msg)) = recv.outgoing.recv().await else {
            panic!("expected NAMESPACE");
        };
        assert_eq!(
            msg.track_namespace_suffix.to_utf8_path(),
            "/participant=100"
        );
    }

    #[test]
    fn namespace_done_before_namespace_is_error() {
        let (mut send, _recv, _active) = make();
        let namespace = TrackNamespace::from_utf8_path("example.com/meeting=123/participant=100");

        assert!(send.namespace_done(&namespace).is_err());
    }

    #[tokio::test]
    async fn drop_before_ok_queues_request_error_and_removes_prefix() {
        let (send, mut recv, active) = make();

        drop(send);

        assert!(matches!(
            recv.outgoing.recv().await,
            Some(Message::RequestError(_))
        ));
        assert!(active.lock().unwrap().contains_key(&0));

        drop(recv);
        assert!(!active.lock().unwrap().contains_key(&0));
    }

    #[tokio::test]
    async fn cancel_keeps_active_prefix_until_receiver_drops() {
        let (send, mut recv, active) = make();

        recv.recv_cancel();

        assert!(matches!(send.closed().await, Err(ServeError::Cancel)));
        assert!(active.lock().unwrap().contains_key(&0));

        drop(recv);
        assert!(!active.lock().unwrap().contains_key(&0));
    }

    #[tokio::test]
    async fn graceful_cancel_preserves_queued_responses() {
        let (mut send, mut recv, _active) = make();
        let namespace = TrackNamespace::from_utf8_path("example.com/meeting=123/participant=100");
        send.ok().unwrap();
        send.namespace(&namespace).unwrap();

        recv.recv_cancel();

        assert!(matches!(
            recv.outgoing.recv().await,
            Some(Message::RequestOk(_))
        ));
        assert!(matches!(
            recv.outgoing.recv().await,
            Some(Message::Namespace(_))
        ));
        assert!(recv.outgoing.recv().await.is_none());
        assert!(matches!(
            send.namespace(&namespace),
            Err(ServeError::Cancel)
        ));
    }
}
