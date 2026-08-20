// SPDX-FileCopyrightText: 2026 Cloudflare Inc.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared PUBLISH_DONE drain accounting (draft-18 §10.11).
//!
//! PUBLISH_DONE ends a subscription, but it carries a Stream Count precisely
//! because data streams the publisher already opened can still be in flight:
//!
//! > Stream Count: An integer indicating the number of data streams the
//! > publisher opened for this subscription, including streams that contained
//! > no Objects (e.g., an empty Subgroup). This helps the subscriber know if it
//! > has received all of the data published in this subscription by comparing
//! > the number of streams received.
//!
//! Destroying subscription state as soon as PUBLISH_DONE is parsed routes those
//! streams nowhere and silently drops Objects the publisher legitimately sent.
//! §10.11 instead destroys state "once all open streams for the subscription
//! have closed", backed by a timeout for a publisher that announced streams it
//! never sent.
//!
//! Both subscription flavours need this: PUBLISH-created subscriptions
//! (`PublishReceivedRecv`) and SUBSCRIBE-created ones (`SubscribeRecv`). `T` is
//! the terminal value each one applies when the drain completes.

/// What [`StreamDrain::arm`] did with a PUBLISH_DONE.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DoneOutcome {
    /// The subscription ended; the caller releases its session-level state.
    Finished,
    /// Streams announced by Stream Count are still outstanding. The caller
    /// keeps the state and arms the drain timeout.
    DrainArmed,
    /// A drain was already running, so this PUBLISH_DONE changed nothing. The
    /// caller must not arm a second timer.
    AlreadyDraining,
}

/// Tracks the data streams of one subscription against its Stream Count.
pub(crate) struct StreamDrain<T> {
    /// Streams whose SUBGROUP_HEADER has been accepted, compared against the
    /// Stream Count. Datagrams are not streams and do not count.
    received: u64,
    /// Accepted streams that are still being read.
    open: u64,
    /// Terminal value and Stream Count recorded while streams are outstanding.
    pending: Option<(T, u64)>,
    /// Set once a terminal value has been handed out, so a subscription can
    /// never be re-armed or ended twice.
    finished: bool,
}

impl<T> Default for StreamDrain<T> {
    fn default() -> Self {
        Self {
            received: 0,
            open: 0,
            pending: None,
            finished: false,
        }
    }
}

impl<T> StreamDrain<T> {
    /// Record a data stream arriving.
    ///
    /// Counted when the SUBGROUP_HEADER is accepted, because §10.11 counts
    /// "streams that contained no Objects (e.g., an empty Subgroup)" and so
    /// cannot wait for a first Object. Must be paired with
    /// [`note_stream_finished`](Self::note_stream_finished).
    pub fn note_stream_received(&mut self) {
        self.received = self.received.saturating_add(1);
        self.open = self.open.saturating_add(1);
    }

    /// Record a data stream finishing, returning the terminal value if this
    /// completed a deferred teardown.
    #[must_use]
    pub fn note_stream_finished(&mut self) -> Option<T> {
        self.open = self.open.saturating_sub(1);
        self.take_if_drained()
    }

    /// Record PUBLISH_DONE. Returns what the caller must do next, and the
    /// terminal value when the subscription can end immediately.
    pub fn arm(&mut self, terminal: T, stream_count: u64) -> (DoneOutcome, Option<T>) {
        if self.finished {
            return (DoneOutcome::Finished, None);
        }
        if self.pending.is_some() {
            return (DoneOutcome::AlreadyDraining, None);
        }

        self.pending = Some((terminal, stream_count));
        match self.take_if_drained() {
            Some(terminal) => (DoneOutcome::Finished, Some(terminal)),
            None => (DoneOutcome::DrainArmed, None),
        }
    }

    /// Give up waiting and return the recorded terminal value, if any.
    ///
    /// §10.11 requires this backstop because the publisher may have
    /// over-counted, reset a stream before its SUBGROUP_HEADER, or declared
    /// that it could not count its streams at all.
    #[must_use]
    pub fn timeout(&mut self) -> Option<T> {
        let terminal = self.pending.take().map(|(terminal, _)| terminal);
        if terminal.is_some() {
            self.finished = true;
        }
        terminal
    }

    /// True once the subscription has ended.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Mark the subscription ended without going through the drain, for
    /// terminal conditions that bypass PUBLISH_DONE entirely.
    pub fn mark_finished(&mut self) {
        self.pending = None;
        self.finished = true;
    }

    /// True while waiting for streams announced by PUBLISH_DONE.
    pub fn is_draining(&self) -> bool {
        self.pending.is_some()
    }

    fn take_if_drained(&mut self) -> Option<T> {
        let (_, stream_count) = self.pending.as_ref()?;
        // §10.11: wait for the announced streams, and for any stream still
        // being read, before destroying subscription state.
        if self.received < *stream_count || self.open > 0 {
            return None;
        }
        self.finished = true;
        self.pending.take().map(|(terminal, _)| terminal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_streams_announced_ends_immediately() {
        let mut drain = StreamDrain::default();
        assert_eq!(drain.arm("done", 0), (DoneOutcome::Finished, Some("done")));
    }

    #[test]
    fn an_announced_stream_defers_teardown_until_it_closes() {
        let mut drain = StreamDrain::default();
        assert_eq!(drain.arm("done", 1), (DoneOutcome::DrainArmed, None));

        drain.note_stream_received();
        assert_eq!(
            drain.note_stream_finished(),
            Some("done"),
            "the last announced stream closing ends the subscription"
        );
    }

    /// A stream that is still being read must hold the subscription open even
    /// though its header already satisfied the count.
    #[test]
    fn an_open_stream_holds_the_subscription_open() {
        let mut drain = StreamDrain::default();
        drain.note_stream_received();
        assert_eq!(drain.arm("done", 1), (DoneOutcome::DrainArmed, None));
        assert_eq!(drain.note_stream_finished(), Some("done"));
    }

    #[test]
    fn streams_closed_before_publish_done_end_it_immediately() {
        let mut drain = StreamDrain::default();
        drain.note_stream_received();
        assert_eq!(drain.note_stream_finished(), None);
        assert_eq!(drain.arm("done", 1), (DoneOutcome::Finished, Some("done")));
    }

    /// §10.11: a publisher unable to count its streams sends 2^62 - 1, which no
    /// subscriber can ever reach, so only the timeout ends the subscription.
    #[test]
    fn an_unknown_stream_count_is_resolved_by_the_timeout() {
        let mut drain = StreamDrain::default();
        assert_eq!(
            drain.arm("done", (1u64 << 62) - 1),
            (DoneOutcome::DrainArmed, None)
        );
        drain.note_stream_received();
        assert_eq!(drain.note_stream_finished(), None);
        assert_eq!(drain.timeout(), Some("done"));
        assert_eq!(drain.timeout(), None, "the timeout fires once");
    }

    #[test]
    fn a_duplicate_publish_done_does_not_rearm() {
        let mut drain = StreamDrain::default();
        assert_eq!(drain.arm("first", 2), (DoneOutcome::DrainArmed, None));
        assert_eq!(drain.arm("second", 0), (DoneOutcome::AlreadyDraining, None));

        drain.note_stream_received();
        assert_eq!(drain.note_stream_finished(), None);
        drain.note_stream_received();
        assert_eq!(
            drain.note_stream_finished(),
            Some("first"),
            "the original count and status survive a duplicate"
        );
    }

    /// More streams than announced must not underflow the open counter or
    /// resurrect a finished drain.
    /// A subscription must never be re-armed once it has ended, or a second
    /// PUBLISH_DONE would resurrect it.
    #[test]
    fn a_finished_drain_cannot_be_rearmed() {
        let mut drain = StreamDrain::default();
        assert_eq!(drain.arm("done", 0), (DoneOutcome::Finished, Some("done")));
        assert!(drain.is_finished());
        assert_eq!(
            drain.arm("again", 5),
            (DoneOutcome::Finished, None),
            "a finished subscription reports finished and hands out no second terminal"
        );
        assert!(!drain.is_draining());
    }

    #[test]
    fn extra_streams_after_teardown_are_inert() {
        let mut drain = StreamDrain::default();
        assert_eq!(drain.arm("done", 0), (DoneOutcome::Finished, Some("done")));

        drain.note_stream_received();
        assert_eq!(drain.note_stream_finished(), None);
        assert_eq!(drain.note_stream_finished(), None);
        assert!(!drain.is_draining());
    }
}
