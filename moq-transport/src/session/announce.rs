use std::{collections::VecDeque, ops};

use crate::watch::State;
use crate::{message, serve::ServeError};

use super::{Publisher, Subscribed, TrackStatusRequested};

#[derive(Debug, Clone)]
pub struct AnnounceInfo {
	pub namespace: String,
}

#[derive(Debug)]
struct AnnounceState {
	subscribers: VecDeque<Subscribed>,
	track_statuses_requested: VecDeque<TrackStatusRequested>,
	ok: bool,
	closed: Result<(), ServeError>,
}

impl Default for AnnounceState {
	fn default() -> Self {
		Self {
			subscribers: Default::default(),
			track_statuses_requested: Default::default(),
			ok: false,
			closed: Ok(()),
		}
	}
}

impl Drop for AnnounceState {
	fn drop(&mut self) {
		for subscriber in self.subscribers.drain(..) {
			subscriber.close(ServeError::NotFound).ok();
		}
		// TODO: Flush any pending track status requests with code 0x01?
	}
}

#[must_use = "unannounce on drop"]
pub struct Announce {
	publisher: Publisher,
	state: State<AnnounceState>,

	pub info: AnnounceInfo,
}

impl Announce {
	pub(super) fn new(mut publisher: Publisher, namespace: String) -> (Announce, AnnounceRecv) {
		let info = AnnounceInfo {
			namespace: namespace.clone(),
		};

		publisher.send_message(message::Announce {
			namespace,
			params: Default::default(),
		});

		let (send, recv) = State::default().split();

		let send = Self {
			publisher,
			info,
			state: send,
		};
		let recv = AnnounceRecv { state: recv };

		(send, recv)
	}

	// Run until we get an error
	pub async fn closed(&self) -> Result<(), ServeError> {
		loop {
			{
				let state = self.state.lock();
				state.closed.clone()?;

				match state.modified() {
					Some(notified) => notified,
					None => return Ok(()),
				}
			}
			.await;
		}
	}

	pub async fn subscribed(&mut self) -> Result<Option<Subscribed>, ServeError> {
		loop {
			log::debug!("subscribed loop");
			{
				log::debug!("subscribed 1");
				let state = self.state.lock();
				log::debug!("subscribed 2");
				if !state.subscribers.is_empty() {
					log::debug!("subscribed 3");
					return Ok(state.into_mut().and_then(|mut state| state.subscribers.pop_front()));
				}

				log::debug!("subscribed 4");
				state.closed.clone()?;
				log::debug!("subscribed 5");
				match state.modified() {
					Some(notified) => {
						log::debug!("subscribed 6");
						notified
					},

					None => {
						log::debug!("subscribed 7");
						return Ok(None)
					},
				}
			}
			.await;
			log::debug!("subscribed 8");
		}
	}

	pub async fn track_status_requested(&mut self) -> Result<Option<TrackStatusRequested>, ServeError> {
		loop {
			log::debug!("track_status_requested loop");
			{
				log::debug!("track_status_requested 1");
				let state = self.state.lock();
				log::debug!("track_status_requested 2");
				if !state.track_statuses_requested.is_empty() {
					log::debug!("track_status_requested 3");
					return Ok(state.into_mut().and_then(|mut state| state.track_statuses_requested.pop_front()));
				}
				log::debug!("track_status_requested 4");

				state.closed.clone()?;
				log::debug!("track_status_requested 5");
				match state.modified() {
					Some(notified) => {
						log::debug!("track_status_requested 5.1");
						dbg!(&notified);
						notified
					},
					None => {
						log::debug!("track_status_requested 5.2");
						return Ok(None)
					},
				}
			}
			.await;
			log::debug!("track_status_requested 6");
		}
	}

	// Wait until an OK is received
	pub async fn ok(&self) -> Result<(), ServeError> {
		loop {
			log::debug!("ok loop");
			{
				log::debug!("ok 1");
				let state = self.state.lock();
				log::debug!("ok 2");
				if state.ok {
					log::debug!("ok 3");
					return Ok(());
				}
				log::debug!("ok 4");
				state.closed.clone()?;
				log::debug!("ok 5");
				match state.modified() {
					Some(notified) =>
						{
							log::debug!("ok 6");
							notified
						} ,
					None => {
						log::debug!("ok 7");
						return Ok(())
					},
				}
			}
			.await;
		}
	}
}

impl Drop for Announce {
	fn drop(&mut self) {
		if self.state.lock().closed.is_err() {
			return;
		}

		self.publisher.send_message(message::Unannounce {
			namespace: self.namespace.to_string(),
		});
	}
}

impl ops::Deref for Announce {
	type Target = AnnounceInfo;

	fn deref(&self) -> &Self::Target {
		&self.info
	}
}

pub(super) struct AnnounceRecv {
	state: State<AnnounceState>,
}

impl AnnounceRecv {
	pub fn recv_ok(&mut self) -> Result<(), ServeError> {
		if let Some(mut state) = self.state.lock_mut() {
			if state.ok {
				return Err(ServeError::Duplicate);
			}

			state.ok = true;
		}

		Ok(())
	}

	pub fn recv_error(self, err: ServeError) -> Result<(), ServeError> {
		let state = self.state.lock();
		state.closed.clone()?;

		let mut state = state.into_mut().ok_or(ServeError::Done)?;
		state.closed = Err(err);

		Ok(())
	}

	pub fn recv_subscribe(&mut self, subscriber: Subscribed) -> Result<(), ServeError> {
		let mut state = self.state.lock_mut().ok_or(ServeError::Done)?;
		state.subscribers.push_back(subscriber);

		Ok(())
	}

	pub fn recv_track_status_requested(&mut self, track_status_requested: TrackStatusRequested) -> Result<(), ServeError> {
		let mut state = self.state.lock_mut().ok_or(ServeError::Done)?;
		state.track_statuses_requested.push_back(track_status_requested);
		Ok(())
	}
}
