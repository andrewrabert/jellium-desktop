//! One single-slot coalescing mailbox, shared by every actor in the workspace
//! that hands work to a dedicated thread.

use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use std::time::Instant;

/// Shared, cloneable handle to one actor's state: a `parking_lot` mutex plus
/// the condvar its consumer blocks on. Coalescing is the state's own business
/// — the mailbox owns only the lock, the wake, and the blocking wait.
pub struct Mailbox<S> {
    inner: Arc<Shared<S>>,
}

struct Shared<S> {
    state: Mutex<S>,
    cv: Condvar,
}

impl<S> Clone for Mailbox<S> {
    fn clone(&self) -> Mailbox<S> {
        Mailbox {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<S> Mailbox<S> {
    pub fn new(state: S) -> Mailbox<S> {
        Mailbox {
            inner: Arc::new(Shared {
                state: Mutex::new(state),
                cv: Condvar::new(),
            }),
        }
    }

    /// Run `f` under the lock, then wake every blocked waiter. The wake is a
    /// broadcast: several waiters can be parked on distinct predicates, and
    /// waking only one would let it swallow another's wakeup.
    pub fn update<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        let mut state = self.inner.state.lock();
        let out = f(&mut state);
        drop(state);
        self.inner.cv.notify_all();
        out
    }

    /// Run `f` under the lock and wake nobody.
    pub fn peek<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        f(&self.inner.state.lock())
    }

    /// Block until `ready` holds of the state, then run `take` under the same
    /// lock without releasing it in between.
    pub fn wait<R>(&self, ready: impl Fn(&S) -> bool, take: impl FnOnce(&mut S) -> R) -> R {
        let mut state = self.inner.state.lock();
        while !ready(&state) {
            self.inner.cv.wait(&mut state);
        }
        take(&mut state)
    }

    /// Block until `ready` holds of the state or `deadline` passes, then run
    /// `take` under the same lock. `None` when the deadline passed with `ready`
    /// still false.
    pub fn wait_until<R>(
        &self,
        deadline: Instant,
        ready: impl Fn(&S) -> bool,
        take: impl FnOnce(&mut S) -> R,
    ) -> Option<R> {
        let mut state = self.inner.state.lock();
        while !ready(&state) {
            // The timeout is only decisive once the predicate has been asked
            // again under the reacquired lock: a wake and the deadline can land
            // together.
            if self.inner.cv.wait_until(&mut state, deadline).timed_out() && !ready(&state) {
                return None;
            }
        }
        Some(take(&mut state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_is_visible_to_peek() {
        let mb = Mailbox::new(0u32);
        mb.update(|s| *s = 7);
        assert_eq!(mb.peek(|s| *s), 7);
    }

    #[test]
    fn wait_returns_immediately_when_already_ready() {
        let mb = Mailbox::new(Some(3u32));
        assert_eq!(mb.wait(|s| s.is_some(), Option::take), Some(3));
        assert_eq!(mb.peek(|s| *s), None);
    }

    #[test]
    fn wait_wakes_on_update_from_another_thread() {
        let mb = Mailbox::new(None::<u32>);
        let producer = mb.clone();
        let t = std::thread::spawn(move || {
            producer.update(|s| *s = Some(9));
        });
        assert_eq!(mb.wait(|s| s.is_some(), Option::take), Some(9));
        t.join().expect("producer thread");
    }

    #[test]
    fn wait_until_returns_none_when_the_deadline_passes() {
        let mb = Mailbox::new(None::<u32>);
        let deadline = Instant::now() + std::time::Duration::from_millis(10);
        assert_eq!(mb.wait_until(deadline, |s| s.is_some(), Option::take), None);
        assert!(Instant::now() >= deadline);
    }

    #[test]
    fn wait_until_takes_a_state_that_became_ready() {
        let mb = Mailbox::new(None::<u32>);
        let producer = mb.clone();
        let t = std::thread::spawn(move || producer.update(|s| *s = Some(4)));
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        assert_eq!(
            mb.wait_until(deadline, |s| s.is_some(), Option::take),
            Some(Some(4))
        );
        t.join().expect("producer thread");
    }

    #[test]
    fn update_wakes_every_waiter() {
        let mb = Mailbox::new(false);
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let mb = mb.clone();
                std::thread::spawn(move || mb.wait(|s| *s, |_| ()))
            })
            .collect();
        mb.update(|s| *s = true);
        for w in waiters {
            w.join().expect("waiter thread");
        }
    }
}
