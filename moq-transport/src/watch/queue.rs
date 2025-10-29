use super::State;
use futures::channel::oneshot;
use std::collections::VecDeque;

pub struct Queue<T> {
    state: State<VecDeque<(T, Option<oneshot::Sender<()>>)>>, // store optional notifier per item
}

impl<T> Queue<T> {
    /// Push an item onto the queue. Returns Err(item) if the queue has been closed.
    pub fn push(&mut self, item: T) -> Result<(), T> {
        match self.state.lock_mut() {
            Some(mut state) => state.push_back((item, None)),
            None => return Err(item),
        };

        Ok(())
    }

    /// Pop an item from the queue, waiting if necessary.
    pub async fn pop(&mut self) -> Option<T> {
        loop {
            // Scope 1: try to pop an item
            {
                let queue = self.state.lock();
                if !queue.is_empty() {
                    // Take mutable access only in a block
                    if let Some((item, notifier)) = {
                        let mut state_mut = queue.into_mut()?;
                        state_mut.pop_front()
                    } {
                        if let Some(tx) = notifier {
                            let _ = tx.send(()); // notify waiter
                        }
                        return Some(item);
                    }
                }
            }

            // Scope 2: wait for modifications
            let queue = self.state.lock();
            queue.modified()?.await;
        }
    }

    /// Drop the state
    pub fn close(self) -> Vec<T> {
        // Drain the queue of any remaining entries
        let res = match self.state.lock_mut() {
            Some(mut queue) => queue.drain(..).map(|(item, _)| item).collect(),
            _ => Vec::new(),
        };

        // Prevent any new entries from being added
        drop(self.state);

        res
    }

    /// Push an item and wait until it is popped.
    pub async fn push_and_wait_until_popped(&mut self, item: T) -> Result<(), T> {
        // Create a oneshot channel
        let (tx, rx) = oneshot::channel();

        // Push the item along with the sender
        match self.state.lock_mut() {
            Some(mut state) => state.push_back((item, Some(tx))),
            None => return Err(item),
        }

        // Wait until the item is popped
        let _ = rx.await;
        Ok(())
    }

    /// Split the queue into two handles that share the same underlying state.
    pub fn split(self) -> (Self, Self) {
        let state = self.state.split();
        (Self { state: state.0 }, Self { state: state.1 })
    }
}

impl<T> Clone for Queue<T> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self {
            state: State::new(Default::default()),
        }
    }
}
