// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Full Track Name reservations, indexed both ways.
//!
//! Draft-18 §5.1 allows only one subscription per track per role, so both the
//! publisher and subscriber sides reserve a [`FullTrackName`] while a request
//! is live. Lookups happen by name (to reject duplicates) and removals happen
//! by request ID (when the request ends), so the reverse index is not optional:
//! without it every teardown is a full scan of the map.

use std::collections::HashMap;

use crate::serve::FullTrackName;

#[derive(Default)]
pub(crate) struct NameRegistry {
    by_name: HashMap<FullTrackName, u64>,
    by_request_id: HashMap<u64, FullTrackName>,
}

impl NameRegistry {
    pub fn contains_name(&self, name: &FullTrackName) -> bool {
        self.by_name.contains_key(name)
    }

    /// Reserve `name` for `request_id`, evicting any stale entry either key
    /// still points at so the two indexes cannot drift apart.
    pub fn insert(&mut self, name: FullTrackName, request_id: u64) {
        if let Some(old_name) = self.by_request_id.insert(request_id, name.clone()) {
            self.by_name.remove(&old_name);
        }
        if let Some(old_id) = self.by_name.insert(name, request_id) {
            self.by_request_id.remove(&old_id);
        }
    }

    pub fn remove_by_request_id(&mut self, request_id: u64) -> Option<FullTrackName> {
        let name = self.by_request_id.remove(&request_id)?;
        self.by_name.remove(&name);
        Some(name)
    }

    #[cfg(test)]
    pub fn get_by_name(&self, name: &FullTrackName) -> Option<u64> {
        self.by_name.get(name).copied()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty() && self.by_request_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding::{TrackName, TrackNamespace};

    fn name(track: &str) -> FullTrackName {
        FullTrackName {
            namespace: TrackNamespace::from_utf8_path("ns"),
            name: TrackName::from(track),
        }
    }

    #[test]
    fn removal_by_request_id_leaves_other_reservations() {
        let mut reg = NameRegistry::default();
        reg.insert(name("video"), 6);
        reg.insert(name("audio"), 8);

        assert_eq!(reg.remove_by_request_id(6), Some(name("video")));
        assert!(!reg.contains_name(&name("video")));
        assert_eq!(reg.get_by_name(&name("audio")), Some(8));
    }

    #[test]
    fn removing_an_unknown_request_id_is_a_noop() {
        let mut reg = NameRegistry::default();
        reg.insert(name("video"), 6);

        assert_eq!(reg.remove_by_request_id(7), None);
        assert_eq!(reg.get_by_name(&name("video")), Some(6));
    }

    /// Both indexes must stay consistent when a name or ID is reused, or a
    /// later removal would strand half a reservation.
    #[test]
    fn reinserting_the_same_name_replaces_the_old_request_id() {
        let mut reg = NameRegistry::default();
        reg.insert(name("video"), 6);
        reg.insert(name("video"), 8);

        assert_eq!(reg.get_by_name(&name("video")), Some(8));
        assert_eq!(reg.remove_by_request_id(6), None, "the old ID is gone");
        assert_eq!(reg.remove_by_request_id(8), Some(name("video")));
        assert!(reg.is_empty());
    }

    #[test]
    fn reusing_a_request_id_releases_its_previous_name() {
        let mut reg = NameRegistry::default();
        reg.insert(name("video"), 6);
        reg.insert(name("audio"), 6);

        assert!(!reg.contains_name(&name("video")));
        assert_eq!(reg.remove_by_request_id(6), Some(name("audio")));
        assert!(reg.is_empty());
    }
}
