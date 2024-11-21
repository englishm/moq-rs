use std::collections::hash_map;
use std::collections::HashMap;

use std::sync::{Arc, Mutex};

use moq_transport::{
	coding::Tuple,
	serve::{ServeError, TracksReader},
};

#[derive(Clone)]
pub struct Locals {
	lookup: Arc<Mutex<HashMap<Tuple, TracksReader>>>,
}

impl Default for Locals {
	fn default() -> Self {
		Self::new()
	}
}

impl Locals {
	pub fn new() -> Self {
		Self {
			lookup: Default::default(),
		}
	}

	pub async fn register(&mut self, tracks: TracksReader) -> anyhow::Result<Registration> {
		let namespace = tracks.namespace.clone();
		match self.lookup.lock().unwrap().entry(namespace.clone()) {
			hash_map::Entry::Vacant(entry) => entry.insert(tracks),
			hash_map::Entry::Occupied(_) => return Err(ServeError::Duplicate.into()),
		};

		let registration = Registration {
			locals: self.clone(),
			namespace,
		};

		Ok(registration)
	}

	pub fn route(&self, namespace: &Tuple) -> Option<TracksReader> {
		self.lookup.lock().unwrap().get(namespace).cloned()
	}
}

pub struct Registration {
	locals: Locals,
	namespace: Tuple,
}

impl Drop for Registration {
	fn drop(&mut self) {
		self.locals.lookup.lock().unwrap().remove(&self.namespace);
	}
}
