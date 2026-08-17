// SPDX-FileCopyrightText: 2024-2026 Cloudflare Inc., Luke Curley, Mike English and contributors
// SPDX-FileCopyrightText: 2023-2024 Luke Curley and contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    fmt,
    future::Future,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, Weak},
    task,
};

struct StateInner<T> {
    value: T,
    wakers: Vec<task::Waker>,
    epoch: usize,
    dropped: Option<()>,
}

impl<T> StateInner<T> {
    pub fn new(value: T) -> Self {
        Self {
            value,
            wakers: Vec::new(),
            epoch: 0,
            dropped: Some(()),
        }
    }

    pub fn register(&mut self, waker: &task::Waker) {
        self.wakers.retain(|existing| !existing.will_wake(waker));
        self.wakers.push(waker.clone());
    }

    pub fn notify(&mut self) {
        self.epoch += 1;
        for waker in self.wakers.drain(..) {
            waker.wake();
        }
    }
}

impl<T: Default> Default for StateInner<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: fmt::Debug> fmt::Debug for StateInner<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(f)
    }
}

pub struct State<T> {
    state: Arc<Mutex<StateInner<T>>>,
    drop: Arc<StateDrop<T>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateError {
    Poisoned,
}

/// Take the inner lock, recovering if a previous holder panicked.
///
/// Poisoning means another task panicked while holding this state, which has
/// already failed whatever that task was doing. One `State` is shared by every
/// subscription, track reader and waiter hanging off it, so propagating the
/// poison would turn one failed request into a panic in every unrelated task that
/// later touches the same state. Recovering keeps the damage where it started.
///
/// The state may be a half-finished update, which is why this is logged rather
/// than passed over in silence. The poison flag is cleared so the log records one
/// line per incident instead of one per access for the rest of the process's life.
///
/// Callers that would rather decide for themselves have [`State::try_lock`] and
/// [`State::try_lock_mut`], which still report [`StateError::Poisoned`].
fn lock_recovering<T>(state: &Mutex<StateInner<T>>) -> MutexGuard<'_, StateInner<T>> {
    match state.lock() {
        Ok(lock) => lock,
        Err(poisoned) => {
            state.clear_poison();
            tracing::error!("recovered a poisoned watch state: a task panicked while holding it");
            poisoned.into_inner()
        }
    }
}

impl<T> State<T> {
    pub fn new(initial: T) -> Self {
        let state = Arc::new(Mutex::new(StateInner::new(initial)));

        Self {
            state: state.clone(),
            drop: Arc::new(StateDrop { state }),
        }
    }

    pub fn lock(&self) -> StateRef<'_, T> {
        StateRef {
            state: self.state.clone(),
            drop: self.drop.clone(),
            lock: lock_recovering(&self.state),
        }
    }

    pub fn try_lock(&self) -> Result<StateRef<'_, T>, StateError> {
        let lock = self.state.lock().map_err(|_| StateError::Poisoned)?;
        Ok(StateRef {
            state: self.state.clone(),
            drop: self.drop.clone(),
            lock,
        })
    }

    pub fn lock_mut(&self) -> Option<StateMut<'_, T>> {
        let lock = lock_recovering(&self.state);
        lock.dropped?;
        Some(StateMut {
            lock,
            _drop: self.drop.clone(),
        })
    }

    pub fn try_lock_mut(&self) -> Result<Option<StateMut<'_, T>>, StateError> {
        let lock = self.state.lock().map_err(|_| StateError::Poisoned)?;
        if lock.dropped.is_none() {
            return Ok(None);
        }

        Ok(Some(StateMut {
            lock,
            _drop: self.drop.clone(),
        }))
    }

    pub fn downgrade(&self) -> StateWeak<T> {
        StateWeak {
            state: Arc::downgrade(&self.state),
            drop: Arc::downgrade(&self.drop),
        }
    }

    pub fn split(self) -> (Self, Self) {
        let state = self.state.clone();
        (
            self, // important that we don't make a new drop here
            Self {
                state: state.clone(),
                drop: Arc::new(StateDrop { state }),
            },
        )
    }
}

impl<T> Clone for State<T> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            drop: self.drop.clone(),
        }
    }
}

impl<T: Default> Default for State<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: fmt::Debug> fmt::Debug for State<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.state.try_lock() {
            Ok(lock) => lock.value.fmt(f),
            Err(_) => write!(f, "<locked>"),
        }
    }
}

pub struct StateRef<'a, T> {
    state: Arc<Mutex<StateInner<T>>>,
    lock: MutexGuard<'a, StateInner<T>>,
    drop: Arc<StateDrop<T>>,
}

impl<'a, T> StateRef<'a, T> {
    // Release the lock and wait for a notification when next updated.
    pub fn modified(self) -> Option<StateChanged<T>> {
        self.lock.dropped?;

        Some(StateChanged {
            state: self.state,
            epoch: self.lock.epoch,
        })
    }

    // Upgrade to a mutable references that automatically calls notify on drop.
    pub fn into_mut(self) -> Option<StateMut<'a, T>> {
        self.lock.dropped?;
        Some(StateMut {
            lock: self.lock,
            _drop: self.drop,
        })
    }

    /// Mutate locally buffered state after the other half has closed.
    pub(crate) fn into_mut_closed(self) -> StateMut<'a, T> {
        StateMut {
            lock: self.lock,
            _drop: self.drop,
        }
    }
}

impl<T> Deref for StateRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.lock.value
    }
}

impl<T: fmt::Debug> fmt::Debug for StateRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.lock.fmt(f)
    }
}

pub struct StateMut<'a, T> {
    lock: MutexGuard<'a, StateInner<T>>,
    _drop: Arc<StateDrop<T>>,
}

impl<T> Deref for StateMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.lock.value
    }
}

impl<T> DerefMut for StateMut<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.lock.value
    }
}

impl<T> Drop for StateMut<'_, T> {
    fn drop(&mut self) {
        self.lock.notify();
    }
}

impl<T: fmt::Debug> fmt::Debug for StateMut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.lock.fmt(f)
    }
}

pub struct StateChanged<T> {
    state: Arc<Mutex<StateInner<T>>>,
    epoch: usize,
}

impl<T> Future for StateChanged<T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        // TODO is there an API we can make that doesn't drop this lock?
        let mut state = lock_recovering(&self.state);

        if state.epoch > self.epoch {
            task::Poll::Ready(())
        } else {
            state.register(cx.waker());
            task::Poll::Pending
        }
    }
}

pub struct StateWeak<T> {
    state: Weak<Mutex<StateInner<T>>>,
    drop: Weak<StateDrop<T>>,
}

impl<T> StateWeak<T> {
    pub fn upgrade(&self) -> Option<State<T>> {
        if let (Some(state), Some(drop)) = (self.state.upgrade(), self.drop.upgrade()) {
            Some(State { state, drop })
        } else {
            None
        }
    }
}

impl<T> Clone for StateWeak<T> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            drop: self.drop.clone(),
        }
    }
}

struct StateDrop<T> {
    state: Arc<Mutex<StateInner<T>>>,
}

impl<T> Drop for StateDrop<T> {
    fn drop(&mut self) {
        // Recovering rather than bailing out: skipping the notify below would
        // leave every task awaiting this state parked forever, so one panic
        // elsewhere would hang the subscriptions built on it.
        let mut state = lock_recovering(&self.state);
        state.dropped = None;
        state.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` on another thread and let it panic there, returning once the panic
    /// has unwound. The state's mutex is poisoned from then on.
    fn panic_while_holding(f: impl FnOnce() + Send + 'static) {
        let previous = std::panic::take_hook();
        // The panic is the point of the test, so keep its backtrace out of the
        // test output.
        std::panic::set_hook(Box::new(|_| {}));
        let joined = std::thread::spawn(f).join();
        std::panic::set_hook(previous);
        assert!(joined.is_err(), "the helper thread was supposed to panic");
    }

    /// A panic while holding the state used to poison the mutex and turn every
    /// later access into a panic of its own. One state is shared by every reader
    /// and waiter built on it, so that spread a single failed request across all
    /// of them.
    #[test]
    fn a_panic_under_the_lock_does_not_spread() {
        let state = State::new(1u32);

        let poisoner = state.clone();
        panic_while_holding(move || {
            let mut guard = poisoner.lock_mut().expect("state is live");
            *guard = 2;
            panic!("while holding the state");
        });

        assert_eq!(*state.lock(), 2, "the completed write is still visible");

        let mut guard = state.lock_mut().expect("mutation still works afterwards");
        *guard = 3;
        drop(guard);
        assert_eq!(*state.lock(), 3);
    }

    /// Callers that want to know are still told, which is what `queue.rs` and the
    /// PUBLISH paths rely on. Only the first recovery clears the flag, so this
    /// runs before anything calls `lock`.
    #[test]
    fn try_lock_still_reports_poisoning() {
        let state = State::new(1u32);

        let poisoner = state.clone();
        panic_while_holding(move || {
            let _guard = poisoner.lock_mut().expect("state is live");
            panic!("while holding the state");
        });

        assert_eq!(state.try_lock().err(), Some(StateError::Poisoned));
    }

    /// Dropping the state has to wake its waiters even when the lock was
    /// poisoned first. Bailing out of the drop instead skipped the notify, so
    /// anything awaiting the state waited for a change that could never come.
    ///
    /// The panic here holds a shared `StateRef` on purpose. A `StateMut` notifies
    /// as it unwinds, which wakes the waiter for an unrelated reason and would
    /// let this pass either way.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_a_poisoned_state_still_wakes_waiters() {
        let (writer, reader) = State::new(1u32).split();

        let changed = reader.lock().modified().expect("state is live");

        let poisoner = reader.clone();
        panic_while_holding(move || {
            let _guard = poisoner.lock();
            panic!("while holding the state");
        });

        drop(writer);

        tokio::time::timeout(std::time::Duration::from_secs(5), changed)
            .await
            .expect("dropping the state wakes the waiter");
    }
}
