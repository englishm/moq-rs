// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Outbound SUBSCRIBE_NAMESPACE handling.

use std::{
    collections::HashSet,
    ops,
    sync::{Arc, Mutex},
};

use futures::channel::oneshot;

use crate::{
    coding::{KeyValuePairs, TrackNamespace, TrackNamespacePrefix},
    message::{self, Message, SubscribeOptions},
    mlog,
    serve::ServeError,
    watch::State,
};

use super::{Reader, Session, SessionError, Subscriber, Writer};

/// Safety bound on the number of distinct namespaces tracked for a single
/// SUBSCRIBE_NAMESPACE response stream. A misbehaving upstream could otherwise
/// send unbounded NAMESPACE additions and exhaust memory; exceeding it closes the
/// stream with a protocol violation. Chosen high enough not to affect legitimate
/// broad-prefix discovery.
const MAX_KNOWN_NAMESPACE_SUFFIXES: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct SubscribeNamespaceInfo {
    pub request_id: u64,
    pub namespace_prefix: TrackNamespacePrefix,
    pub subscribe_options: SubscribeOptions,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum NamespaceEvent {
    Added(TrackNamespace),
    Removed(TrackNamespace),
}

struct SubscribeNamespaceState {
    ok: bool,
    events: std::collections::VecDeque<NamespaceEvent>,
    closed: Result<(), ServeError>,
}

impl Default for SubscribeNamespaceState {
    fn default() -> Self {
        Self {
            ok: false,
            events: Default::default(),
            closed: Ok(()),
        }
    }
}

/// Outbound SUBSCRIBE_NAMESPACE request.
///
/// This handle only exposes `NAMESPACE` / `NAMESPACE_DONE` updates from the
/// request's dedicated response stream. If the request used
/// `SubscribeOptions::Publish` or `SubscribeOptions::Both`, matching `PUBLISH`
/// messages arrive on the session control stream (§9.25) and are surfaced via
/// [`Subscriber::publish_received`].
#[must_use = "cancels SUBSCRIBE_NAMESPACE on drop"]
pub struct SubscribeNamespace {
    state: State<SubscribeNamespaceState>,
    // Keep the request half after FIN so a cancellation timeout can still reset it (§6.1).
    writer: Option<Writer>,
    request_finished: bool,
    force_reset: Option<oneshot::Sender<u32>>,

    pub info: SubscribeNamespaceInfo,
}

impl SubscribeNamespace {
    pub(super) fn new(
        subscriber: Subscriber,
        info: SubscribeNamespaceInfo,
        writer: Writer,
    ) -> (Self, SubscribeNamespaceRecv) {
        let (send_state, recv_state) = State::default().split();
        let (force_reset, recv_force_reset) = oneshot::channel();
        let recv = SubscribeNamespaceRecv {
            state: recv_state,
            request_id: info.request_id,
            namespace_prefix: info.namespace_prefix.clone(),
            responded: false,
            known_suffixes: HashSet::default(),
            max_known_suffixes: MAX_KNOWN_NAMESPACE_SUFFIXES,
            subscriber,
            force_reset: Some(recv_force_reset),
        };
        let send = Self {
            state: send_state,
            writer: Some(writer),
            request_finished: false,
            force_reset: Some(force_reset),
            info,
        };

        (send, recv)
    }

    pub async fn ok(&self) -> Result<(), ServeError> {
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

    /// Cancel the request gracefully with FIN while continuing to receive its response.
    pub fn finish_request(&mut self) -> Result<(), SessionError> {
        if self.request_finished {
            return Ok(());
        }

        self.request_finished = true;
        match self.writer.as_mut() {
            Some(writer) => writer.finish(),
            None => Ok(()),
        }
    }

    /// Force both halves of this request stream closed without closing the session.
    pub fn reset_request(&mut self, code: u32) {
        let Some(force_reset) = self.force_reset.take() else {
            return;
        };

        let _ = force_reset.send(code);
        if let Some(mut writer) = self.writer.take() {
            writer.reset(code);
        }
    }

    pub async fn next(&self) -> Result<Option<NamespaceEvent>, ServeError> {
        loop {
            {
                let state = self.state.lock();
                if !state.events.is_empty() {
                    return Ok(state.into_mut_closed().events.pop_front());
                }

                state.closed.clone()?;
                match state.modified() {
                    Some(notify) => notify,
                    None => return Ok(None),
                }
            }
            .await;
        }
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
}

impl ops::Deref for SubscribeNamespace {
    type Target = SubscribeNamespaceInfo;

    fn deref(&self) -> &Self::Target {
        &self.info
    }
}

pub(super) struct SubscribeNamespaceRecv {
    state: State<SubscribeNamespaceState>,
    request_id: u64,
    namespace_prefix: TrackNamespacePrefix,
    responded: bool,
    known_suffixes: HashSet<TrackNamespacePrefix>,
    max_known_suffixes: usize,
    subscriber: Subscriber,
    force_reset: Option<oneshot::Receiver<u32>>,
}

impl SubscribeNamespaceRecv {
    pub async fn run(
        mut self,
        mut reader: Reader,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        let mut force_reset = self.force_reset.take().ok_or(SessionError::Internal)?;

        tokio::select! {
            biased;
            reset = &mut force_reset => {
                if let Ok(code) = reset {
                    reader.stop(code);
                }
                Ok(())
            }
            result = self.run_response(&mut reader, &mlog) => {
                match result {
                    Err(err) if err.is_stream_error() => Ok(()),
                    result => result,
                }
            }
        }
    }

    async fn run_response(
        &mut self,
        reader: &mut Reader,
        mlog: &Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        loop {
            if self.responded && reader.done().await? {
                return Ok(());
            }

            let msg = reader.decode::<Message>().await?;
            Session::log_control_message(reader.session_id(), &msg, "recv");
            self.emit_mlog(mlog, &msg);
            if !self.recv_message(msg)? {
                return Ok(());
            }
        }
    }

    fn emit_mlog(&self, mlog: &Option<Arc<Mutex<mlog::MlogWriter>>>, msg: &Message) {
        if let Some(mlog) = mlog {
            if let Ok(mut mlog) = mlog.lock() {
                let time = mlog.elapsed_ms();
                let event = match msg {
                    Message::RequestOk(msg) => Some(mlog::events::request_ok_parsed(
                        time,
                        0,
                        "subscribe_namespace",
                        msg,
                    )),
                    Message::RequestError(msg) => Some(mlog::events::request_error_parsed(
                        time,
                        0,
                        "subscribe_namespace",
                        msg,
                    )),
                    Message::Namespace(msg) => Some(mlog::events::namespace_parsed(time, 0, msg)),
                    Message::NamespaceDone(msg) => {
                        Some(mlog::events::namespace_done_parsed(time, 0, msg))
                    }
                    _ => None,
                };
                if let Some(event) = event {
                    let _ = mlog.add_event(event);
                }
            }
        }
    }

    fn recv_message(&mut self, msg: Message) -> Result<bool, SessionError> {
        match msg {
            Message::RequestOk(msg) if !self.responded => {
                self.check_response_id(msg.id)?;
                self.responded = true;
                self.recv_ok();
                Ok(true)
            }
            Message::RequestError(msg) if !self.responded => {
                self.check_response_id(msg.id)?;
                self.responded = true;
                self.recv_error(ServeError::Closed(msg.error_code));
                Ok(false)
            }
            Message::Namespace(msg) if self.responded => self.recv_namespace(msg),
            Message::NamespaceDone(msg) if self.responded => self.recv_namespace_done(msg),
            Message::RequestOk(_) | Message::RequestError(_) => {
                Err(SessionError::ProtocolViolation(
                    "SUBSCRIBE_NAMESPACE response stream received multiple request responses"
                        .to_string(),
                ))
            }
            other => Err(SessionError::ProtocolViolation(format!(
                "unexpected {} on SUBSCRIBE_NAMESPACE response stream",
                other.name()
            ))),
        }
    }

    fn check_response_id(&self, id: u64) -> Result<(), SessionError> {
        if id == self.request_id {
            return Ok(());
        }

        Err(SessionError::ProtocolViolation(
            "SUBSCRIBE_NAMESPACE response request ID mismatch".to_string(),
        ))
    }

    fn recv_ok(&mut self) {
        if let Some(mut state) = self.state.lock_mut() {
            state.ok = true;
        }
    }

    fn recv_error(&mut self, err: ServeError) {
        if let Some(mut state) = self.state.lock_mut() {
            state.closed = Err(err);
        }
    }

    fn recv_namespace(&mut self, msg: message::Namespace) -> Result<bool, SessionError> {
        let namespace = self
            .namespace_prefix
            .join_suffix(&msg.track_namespace_suffix)
            .map_err(|err| {
                SessionError::ProtocolViolation(format!(
                    "invalid NAMESPACE suffix for SUBSCRIBE_NAMESPACE: {}",
                    err
                ))
            })?;

        if !self.known_suffixes.contains(&msg.track_namespace_suffix)
            && self.known_suffixes.len() >= self.max_known_suffixes
        {
            return Err(SessionError::ProtocolViolation(format!(
                "SUBSCRIBE_NAMESPACE response exceeded {} active namespaces",
                self.max_known_suffixes
            )));
        }

        self.known_suffixes.insert(msg.track_namespace_suffix);
        let Some(mut state) = self.state.lock_mut() else {
            return Ok(false);
        };
        state.events.push_back(NamespaceEvent::Added(namespace));
        Ok(true)
    }

    fn recv_namespace_done(&mut self, msg: message::NamespaceDone) -> Result<bool, SessionError> {
        if !self.known_suffixes.remove(&msg.track_namespace_suffix) {
            return Err(SessionError::ProtocolViolation(
                "NAMESPACE_DONE received before corresponding NAMESPACE".to_string(),
            ));
        }

        let namespace = self
            .namespace_prefix
            .join_suffix(&msg.track_namespace_suffix)
            .map_err(|err| {
                SessionError::ProtocolViolation(format!(
                    "invalid NAMESPACE_DONE suffix for SUBSCRIBE_NAMESPACE: {}",
                    err
                ))
            })?;

        let Some(mut state) = self.state.lock_mut() else {
            return Ok(false);
        };
        state.events.push_back(NamespaceEvent::Removed(namespace));
        Ok(true)
    }
}

impl Drop for SubscribeNamespaceRecv {
    fn drop(&mut self) {
        self.subscriber.remove_subscribe_namespace(self.request_id);
    }
}

pub(super) struct OpenSubscribeNamespace {
    pub subscriber: Subscriber,
    pub info: SubscribeNamespaceInfo,
    pub message: message::SubscribeNamespace,
    pub reply: oneshot::Sender<Result<SubscribeNamespace, SessionError>>,
}

impl OpenSubscribeNamespace {
    pub fn new(
        subscriber: Subscriber,
        request_id: u64,
        namespace_prefix: TrackNamespacePrefix,
        subscribe_options: SubscribeOptions,
        params: KeyValuePairs,
        reply: oneshot::Sender<Result<SubscribeNamespace, SessionError>>,
    ) -> Self {
        let info = SubscribeNamespaceInfo {
            request_id,
            namespace_prefix: namespace_prefix.clone(),
            subscribe_options,
        };
        let message = message::SubscribeNamespace {
            id: request_id,
            track_namespace_prefix: namespace_prefix,
            subscribe_options,
            params,
        };

        Self {
            subscriber,
            info,
            message,
            reply,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        message::RequestOk, session::PendingRequests, session::RequestId, session::SessionId,
        watch::Queue,
    };

    use super::*;

    fn subscriber() -> Subscriber {
        let request_id = RequestId::new(0, 100, 100, 0);
        Subscriber::new(
            Queue::default(),
            Queue::default(),
            None,
            request_id,
            PendingRequests::default(),
            SessionId::generate(),
        )
    }

    fn recv(prefix: &str) -> SubscribeNamespaceRecv {
        let info = SubscribeNamespaceInfo {
            request_id: 0,
            namespace_prefix: TrackNamespacePrefix::from_utf8_path(prefix),
            subscribe_options: SubscribeOptions::Namespace,
        };
        SubscribeNamespaceRecv {
            state: State::<SubscribeNamespaceState>::default(),
            request_id: info.request_id,
            namespace_prefix: info.namespace_prefix,
            responded: false,
            known_suffixes: HashSet::default(),
            max_known_suffixes: MAX_KNOWN_NAMESPACE_SUFFIXES,
            subscriber: subscriber(),
            force_reset: None,
        }
    }

    fn pair(prefix: &str) -> (SubscribeNamespace, SubscribeNamespaceRecv) {
        let info = SubscribeNamespaceInfo {
            request_id: 0,
            namespace_prefix: TrackNamespacePrefix::from_utf8_path(prefix),
            subscribe_options: SubscribeOptions::Namespace,
        };
        let subscriber = subscriber();
        let (send_state, recv_state) = State::default().split();
        let (force_reset, recv_force_reset) = oneshot::channel();

        (
            SubscribeNamespace {
                state: send_state,
                writer: None,
                request_finished: false,
                force_reset: Some(force_reset),
                info: info.clone(),
            },
            SubscribeNamespaceRecv {
                state: recv_state,
                request_id: info.request_id,
                namespace_prefix: info.namespace_prefix,
                responded: false,
                known_suffixes: HashSet::default(),
                max_known_suffixes: MAX_KNOWN_NAMESPACE_SUFFIXES,
                subscriber,
                force_reset: Some(recv_force_reset),
            },
        )
    }

    #[tokio::test]
    async fn queued_event_is_drained_after_response_fin() {
        let (subscribe, mut recv) = pair("example.com");
        recv.recv_message(Message::RequestOk(RequestOk {
            id: 0,
            params: Default::default(),
        }))
        .unwrap();
        recv.recv_message(Message::Namespace(message::Namespace {
            track_namespace_suffix: TrackNamespacePrefix::from_utf8_path("meeting=123"),
        }))
        .unwrap();

        drop(recv);

        assert_eq!(
            subscribe.next().await.unwrap(),
            Some(NamespaceEvent::Added(TrackNamespace::from_utf8_path(
                "example.com/meeting=123"
            )))
        );
        assert_eq!(subscribe.next().await.unwrap(), None);
    }

    #[tokio::test]
    async fn graceful_finish_keeps_response_half_open() {
        let (mut subscribe, mut recv) = pair("example.com");

        subscribe.finish_request().unwrap();
        subscribe.finish_request().unwrap();

        assert!(subscribe.request_finished);
        assert_eq!(recv.force_reset.as_mut().unwrap().try_recv().unwrap(), None);
    }

    #[tokio::test]
    async fn forced_reset_signals_response_half() {
        let (mut subscribe, mut recv) = pair("example.com");
        let force_reset = recv.force_reset.take().unwrap();

        subscribe.reset_request(42);

        assert_eq!(force_reset.await.unwrap(), 42);
    }

    #[test]
    fn request_ok_marks_subscription_ok() {
        let mut recv = recv("example.com");

        assert!(recv
            .recv_message(Message::RequestOk(RequestOk {
                id: 0,
                params: Default::default(),
            }))
            .unwrap());

        assert!(recv.state.lock().ok);
    }

    #[test]
    fn namespace_event_reconstructs_full_namespace() {
        let mut recv = recv("example.com/meeting=123");
        recv.recv_message(Message::RequestOk(RequestOk {
            id: 0,
            params: Default::default(),
        }))
        .unwrap();

        recv.recv_message(Message::Namespace(message::Namespace {
            track_namespace_suffix: TrackNamespacePrefix::from_utf8_path("participant=100"),
        }))
        .unwrap();

        let event = recv.state.lock_mut().unwrap().events.pop_front().unwrap();
        assert_eq!(
            event,
            NamespaceEvent::Added(TrackNamespace::from_utf8_path(
                "example.com/meeting=123/participant=100"
            ))
        );
    }

    #[test]
    fn namespace_done_before_namespace_is_protocol_violation() {
        let mut recv = recv("example.com");
        recv.recv_message(Message::RequestOk(RequestOk {
            id: 0,
            params: Default::default(),
        }))
        .unwrap();

        let err = recv
            .recv_message(Message::NamespaceDone(message::NamespaceDone {
                track_namespace_suffix: TrackNamespacePrefix::from_utf8_path("meeting=123"),
            }))
            .unwrap_err();

        assert!(matches!(err, SessionError::ProtocolViolation(_)));
    }

    #[test]
    fn second_request_response_is_protocol_violation() {
        let mut recv = recv("example.com");
        recv.recv_message(Message::RequestOk(RequestOk {
            id: 0,
            params: Default::default(),
        }))
        .unwrap();

        let err = recv
            .recv_message(Message::RequestOk(RequestOk {
                id: 0,
                params: Default::default(),
            }))
            .unwrap_err();

        assert!(matches!(err, SessionError::ProtocolViolation(_)));
    }

    #[test]
    fn namespace_additions_are_capped() {
        let mut recv = recv("example.com");
        recv.max_known_suffixes = 2;
        recv.recv_message(Message::RequestOk(RequestOk {
            id: 0,
            params: Default::default(),
        }))
        .unwrap();

        for i in 0..recv.max_known_suffixes {
            recv.recv_message(Message::Namespace(message::Namespace {
                track_namespace_suffix: TrackNamespacePrefix::from_utf8_path(&format!(
                    "meeting={i}"
                )),
            }))
            .expect("additions up to the cap are accepted");
        }

        // A new distinct suffix beyond the cap is rejected as a protocol violation.
        let err = recv
            .recv_message(Message::Namespace(message::Namespace {
                track_namespace_suffix: TrackNamespacePrefix::from_utf8_path("meeting=overflow"),
            }))
            .unwrap_err();
        assert!(matches!(err, SessionError::ProtocolViolation(_)));

        // Re-adding an already-tracked suffix does not grow the set, so it is allowed.
        recv.recv_message(Message::Namespace(message::Namespace {
            track_namespace_suffix: TrackNamespacePrefix::from_utf8_path("meeting=0"),
        }))
        .expect("re-adding a tracked suffix stays within the cap");
    }
}
