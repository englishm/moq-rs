mod announce;
mod announced;
mod error;
mod publisher;
mod reader;
mod subscribe;
mod subscribed;
mod subscriber;
mod track_status_requested;
mod writer;

pub use announce::*;
pub use announced::*;
pub use error::*;
pub use publisher::*;
pub use subscribe::*;
pub use subscribed::*;
pub use subscriber::*;
pub use track_status_requested::*;

use reader::*;
use writer::*;

use futures::{stream::FuturesUnordered, StreamExt};

use crate::message::Message;
use crate::watch::Queue;
use crate::{message, setup};

#[must_use = "run() must be called"]
pub struct Session {
    webtransport: web_transport::Session,

    sender: Writer,
    recver: Reader,

    publisher: Option<Publisher>,
    subscriber: Option<Subscriber>,

    outgoing: Queue<Message>,
}

impl Session {
    fn new(
        webtransport: web_transport::Session,
        sender: Writer,
        recver: Reader,
    ) -> (Self, Option<Publisher>, Option<Subscriber>) {
        let outgoing = Queue::default().split();
        let publisher = true.then(|| Publisher::new(outgoing.0.clone(), webtransport.clone()));
        let subscriber = true.then(|| Subscriber::new(outgoing.0));

        let session = Self {
            webtransport,
            sender,
            recver,
            publisher: publisher.clone(),
            subscriber: subscriber.clone(),
            outgoing: outgoing.1,
        };

        (session, publisher, subscriber)
    }

    pub async fn connect(
        session: web_transport::Session,
    ) -> Result<(Session, Publisher, Subscriber), SessionError> {
        Self::connect_role(session)
            .await
            .map(|(session, publisher, subscriber)| {
                (session, publisher.unwrap(), subscriber.unwrap())
            })
    }

    pub async fn connect_role(
        mut session: web_transport::Session,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        let control = session.open_bi().await?;
        let mut sender = Writer::new(control.0);
        let mut recver = Reader::new(control.1);

        let versions: setup::Versions = [setup::Version::DRAFT_12].into();

        let client = setup::Client {
            versions: versions.clone(),
            params: Default::default(),
        };

        log::debug!("sending client SETUP: {:?}", client);
        sender.encode(&client).await?;

        let server: setup::Server = recver.decode().await?;
        log::debug!("received server SETUP: {:?}", server);

        Ok(Session::new(session, sender, recver))
    }

    pub async fn accept(
        session: web_transport::Session,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        Self::accept_role(session).await
    }

    pub async fn accept_role(
        mut session: web_transport::Session,
    ) -> Result<(Session, Option<Publisher>, Option<Subscriber>), SessionError> {
        let control = session.accept_bi().await?;
        let mut sender = Writer::new(control.0);
        let mut recver = Reader::new(control.1);

        let client: setup::Client = recver.decode().await?;
        log::debug!("received client SETUP: {:?}", client);

        if !client.versions.contains(&setup::Version::DRAFT_12) {
            return Err(SessionError::Version(
                client.versions,
                [setup::Version::DRAFT_12].into(),
            ));
        }

        let server = setup::Server {
            version: setup::Version::DRAFT_12,
            params: Default::default(),
        };

        log::debug!("sending server SETUP: {:?}", server);
        sender.encode(&server).await?;

        Ok(Session::new(session, sender, recver))
    }

    pub async fn run(self) -> Result<(), SessionError> {
        tokio::select! {
            res = Self::run_recv(self.recver, self.publisher, self.subscriber.clone()) => res,
            res = Self::run_send(self.sender, self.outgoing) => res,
            res = Self::run_streams(self.webtransport.clone(), self.subscriber.clone()) => res,
            res = Self::run_datagrams(self.webtransport, self.subscriber) => res,
        }
    }

    async fn run_send(
        mut sender: Writer,
        mut outgoing: Queue<message::Message>,
    ) -> Result<(), SessionError> {
        while let Some(msg) = outgoing.pop().await {
            log::debug!("sending message: {:?}", msg);
            sender.encode(&msg).await?;
        }

        Ok(())
    }

    async fn run_recv(
        mut recver: Reader,
        mut publisher: Option<Publisher>,
        mut subscriber: Option<Subscriber>,
    ) -> Result<(), SessionError> {
        loop {
            let msg: message::Message = recver.decode().await?;
            log::debug!("received message: {:?}", msg);

            let msg = match TryInto::<message::Publisher>::try_into(msg) {
                Ok(msg) => {
                    subscriber
                        .as_mut()
                        .ok_or(SessionError::Internal)?
                        .recv_message(msg)?;
                    continue;
                }
                Err(msg) => msg,
            };

            let msg = match TryInto::<message::Subscriber>::try_into(msg) {
                Ok(msg) => {
                    publisher
                        .as_mut()
                        .ok_or(SessionError::Internal)?
                        .recv_message(msg)?;
                    continue;
                }
                Err(msg) => msg,
            };

            // TODO GOAWAY
            unimplemented!("unknown message context: {:?}", msg)
        }
    }

    async fn run_streams(
        mut webtransport: web_transport::Session,
        subscriber: Option<Subscriber>,
    ) -> Result<(), SessionError> {
        let mut tasks = FuturesUnordered::new();

        loop {
            tokio::select! {
                res = webtransport.accept_uni() => {
                    let stream = res?;
                    let subscriber = subscriber.clone().ok_or(SessionError::Internal)?;

                    tasks.push(async move {
                        if let Err(err) = Subscriber::recv_stream(subscriber, stream).await {
                            log::warn!("failed to serve stream: {}", err);
                        };
                    });
                },
                _ = tasks.next(), if !tasks.is_empty() => {},
            };
        }
    }

    async fn run_datagrams(
        mut webtransport: web_transport::Session,
        mut subscriber: Option<Subscriber>,
    ) -> Result<(), SessionError> {
        loop {
            let datagram = webtransport.recv_datagram().await?;
            subscriber
                .as_mut()
                .ok_or(SessionError::Internal)?
                .recv_datagram(datagram)?;
        }
    }
}
