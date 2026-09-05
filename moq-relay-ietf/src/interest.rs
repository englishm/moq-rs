// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Downstream interest tracking for pull-through cache entries.
//!
//! When a relay serves a track it does not publish itself, it subscribes
//! upstream and caches the resulting reader so later subscribers can share it.
//! Nothing previously counted how many downstream subscribers were actually
//! using that cached reader, so the upstream subscription lived until the
//! upstream session died — the relay kept pulling bytes for a track nobody was
//! watching.
//!
//! [`TrackInterest`] is a reference count with the ability to await "nobody has
//! held a guard for a while". Each downstream subscriber holds a
//! [`TrackInterestGuard`] for as long as it is being served, so the count is
//! maintained by RAII and a cancelled subscriber cannot leak a reference.
//!
//! Guards hold a strong reference to the counter itself rather than a cache key.
//! If a cache entry is evicted and a replacement is created for the same track
//! name, an outstanding guard from the old entry decrements the old counter
//! instead of making the new entry look busy.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

/// Shared interest counter for one cache entry.
#[derive(Clone, Debug)]
pub struct TrackInterest {
    inner: Arc<InterestInner>,
}

#[derive(Debug)]
struct InterestInner {
    /// Number of live guards. The count lives in the watch value so that
    /// mutations and change notifications cannot get out of step.
    count: watch::Sender<usize>,
}

impl Default for TrackInterest {
    fn default() -> Self {
        Self::new()
    }
}

impl TrackInterest {
    pub fn new() -> Self {
        let (count, _) = watch::channel(0);
        Self {
            inner: Arc::new(InterestInner { count }),
        }
    }

    /// Register a downstream subscriber, releasing the interest on drop.
    ///
    /// Callers must create the guard while holding the same cache lock that
    /// [`TrackInterest::is_idle`] is checked under, so a subscriber arriving
    /// concurrently with eviction is either counted or gets a fresh entry.
    pub fn guard(&self) -> TrackInterestGuard {
        self.inner.count.send_modify(|count| *count += 1);
        TrackInterestGuard {
            inner: self.inner.clone(),
        }
    }

    /// Current number of downstream subscribers.
    pub fn count(&self) -> usize {
        *self.inner.count.borrow()
    }

    /// True when no downstream subscriber is being served from this entry.
    pub fn is_idle(&self) -> bool {
        self.count() == 0
    }

    /// True when both handles refer to the same counter, i.e. the same cache
    /// entry generation.
    pub fn same_generation(&self, other: &TrackInterest) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Resolve once the count has been continuously zero for `grace`.
    ///
    /// The timer restarts whenever a subscriber appears, so a track that keeps
    /// picking up new subscribers is never considered idle. A zero `grace`
    /// resolves as soon as the count reaches zero.
    pub async fn idle_for(&self, grace: Duration) {
        let mut rx = self.inner.count.subscribe();

        loop {
            // Scoped so the borrow is released before the awaits below.
            let busy = { *rx.borrow_and_update() > 0 };

            if busy {
                // Wait for the count to change at all before re-checking.
                if rx.changed().await.is_err() {
                    // Sender gone, so the entry is unreachable; treat as idle.
                    return;
                }
                continue;
            }

            if grace.is_zero() {
                return;
            }

            tokio::select! {
                _ = tokio::time::sleep(grace) => return,
                // A subscriber arrived (or another change landed) inside the
                // grace period; go around again and restart the timer.
                res = rx.changed() => {
                    if res.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// Held for as long as a downstream subscriber is served from a cache entry.
#[derive(Debug)]
pub struct TrackInterestGuard {
    inner: Arc<InterestInner>,
}

impl Drop for TrackInterestGuard {
    fn drop(&mut self) {
        self.inner.count.send_modify(|count| {
            // Saturating because an underflow here would make a busy track look
            // permanently busy, pinning the upstream subscription forever.
            *count = count.saturating_sub(1);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::time::Instant;

    const GRACE: Duration = Duration::from_millis(50);

    #[test]
    fn guards_count_subscribers() {
        let interest = TrackInterest::new();
        assert_eq!(interest.count(), 0);
        assert!(interest.is_idle());

        let first = interest.guard();
        assert_eq!(interest.count(), 1);
        assert!(!interest.is_idle());

        let second = interest.guard();
        assert_eq!(interest.count(), 2);

        drop(first);
        assert_eq!(interest.count(), 1);

        drop(second);
        assert_eq!(interest.count(), 0);
        assert!(interest.is_idle());
    }

    #[tokio::test(start_paused = true)]
    async fn idle_for_resolves_when_nobody_subscribes() {
        let interest = TrackInterest::new();
        let start = Instant::now();

        interest.idle_for(GRACE).await;

        assert!(start.elapsed() >= GRACE);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_for_waits_for_the_last_subscriber_to_leave() {
        let interest = TrackInterest::new();
        let guard = interest.guard();

        let idle = interest.idle_for(GRACE);
        tokio::pin!(idle);

        // Busy: must not resolve no matter how long we wait.
        tokio::select! {
            _ = &mut idle => panic!("a busy track must never look idle"),
            _ = tokio::time::sleep(GRACE * 10) => {}
        }

        drop(guard);
        idle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_subscriber_restarts_the_grace_period() {
        let interest = TrackInterest::new();

        let idle = interest.idle_for(GRACE);
        tokio::pin!(idle);

        // Part-way through the grace period a subscriber arrives, so the entry
        // must stop looking idle rather than being evicted out from under it.
        tokio::select! {
            _ = &mut idle => panic!("resolved before the grace period elapsed"),
            _ = tokio::time::sleep(GRACE / 2) => {}
        }

        let guard = interest.guard();

        tokio::select! {
            _ = &mut idle => panic!("resolved while a subscriber was present"),
            _ = tokio::time::sleep(GRACE * 10) => {}
        }

        // Once it leaves, a full grace period must pass again.
        drop(guard);
        let after_drop = Instant::now();
        idle.await;
        assert!(after_drop.elapsed() >= GRACE);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_grace_resolves_immediately_when_idle() {
        let interest = TrackInterest::new();
        interest.idle_for(Duration::ZERO).await;
    }

    #[test]
    fn generations_are_distinguished_by_identity() {
        let interest = TrackInterest::new();
        let clone = interest.clone();
        let replacement = TrackInterest::new();

        assert!(interest.same_generation(&clone));
        assert!(!interest.same_generation(&replacement));
    }

    #[test]
    fn a_stale_guard_does_not_make_a_replacement_look_busy() {
        // A subscriber served by an evicted entry can outlive that entry. Its
        // guard must decrement the old counter, not the new one.
        let old = TrackInterest::new();
        let stale_guard = old.guard();

        let new = TrackInterest::new();
        assert!(new.is_idle());

        drop(stale_guard);

        assert_eq!(old.count(), 0);
        assert!(new.is_idle(), "replacement entry should still be idle");
    }
}
