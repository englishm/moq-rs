// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    ops,
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;

use crate::coding::{ReasonPhrase, TrackNamespace};
use crate::message::{Message, RequestErrorCode};
use crate::mlog;
use crate::watch::State;
use crate::{message, serve::ServeError};

use super::{
    PublishNamespaceInfo, Reader, Session, SessionError, Subscriber, Writer, CANCELLED_STREAM_CODE,
};

// Immediate cancellation can queue after REQUEST_OK before the stream task runs.
const RESPONSE_QUEUE_CAPACITY: usize = 2;

enum StreamAction {
    Response(Message),
    Cancel(u32),
}

enum StreamEvent {
    Action(Option<StreamAction>),
    RequestFinished(Result<bool, SessionError>),
    ResponseStopped(Result<Option<u8>, SessionError>),
}

#[derive(Default)]
struct PublishedNamespaceState {
    done: bool,
}

/// Represents an inbound PUBLISH_NAMESPACE received by a subscriber.
///
/// Dropping an accepted namespace cancels its request stream. Dropping an
/// unaccepted namespace rejects it with REQUEST_ERROR.
pub struct PublishedNamespace {
    state: State<PublishedNamespaceState>,
    outgoing: mpsc::Sender<StreamAction>,

    pub info: PublishNamespaceInfo,

    ok: bool,
    error: Option<ServeError>,
}

impl PublishedNamespace {
    pub(super) fn new(
        subscriber: Subscriber,
        request_id: u64,
        namespace: TrackNamespace,
    ) -> (PublishedNamespace, PublishedNamespaceRecv) {
        let info = PublishNamespaceInfo {
            request_id,
            namespace,
        };
        let (send_state, recv_state) = State::default().split();
        let (send_queue, recv_queue) = mpsc::channel(RESPONSE_QUEUE_CAPACITY);
        let send = Self {
            state: send_state,
            outgoing: send_queue,
            info,
            ok: false,
            error: None,
        };
        let recv = PublishedNamespaceRecv {
            state: recv_state,
            outgoing: recv_queue,
            subscriber,
            request_id,
        };

        (send, recv)
    }

    /// Accept the PUBLISH_NAMESPACE by sending REQUEST_OK.
    pub fn ok(&mut self) -> Result<(), ServeError> {
        if self.ok {
            return Err(ServeError::Duplicate);
        }

        self.outgoing
            .try_send(StreamAction::Response(
                message::RequestOk {
                    id: self.info.request_id,
                    params: Default::default(),
                }
                .into(),
            ))
            .map_err(|_| ServeError::Cancel)?;
        self.ok = true;

        Ok(())
    }

    /// Wait until the peer closes the namespace request stream.
    pub async fn closed(&self) -> Result<(), ServeError> {
        loop {
            let Some(modified) = self.state.lock().modified() else {
                return Ok(());
            };

            modified.await;
        }
    }

    /// Reject the PUBLISH_NAMESPACE; the error is sent on drop.
    pub fn close(mut self, err: ServeError) -> Result<(), ServeError> {
        self.error = Some(err);
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
        if self.state.lock().done {
            return;
        }

        if self.ok {
            let _ = self
                .outgoing
                .try_send(StreamAction::Cancel(CANCELLED_STREAM_CODE));
            return;
        }

        let err = self.error.clone().unwrap_or(ServeError::Done);
        let _ = self.outgoing.try_send(StreamAction::Response(
            message::RequestError {
                id: self.info.request_id,
                error_code: RequestErrorCode::Uninterested as u64,
                retry_interval: 0,
                reason: ReasonPhrase(err.to_string()),
            }
            .into(),
        ));
    }
}

pub(super) struct PublishedNamespaceRecv {
    state: State<PublishedNamespaceState>,
    outgoing: mpsc::Receiver<StreamAction>,
    subscriber: Subscriber,
    request_id: u64,
}

impl PublishedNamespaceRecv {
    pub(super) async fn run(
        mut self,
        mut writer: Writer,
        mut reader: Reader,
        mlog: Option<Arc<Mutex<mlog::MlogWriter>>>,
    ) -> Result<(), SessionError> {
        let mut reading = true;

        loop {
            let event = tokio::select! {
                biased;
                done = reader.done(), if reading => StreamEvent::RequestFinished(done),
                action = self.outgoing.recv() => StreamEvent::Action(action),
                stopped = writer.closed() => StreamEvent::ResponseStopped(stopped),
            };

            match event {
                StreamEvent::Action(action) => {
                    let Some(action) = action else {
                        if reading {
                            let _ = writer.finish();
                        } else {
                            writer.reset(CANCELLED_STREAM_CODE);
                        }
                        reader.stop(CANCELLED_STREAM_CODE);
                        return Ok(());
                    };
                    let response = match action {
                        StreamAction::Response(response) => response,
                        StreamAction::Cancel(code) => {
                            if reading {
                                let _ = writer.finish();
                            } else {
                                writer.reset(code);
                            }
                            reader.stop(code);
                            return Ok(());
                        }
                    };
                    self.emit_mlog(&mlog, &response);
                    let terminal = matches!(response, Message::RequestError(_));
                    match Session::encode_bidi_response(&mut writer, &response).await {
                        Ok(()) => {}
                        Err(err) if err.is_stream_error() => return Ok(()),
                        Err(err) => return Err(err),
                    }
                    if terminal {
                        reader.stop(CANCELLED_STREAM_CODE);
                        return match writer.finish() {
                            Ok(()) => Ok(()),
                            Err(err) if err.is_stream_error() => Ok(()),
                            Err(err) => Err(err),
                        };
                    }
                }
                StreamEvent::RequestFinished(done) => match done {
                    Ok(true) => {
                        reading = false;
                    }
                    Ok(false) => {
                        return Err(SessionError::ProtocolViolation(
                            "unexpected data after PUBLISH_NAMESPACE request".to_string(),
                        ));
                    }
                    Err(err) if err.is_stream_error() => {
                        writer.reset(CANCELLED_STREAM_CODE);
                        return Ok(());
                    }
                    Err(err) => return Err(err),
                },
                StreamEvent::ResponseStopped(stopped) => {
                    let _ = stopped?;
                    reader.stop(CANCELLED_STREAM_CODE);
                    return Ok(());
                }
            }
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
                        "publish_namespace",
                        msg,
                    )),
                    Message::RequestError(msg) => Some(mlog::events::request_error_created(
                        time,
                        0,
                        "publish_namespace",
                        msg,
                    )),
                    _ => None,
                };
                if let Some(event) = event {
                    let _ = mlog.add_event(event);
                }
            }
        }
    }
}

impl Drop for PublishedNamespaceRecv {
    fn drop(&mut self) {
        if let Some(mut state) = self.state.lock_mut() {
            state.done = true;
        }
        self.subscriber.remove_published_namespace(self.request_id);
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        session::{
            test_support::{loopback_session, loopback_session_pair},
            RequestId, Transport,
        },
        watch::Queue,
    };

    use super::*;

    async fn pair() -> (PublishedNamespace, PublishedNamespaceRecv) {
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscriber = Subscriber::new(
            Queue::default(),
            loopback_session().await,
            Transport::WebTransport,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
            Default::default(),
        );
        PublishedNamespace::new(subscriber, 6, TrackNamespace::from_utf8_path("test"))
    }

    #[tokio::test]
    async fn dropping_accepted_namespace_cancels_request_stream() {
        let (mut published, mut recv) = pair().await;
        published.ok().unwrap();
        assert!(matches!(
            recv.outgoing.recv().await,
            Some(StreamAction::Response(Message::RequestOk(_)))
        ));
        drop(published);

        assert!(matches!(
            recv.outgoing.recv().await,
            Some(StreamAction::Cancel(CANCELLED_STREAM_CODE))
        ));
    }

    #[tokio::test]
    async fn dropping_receiver_closes_namespace() {
        let (published, recv) = pair().await;

        drop(recv);

        published.closed().await.unwrap();
        assert!(published.state.lock().done);
    }

    #[tokio::test]
    async fn accepted_namespace_survives_request_fin_until_local_cancel() {
        let (client, server) = loopback_session_pair().await;
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscriber = Subscriber::new(
            Queue::default(),
            server.clone(),
            Transport::WebTransport,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
            Default::default(),
        );
        let (mut published, recv) =
            PublishedNamespace::new(subscriber, 0, TrackNamespace::from_utf8_path("test"));

        let (mut request_send, response_recv) = client.open_bi().await.unwrap();
        request_send.write(b"request").await.unwrap();
        let (response_send, mut request_recv) = server.accept_bi().await.unwrap();
        assert_eq!(
            request_recv.read(7).await.unwrap().unwrap().as_ref(),
            b"request"
        );

        published.ok().unwrap();
        let run =
            tokio::spawn(recv.run(Writer::new(response_send), Reader::new(request_recv), None));
        request_send.finish().unwrap();

        let mut response_reader = Reader::new(response_recv);
        assert!(matches!(
            Session::decode_bidi_response(&mut response_reader, 0)
                .await
                .unwrap(),
            Message::RequestOk(_)
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), published.closed())
                .await
                .is_err()
        );

        drop(published);

        run.await.unwrap().unwrap();
        let reset = response_reader.done().await.unwrap_err();
        assert!(reset.is_stream_error());
    }

    #[tokio::test]
    async fn immediate_cancel_preserves_namespace_acceptance() {
        let (client, server) = loopback_session_pair().await;
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscriber = Subscriber::new(
            Queue::default(),
            server.clone(),
            Transport::WebTransport,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
            Default::default(),
        );
        let (mut published, recv) =
            PublishedNamespace::new(subscriber, 0, TrackNamespace::from_utf8_path("test"));

        let (mut request_send, response_recv) = client.open_bi().await.unwrap();
        request_send.write(b"request").await.unwrap();
        let (response_send, mut request_recv) = server.accept_bi().await.unwrap();
        assert_eq!(
            request_recv.read(7).await.unwrap().unwrap().as_ref(),
            b"request"
        );

        published.ok().unwrap();
        drop(published);
        let run =
            tokio::spawn(recv.run(Writer::new(response_send), Reader::new(request_recv), None));

        let mut response_reader = Reader::new(response_recv);
        assert!(matches!(
            Session::decode_bidi_response(&mut response_reader, 0)
                .await
                .unwrap(),
            Message::RequestOk(_)
        ));
        assert!(response_reader.done().await.unwrap());
        assert_eq!(
            request_send.closed().await.unwrap(),
            Some(u8::try_from(CANCELLED_STREAM_CODE).unwrap())
        );
        run.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancel_resets_response_when_request_fin_precedes_dispatch() {
        let (client, server) = loopback_session_pair().await;
        let (bidi_task_tx, _bidi_task_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscriber = Subscriber::new(
            Queue::default(),
            server.clone(),
            Transport::WebTransport,
            None,
            RequestId::new(0, 1),
            bidi_task_tx,
            Default::default(),
        );
        let (mut published, recv) =
            PublishedNamespace::new(subscriber, 0, TrackNamespace::from_utf8_path("test"));

        let (mut request_send, response_recv) = client.open_bi().await.unwrap();
        request_send.write(b"request").await.unwrap();
        request_send.finish().unwrap();
        let (response_send, mut request_recv) = server.accept_bi().await.unwrap();
        assert_eq!(
            request_recv.read(7).await.unwrap().unwrap().as_ref(),
            b"request"
        );

        published.ok().unwrap();
        drop(published);
        let run =
            tokio::spawn(recv.run(Writer::new(response_send), Reader::new(request_recv), None));

        let mut response_reader = Reader::new(response_recv);
        match Session::decode_bidi_response(&mut response_reader, 0).await {
            Ok(Message::RequestOk(_)) => {
                let reset = response_reader.done().await.unwrap_err();
                assert!(reset.is_stream_error());
            }
            Err(err) => assert!(err.is_stream_error()),
            Ok(msg) => panic!("unexpected response: {}", msg.name()),
        }
        run.await.unwrap().unwrap();
    }
}
