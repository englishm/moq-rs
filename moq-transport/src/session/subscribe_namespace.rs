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
    coding::{TrackNamespace, TrackNamespacePrefix},
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

/// Cap on events queued for the application but not yet read. Bounds the memory a
/// peer can make this endpoint hold by churning NAMESPACE and NAMESPACE_DONE for
/// the same namespaces faster than the application drains them.
const MAX_PENDING_NAMESPACE_EVENTS: usize = 1 << 16;

/// Whether the response stream carries on after a message.
///
/// Named rather than a bool because two different questions were being answered
/// by the same `false`: whether a message changed anything worth reporting to the
/// application, and whether the subscription is over. Only the second may end the
/// loop, and draft-18 §14.4 gives that meaning to a FIN or reset of the response
/// stream, not to a message we chose to ignore.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ResponseFlow {
    /// Keep reading the response stream.
    Continue,
    /// The subscription is over: the request was refused, or nothing is left
    /// listening for its events.
    Ended,
}

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

impl SubscribeNamespaceState {
    /// Queue an event for the application, refusing to grow without bound.
    ///
    /// Suppressing duplicate additions bounds this by the active namespace count,
    /// but a peer can still churn add/remove pairs faster than an application
    /// drains them, so the queue needs its own limit.
    fn queue_event(&mut self, event: NamespaceEvent) -> Result<(), SessionError> {
        if self.events.len() >= MAX_PENDING_NAMESPACE_EVENTS {
            return Err(SessionError::ProtocolViolation(format!(
                "SUBSCRIBE_NAMESPACE response exceeded {} pending namespace events",
                MAX_PENDING_NAMESPACE_EVENTS
            )));
        }
        self.events.push_back(event);
        Ok(())
    }
}

/// Outbound SUBSCRIBE_NAMESPACE request.
///
/// This handle only exposes `NAMESPACE` / `NAMESPACE_DONE` updates from the
/// request's dedicated bidirectional stream. If the request used
/// `SubscribeOptions::Publish` or `SubscribeOptions::Both`, matching `PUBLISH`
/// messages arrive on their own request streams and are surfaced via
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

            // Draft-18: responses on a request's own bidi stream omit the
            // Request ID, so the stream's known request ID is injected here
            // rather than read off the wire.
            let msg = Session::decode_bidi_response(reader, self.request_id).await?;
            self.emit_mlog(mlog, &msg);
            if self.recv_message(msg)? == ResponseFlow::Ended {
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

    fn recv_message(&mut self, msg: Message) -> Result<ResponseFlow, SessionError> {
        match msg {
            Message::RequestOk(_) if !self.responded => {
                self.responded = true;
                self.recv_ok();
                Ok(ResponseFlow::Continue)
            }
            Message::RequestError(msg) if !self.responded => {
                self.responded = true;
                self.recv_error(ServeError::Closed(msg.error_code));
                Ok(ResponseFlow::Ended)
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

    fn recv_namespace(&mut self, msg: message::Namespace) -> Result<ResponseFlow, SessionError> {
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

        if !self.known_suffixes.insert(msg.track_namespace_suffix) {
            // Already active, so nothing changed and there is nothing to report.
            // Queuing an event per duplicate is how the active-set cap gets
            // bypassed: the set does not grow, so the cap above never trips while
            // `events` grows without bound. Repeating a NAMESPACE is not a
            // violation, so the subscription carries on.
            return Ok(ResponseFlow::Continue);
        }

        let Some(mut state) = self.state.lock_mut() else {
            return Ok(ResponseFlow::Ended);
        };
        state.queue_event(NamespaceEvent::Added(namespace))?;
        Ok(ResponseFlow::Continue)
    }

    fn recv_namespace_done(
        &mut self,
        msg: message::NamespaceDone,
    ) -> Result<ResponseFlow, SessionError> {
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
            return Ok(ResponseFlow::Ended);
        };
        state.queue_event(NamespaceEvent::Removed(namespace))?;
        Ok(ResponseFlow::Continue)
    }
}

impl Drop for SubscribeNamespaceRecv {
    fn drop(&mut self) {
        self.subscriber.remove_subscribe_namespace(self.request_id);
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        message::RequestOk,
        session::{test_support::loopback_session, RequestId},
        watch::Queue,
    };

    use super::*;

    async fn subscriber() -> Subscriber {
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        Subscriber::new(
            Queue::default(),
            loopback_session().await,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
        )
    }

    async fn recv(prefix: &str) -> SubscribeNamespaceRecv {
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
            subscriber: subscriber().await,
            force_reset: None,
        }
    }

    async fn pair(prefix: &str) -> (SubscribeNamespace, SubscribeNamespaceRecv) {
        let info = SubscribeNamespaceInfo {
            request_id: 0,
            namespace_prefix: TrackNamespacePrefix::from_utf8_path(prefix),
            subscribe_options: SubscribeOptions::Namespace,
        };
        let subscriber = subscriber().await;
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

    fn request_ok() -> Message {
        Message::RequestOk(RequestOk {
            id: 0,
            params: Default::default(),
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_event_is_drained_after_response_fin() {
        let (subscribe, mut recv) = pair("example.com").await;
        recv.recv_message(request_ok()).unwrap();
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_finish_keeps_response_half_open() {
        let (mut subscribe, mut recv) = pair("example.com").await;

        subscribe.finish_request().unwrap();
        subscribe.finish_request().unwrap();

        assert!(subscribe.request_finished);
        assert_eq!(recv.force_reset.as_mut().unwrap().try_recv().unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forced_reset_signals_response_half() {
        let (mut subscribe, mut recv) = pair("example.com").await;
        let force_reset = recv.force_reset.take().unwrap();

        subscribe.reset_request(42);

        assert_eq!(force_reset.await.unwrap(), 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_ok_marks_subscription_ok() {
        let mut recv = recv("example.com").await;

        assert_eq!(
            recv.recv_message(request_ok()).unwrap(),
            ResponseFlow::Continue
        );

        assert!(recv.state.lock().ok);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn namespace_event_reconstructs_full_namespace() {
        let mut recv = recv("example.com/meeting=123").await;
        recv.recv_message(request_ok()).unwrap();

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn namespace_done_before_namespace_is_protocol_violation() {
        let mut recv = recv("example.com").await;
        recv.recv_message(request_ok()).unwrap();

        let err = recv
            .recv_message(Message::NamespaceDone(message::NamespaceDone {
                track_namespace_suffix: TrackNamespacePrefix::from_utf8_path("meeting=123"),
            }))
            .unwrap_err();

        assert!(matches!(err, SessionError::ProtocolViolation(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_request_response_is_protocol_violation() {
        let mut recv = recv("example.com").await;
        recv.recv_message(request_ok()).unwrap();

        let err = recv.recv_message(request_ok()).unwrap_err();

        assert!(matches!(err, SessionError::ProtocolViolation(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn namespace_additions_are_capped() {
        let mut recv = recv("example.com").await;
        recv.max_known_suffixes = 2;
        recv.recv_message(request_ok()).unwrap();

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

    /// A duplicate NAMESPACE does not change the active set, so it must not queue
    /// another event. Otherwise a peer repeating one NAMESPACE forever grows the
    /// event queue without ever tripping the active-set cap.
    ///
    /// It must also leave the subscription running. Suppressing the event by
    /// reporting "nothing happened" as the same value that means "the response is
    /// over" ended the stream instead, so the relay saw discovery finish and
    /// dropped its upstream registration, on a message the peer was entitled to
    /// send.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_namespace_additions_queue_one_event_and_keep_the_stream() {
        let mut recv = recv("example.com").await;
        recv.recv_message(request_ok()).unwrap();

        let namespace = || {
            Message::Namespace(message::Namespace {
                track_namespace_suffix: TrackNamespacePrefix::from_utf8_path("meeting=0"),
            })
        };

        for _ in 0..50 {
            assert_eq!(
                recv.recv_message(namespace()).unwrap(),
                ResponseFlow::Continue
            );
        }

        assert_eq!(recv.state.lock().events.len(), 1);

        // Still live afterwards: a subsequent distinct namespace is reported.
        assert_eq!(
            recv.recv_message(Message::Namespace(message::Namespace {
                track_namespace_suffix: TrackNamespacePrefix::from_utf8_path("meeting=1"),
            }))
            .unwrap(),
            ResponseFlow::Continue
        );
        assert_eq!(recv.state.lock().events.len(), 2);
    }
}
