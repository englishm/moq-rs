// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;

use moq_transport::coding::TrackNamespacePrefix;

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct RootDelta {
    pub(crate) added: HashSet<TrackNamespacePrefix>,
    pub(crate) removed: HashSet<TrackNamespacePrefix>,
}

#[derive(Default)]
pub(crate) struct CoveringPrefixSet {
    // The manager's shared lease layer turns duplicate owners into one active prefix.
    active: HashSet<TrackNamespacePrefix>,
    roots: HashSet<TrackNamespacePrefix>,
}

impl CoveringPrefixSet {
    pub(crate) fn add(&mut self, prefix: TrackNamespacePrefix) -> RootDelta {
        if !self.active.insert(prefix.clone()) {
            return RootDelta::default();
        }

        if self.roots.iter().any(|root| covers(root, &prefix)) {
            return RootDelta::default();
        }

        let removed = self
            .roots
            .iter()
            .filter(|root| covers(&prefix, root))
            .cloned()
            .collect::<HashSet<_>>();
        self.roots.retain(|root| !removed.contains(root));
        self.roots.insert(prefix.clone());

        RootDelta {
            added: HashSet::from([prefix]),
            removed,
        }
    }

    pub(crate) fn remove(&mut self, prefix: &TrackNamespacePrefix) -> RootDelta {
        if !self.active.remove(prefix) {
            return RootDelta::default();
        }

        if !self.roots.remove(prefix) {
            return RootDelta::default();
        }

        let added = self
            .active
            .iter()
            .filter(|candidate| covers(prefix, candidate))
            .filter(|candidate| {
                !self
                    .active
                    .iter()
                    .any(|other| strictly_covers(other, candidate))
            })
            .cloned()
            .collect::<HashSet<_>>();
        self.roots.extend(added.iter().cloned());

        RootDelta {
            added,
            removed: HashSet::from([prefix.clone()]),
        }
    }
}

fn covers(broader: &TrackNamespacePrefix, narrower: &TrackNamespacePrefix) -> bool {
    broader.fields.len() <= narrower.fields.len() && broader.overlaps(narrower)
}

fn strictly_covers(broader: &TrackNamespacePrefix, narrower: &TrackNamespacePrefix) -> bool {
    broader.fields.len() < narrower.fields.len() && broader.overlaps(narrower)
}

#[cfg(test)]
mod tests {
    use super::*;
    use moq_transport::coding::TupleField;

    fn prefix(path: &str) -> TrackNamespacePrefix {
        TrackNamespacePrefix::from_utf8_path(path)
    }

    fn prefixes(paths: &[&str]) -> HashSet<TrackNamespacePrefix> {
        paths.iter().map(|path| prefix(path)).collect()
    }

    fn assert_delta(delta: RootDelta, added: &[&str], removed: &[&str]) {
        assert_eq!(delta.added, prefixes(added));
        assert_eq!(delta.removed, prefixes(removed));
    }

    #[test]
    fn coalesces_idempotent_and_broader_prefixes() {
        let mut set = CoveringPrefixSet::default();
        let foo_bar = prefix("foo/bar");
        let foo = prefix("foo");

        assert_delta(set.add(foo_bar.clone()), &["foo/bar"], &[]);
        assert_delta(set.add(foo_bar.clone()), &[], &[]);
        assert_delta(set.add(foo.clone()), &["foo"], &["foo/bar"]);
        assert_delta(set.remove(&foo), &["foo/bar"], &["foo"]);
        assert_delta(set.remove(&foo_bar), &[], &["foo/bar"]);
    }

    #[test]
    fn keeps_unrelated_siblings_as_roots() {
        let mut set = CoveringPrefixSet::default();
        let foo_a = prefix("foo/a");
        let foo_b = prefix("foo/b");

        assert_delta(set.add(foo_a.clone()), &["foo/a"], &[]);
        assert_delta(set.add(foo_b.clone()), &["foo/b"], &[]);
        assert_delta(set.remove(&foo_a), &[], &["foo/a"]);
        assert_delta(set.remove(&foo_b), &[], &["foo/b"]);
    }

    #[test]
    fn ignores_narrower_prefix_under_broad_root() {
        let mut set = CoveringPrefixSet::default();
        let foo = prefix("foo");
        let foo_bar = prefix("foo/bar");

        assert_delta(set.add(foo.clone()), &["foo"], &[]);
        assert_delta(set.add(foo_bar.clone()), &[], &[]);
        assert_delta(set.remove(&foo_bar), &[], &[]);
        assert_delta(set.add(foo_bar.clone()), &[], &[]);
        assert_delta(set.remove(&foo), &["foo/bar"], &["foo"]);
        assert_delta(set.remove(&foo_bar), &[], &["foo/bar"]);
    }

    #[test]
    fn reexposes_multiple_minimal_roots_after_deep_nesting() {
        let mut set = CoveringPrefixSet::default();
        let deep = prefix("foo/a/b/c");
        let middle = prefix("foo/a/b");
        let branch = prefix("foo/a/d/e");
        let branch_leaf = prefix("foo/a/d/e/f");
        let broad = prefix("foo");
        let direct = prefix("foo/z");

        assert_delta(set.add(deep.clone()), &["foo/a/b/c"], &[]);
        assert_delta(set.add(middle.clone()), &["foo/a/b"], &["foo/a/b/c"]);
        assert_delta(set.add(branch.clone()), &["foo/a/d/e"], &[]);
        assert_delta(set.add(branch_leaf.clone()), &[], &[]);
        assert_delta(set.add(broad.clone()), &["foo"], &["foo/a/b", "foo/a/d/e"]);
        assert_delta(set.add(direct.clone()), &[], &[]);
        assert_delta(
            set.remove(&broad),
            &["foo/a/b", "foo/a/d/e", "foo/z"],
            &["foo"],
        );
        assert_delta(set.remove(&middle), &["foo/a/b/c"], &["foo/a/b"]);
        assert_delta(set.remove(&branch), &["foo/a/d/e/f"], &["foo/a/d/e"]);
    }

    #[test]
    fn empty_prefix_covers_and_reexposes_everything() {
        let mut set = CoveringPrefixSet::default();
        let empty = TrackNamespacePrefix::new();

        assert_delta(set.add(prefix("foo/a")), &["foo/a"], &[]);
        assert_delta(set.add(prefix("bar/b")), &["bar/b"], &[]);
        assert_delta(set.add(empty.clone()), &[""], &["foo/a", "bar/b"]);
        assert_delta(set.add(empty.clone()), &[], &[]);
        assert_delta(set.add(prefix("baz")), &[], &[]);
        assert_delta(set.remove(&empty), &["foo/a", "bar/b", "baz"], &[""]);
    }

    #[test]
    fn removing_absent_prefix_is_a_no_op() {
        let mut set = CoveringPrefixSet::default();
        let foo = prefix("foo");

        assert_delta(set.remove(&foo), &[], &[]);
        assert_delta(set.add(foo.clone()), &["foo"], &[]);
        assert_delta(set.remove(&prefix("bar")), &[], &[]);
        assert_delta(set.remove(&foo), &[], &["foo"]);
    }

    #[test]
    fn containment_uses_tuple_fields_not_rendered_paths() {
        let mut set = CoveringPrefixSet::default();
        let single_field = TrackNamespacePrefix {
            fields: vec![TupleField::from_utf8("foo/bar")],
        };
        let two_fields = prefix("foo/bar");

        assert_eq!(single_field.to_utf8_path(), two_fields.to_utf8_path());

        let delta = set.add(single_field.clone());
        assert_eq!(delta.added, HashSet::from([single_field.clone()]));
        assert!(delta.removed.is_empty());

        let delta = set.add(two_fields.clone());
        assert_eq!(delta.added, HashSet::from([two_fields.clone()]));
        assert!(delta.removed.is_empty());

        let delta = set.remove(&single_field);
        assert!(delta.added.is_empty());
        assert_eq!(delta.removed, HashSet::from([single_field]));

        assert_delta(set.remove(&two_fields), &[], &["foo/bar"]);
    }
}
