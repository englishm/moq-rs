use anyhow::Context;
use bytes::BytesMut;
use std::collections::{HashSet, VecDeque};

use moq_transport::serve::{
	ServeError, SubgroupReader, SubgroupWriter, SubgroupsReader, SubgroupsWriter, TrackReader, TrackReaderMode,
	TrackWriter,
};

pub struct ListingWriter {
	track: Option<TrackWriter>,
	subgroups: Option<SubgroupsWriter>,
	subgroup: Option<SubgroupWriter>,

	current: HashSet<String>,
}

impl ListingWriter {
	pub fn new(track: TrackWriter) -> Self {
		Self {
			track: Some(track),
			subgroups: None,
			subgroup: None,
			current: HashSet::new(),
		}
	}

	pub fn insert(&mut self, name: String) -> Result<(), ServeError> {
		if !self.current.insert(name.clone()) {
			return Err(ServeError::Duplicate);
		}

		match self.subgroup {
			// Create a delta if the current subgroup is small enough.
			Some(ref mut subgroup) if self.current.len() < 2 * subgroup.len() => {
				let msg = format!("+{}", name);
				subgroup.write(msg.into())?;
			}
			// Otherwise create a snapshot with every element.
			_ => self.subgroup = Some(self.snapshot()?),
		}

		Ok(())
	}

	pub fn remove(&mut self, name: &str) -> Result<(), ServeError> {
		if !self.current.remove(name) {
			return Err(ServeError::NotFound);
		}

		match self.subgroup {
			// Create a delta if the current subgroup is small enough.
			Some(ref mut subgroup) if self.current.len() < 2 * subgroup.len() => {
				let msg = format!("-{}", name);
				subgroup.write(msg.into())?;
			}
			// Otherwise create a snapshot with every element.
			_ => self.subgroup = Some(self.snapshot()?),
		}

		Ok(())
	}

	fn snapshot(&mut self) -> Result<SubgroupWriter, ServeError> {
		let mut subgroups = match self.subgroups.take() {
			Some(subgroups) => subgroups,
			None => self.track.take().unwrap().subgroups()?,
		};

		let priority = 127;
		let mut subgroup = subgroups.append(priority)?;

		let mut msg = BytesMut::new();
		for name in &self.current {
			msg.extend_from_slice(name.as_bytes());
			msg.extend_from_slice(b"\n");
		}

		subgroup.write(msg.freeze())?;
		self.subgroups = Some(subgroups);

		Ok(subgroup)
	}

	pub fn len(&self) -> usize {
		self.current.len()
	}

	pub fn is_empty(&self) -> bool {
		self.current.is_empty()
	}
}

#[derive(Clone)]
pub enum ListingDelta {
	Add(String),
	Rem(String),
}

#[derive(Clone)]
pub struct ListingReader {
	track: TrackReader,

	// Keep track of the current subgroup.
	subgroups: Option<SubgroupsReader>,
	subgroup: Option<SubgroupReader>,

	// The current state of the listing.
	current: HashSet<String>,

	// A list of deltas we need to return
	deltas: VecDeque<ListingDelta>,
}

impl ListingReader {
	pub fn new(track: TrackReader) -> Self {
		Self {
			track,
			subgroups: None,
			subgroup: None,

			current: HashSet::new(),
			deltas: VecDeque::new(),
		}
	}

	pub async fn next(&mut self) -> anyhow::Result<Option<ListingDelta>> {
		if let Some(delta) = self.deltas.pop_front() {
			return Ok(Some(delta));
		}

		if self.subgroups.is_none() {
			self.subgroups = match self.track.mode().await? {
				TrackReaderMode::Subgroups(subgroups) => Some(subgroups),
				_ => anyhow::bail!("expected subgroups mode"),
			};
		};

		if self.subgroup.is_none() {
			self.subgroup = Some(self.subgroups.as_mut().unwrap().next().await?.context("empty track")?);
		}

		let mut subgroup_done = false;
		let mut subgroups_done = false;

		loop {
			tokio::select! {
				next = self.subgroups.as_mut().unwrap().next(), if !subgroups_done => {
					if let Some(next) = next? {
						self.subgroup = Some(next);
						subgroup_done = false;
					} else {
						subgroups_done = true;
					}
				},
				object = self.subgroup.as_mut().unwrap().read_next(), if !subgroup_done => {
					let payload = match object? {
						Some(object) => object,
						None => {
							subgroup_done = true;
							continue;
						}
					};

					if payload.is_empty() {
						anyhow::bail!("empty payload");
					} else if self.subgroup.as_mut().unwrap().pos() == 1 {
						// This is a full snapshot, not a delta
						let set = HashSet::from_iter(payload.split(|&b| b == b'\n').map(|s| String::from_utf8_lossy(s).to_string()));

						for name in set.difference(&self.current) {
							self.deltas.push_back(ListingDelta::Add(name.clone()));
						}

						for name in self.current.difference(&set) {
							self.deltas.push_back(ListingDelta::Rem(name.clone()));
						}

						self.current = set;

						if let Some(delta) = self.deltas.pop_front() {
							return Ok(Some(delta));
						}
					} else if payload[0] == b'+' {
						return Ok(Some(ListingDelta::Add(String::from_utf8_lossy(&payload[1..]).to_string())));
					} else if payload[0] == b'-' {
						return Ok(Some(ListingDelta::Rem(String::from_utf8_lossy(&payload[1..]).to_string())));
					} else {
						anyhow::bail!("invalid delta: {:?}", payload);
					}
				}
				else => return Ok(None),
			}
		}
	}

	// If you just want to proxy the track
	pub fn into_inner(self) -> TrackReader {
		self.track
	}
}
