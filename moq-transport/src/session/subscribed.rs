use std::ops;

use futures::stream::FuturesUnordered;
use futures::StreamExt;

use crate::coding::Encode;
use crate::serve::{ServeError, TrackReaderMode};
use crate::watch::State;
use crate::{data, message, serve};

use super::{Publisher, SessionError, SubscribeInfo, Writer};

#[derive(Debug)]
struct SubscribedState {
	max_group_id: Option<(u64, u64)>,
	closed: Result<(), ServeError>,
}

impl SubscribedState {
	fn update_max_group_id(&mut self, group_id: u64, object_id: u64) -> Result<(), ServeError> {
		if let Some((max_group, max_object)) = self.max_group_id {
			if group_id >= max_group && object_id >= max_object {
				self.max_group_id = Some((group_id, object_id));
			}
		}

		Ok(())
	}
}

impl Default for SubscribedState {
	fn default() -> Self {
		Self {
			max_group_id: None,
			closed: Ok(()),
		}
	}
}

pub struct Subscribed {
	publisher: Publisher,
	state: State<SubscribedState>,
	msg: message::Subscribe,
	ok: bool,

	pub info: SubscribeInfo,
}

impl Subscribed {
	pub(super) fn new(publisher: Publisher, msg: message::Subscribe) -> (Self, SubscribedRecv) {
		let (send, recv) = State::default().split();
		let info = SubscribeInfo {
			namespace: msg.track_namespace.clone(),
			name: msg.track_name.clone(),
		};

		let send = Self {
			publisher,
			state: send,
			msg,
			info,
			ok: false,
		};

		// Prevents updates after being closed
		let recv = SubscribedRecv { state: recv };

		(send, recv)
	}

	pub async fn serve(mut self, track: serve::TrackReader) -> Result<(), SessionError> {
		let res = self.serve_inner(track).await;
		if let Err(err) = &res {
			self.close(err.clone().into())?;
		}

		res
	}

	async fn serve_inner(&mut self, track: serve::TrackReader) -> Result<(), SessionError> {
		let latest = track.latest();
		self.state.lock_mut().ok_or(ServeError::Cancel)?.max_group_id = latest;

		self.publisher.send_message(message::SubscribeOk {
			id: self.msg.id,
			expires: None,
			group_order: message::GroupOrder::Descending, // TODO: resolve correct value from publisher / subscriber prefs
			latest,
		});

		self.ok = true; // So we sent SubscribeDone on drop

		match track.mode().await? {
			// TODO cancel track/datagrams on closed
			TrackReaderMode::Stream(stream) => self.serve_track(stream).await,
			TrackReaderMode::Subgroups(subgroups) => self.serve_subgroups(subgroups).await,
			TrackReaderMode::Datagrams(datagrams) => self.serve_datagrams(datagrams).await,
		}
	}

	pub fn close(self, err: ServeError) -> Result<(), ServeError> {
		let state = self.state.lock();
		state.closed.clone()?;

		let mut state = state.into_mut().ok_or(ServeError::Done)?;
		state.closed = Err(err);

		Ok(())
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

impl ops::Deref for Subscribed {
	type Target = SubscribeInfo;

	fn deref(&self) -> &Self::Target {
		&self.info
	}
}

impl Drop for Subscribed {
	fn drop(&mut self) {
		let state = self.state.lock();
		let err = state.closed.as_ref().err().cloned().unwrap_or(ServeError::Done);
		let max_group_id = state.max_group_id;
		drop(state); // Important to avoid a deadlock

		if self.ok {
			self.publisher.send_message(message::SubscribeDone {
				id: self.msg.id,
				last: max_group_id,
				code: err.code(),
				reason: err.to_string(),
			});
		} else {
			self.publisher.send_message(message::SubscribeError {
				id: self.msg.id,
				alias: 0,
				code: err.code(),
				reason: err.to_string(),
			});
		};
	}
}

impl Subscribed {
	async fn serve_track(&mut self, mut track: serve::StreamReader) -> Result<(), SessionError> {
		let mut stream = self.publisher.open_uni().await?;

		// TODO figure out u32 vs u64 priority
		stream.set_priority(track.priority as i32);

		let mut writer = Writer::new(stream);

		let header: data::Header = data::TrackHeader {
			subscribe_id: self.msg.id,
			track_alias: self.msg.track_alias,
			publisher_priority: track.priority,
		}
		.into();

		writer.encode(&header).await?;

		log::trace!("sent track header: {:?}", header);

		while let Some(mut group) = track.next().await? {
			while let Some(mut object) = group.next().await? {
				let header = data::TrackObject {
					group_id: object.group_id,
					object_id: object.object_id,
					size: object.size,
					status: object.status,
				};

				self.state
					.lock_mut()
					.ok_or(ServeError::Done)?
					.update_max_group_id(object.group_id, object.object_id)?;

				writer.encode(&header).await?;

				log::trace!("sent track object: {:?}", header);

				while let Some(chunk) = object.read().await? {
					writer.write(&chunk).await?;
					log::trace!("sent track payload: {:?}", chunk.len());
				}

				log::trace!("sent track done");
			}
		}

		Ok(())
	}

	async fn serve_subgroups(&mut self, mut subgroups: serve::SubgroupsReader) -> Result<(), SessionError> {
		let mut tasks = FuturesUnordered::new();
		let mut done: Option<Result<(), ServeError>> = None;

		loop {
			tokio::select! {
				res = subgroups.next(), if done.is_none() => match res {
					Ok(Some(subgroup)) => {
						let header = data::SubgroupHeader {
							subscribe_id: self.msg.id,
							track_alias: self.msg.track_alias,
							group_id: subgroup.group_id,
							subgroup_id: subgroup.subgroup_id,
							publisher_priority: subgroup.priority,
						};

						let publisher = self.publisher.clone();
						let state = self.state.clone();
						let info = subgroup.info.clone();

						tasks.push(async move {
							if let Err(err) = Self::serve_subgroup(header, subgroup, publisher, state).await {
								log::warn!("failed to serve group: {:?}, error: {}", info, err);
							}
						});
					},
					Ok(None) => done = Some(Ok(())),
					Err(err) => done = Some(Err(err)),
				},
				res = self.closed(), if done.is_none() => done = Some(res),
				_ = tasks.next(), if !tasks.is_empty() => {},
				else => return Ok(done.unwrap()?),
			}
		}
	}

	async fn serve_subgroup(
		header: data::SubgroupHeader,
		mut subgroup: serve::SubgroupReader,
		mut publisher: Publisher,
		state: State<SubscribedState>,
	) -> Result<(), SessionError> {
		let mut stream = publisher.open_uni().await?;

		// TODO figure out u32 vs u64 priority
		stream.set_priority(subgroup.priority as i32);

		let mut writer = Writer::new(stream);

		let header: data::Header = header.into();
		writer.encode(&header).await?;

		log::trace!("sent group: {:?}", header);

		while let Some(mut object) = subgroup.next().await? {
			let header = data::SubgroupObject {
				object_id: object.object_id,
				size: object.size,
				status: object.status,
			};

			writer.encode(&header).await?;

			state
				.lock_mut()
				.ok_or(ServeError::Done)?
				.update_max_group_id(subgroup.group_id, object.object_id)?;

			log::trace!("sent group object: {:?}", header);

			while let Some(chunk) = object.read().await? {
				writer.write(&chunk).await?;
				log::trace!("sent group payload: {:?}", chunk.len());
			}

			log::trace!("sent group done");
		}

		Ok(())
	}

	async fn serve_datagrams(&mut self, mut datagrams: serve::DatagramsReader) -> Result<(), SessionError> {
		while let Some(datagram) = datagrams.read().await? {
			let datagram = data::Datagram {
				subscribe_id: self.msg.id,
				track_alias: self.msg.track_alias,
				group_id: datagram.group_id,
				object_id: datagram.object_id,
				publisher_priority: datagram.priority,
				object_status: datagram.status,
				payload_len: datagram.payload.len() as u64,
				payload: datagram.payload,
			};

			let mut buffer = bytes::BytesMut::with_capacity(datagram.payload.len() + 100);
			datagram.encode(&mut buffer)?;

			self.publisher.send_datagram(buffer.into()).await?;
			log::trace!("sent datagram: {:?}", datagram);

			self.state
				.lock_mut()
				.ok_or(ServeError::Done)?
				.update_max_group_id(datagram.group_id, datagram.object_id)?;
		}

		Ok(())
	}
}

pub(super) struct SubscribedRecv {
	state: State<SubscribedState>,
}

impl SubscribedRecv {
	pub fn recv_unsubscribe(&mut self) -> Result<(), ServeError> {
		let state = self.state.lock();
		state.closed.clone()?;

		if let Some(mut state) = state.into_mut() {
			state.closed = Err(ServeError::Cancel);
		}

		Ok(())
	}
}
