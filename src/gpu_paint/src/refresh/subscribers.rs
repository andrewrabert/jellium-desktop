//! Scoped registrations for display refresh changes.
//! Publishers release the registry lock before invoking a recipient.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

struct Callback {
    active: AtomicBool,
    call: Box<dyn Fn() + Send + Sync>,
}

impl Callback {
    fn invoke(&self) {
        if self.active.load(Ordering::Acquire) {
            (self.call)();
        }
    }
}

#[derive(Default)]
struct Registry {
    next_id: u64,
    callbacks: BTreeMap<u64, Arc<Callback>>,
}

/// A set of independent registrations, including duplicate callback functions.
#[derive(Default)]
pub(crate) struct Subscribers {
    registry: Arc<Mutex<Registry>>,
}

impl Subscribers {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn subscribe(&self, callback: impl Fn() + Send + Sync + 'static) -> Subscription {
        let callback = Arc::new(Callback {
            active: AtomicBool::new(true),
            call: Box::new(callback),
        });
        let mut registry = self.registry.lock();
        // Exhausting IDs must never replace an existing registration.
        #[allow(clippy::expect_used)]
        let id = registry
            .next_id
            .checked_add(1)
            .expect("notification IDs exhausted");
        registry.next_id = id;
        registry.callbacks.insert(id, Arc::clone(&callback));
        Subscription {
            id,
            callback,
            registry: Arc::downgrade(&self.registry),
        }
    }

    pub(crate) fn notify(&self) {
        let callbacks: Vec<_> = self.registry.lock().callbacks.values().cloned().collect();
        for callback in callbacks {
            callback.invoke();
        }
    }
}

/// Dropping this token revokes queued notifications and removes only its own
/// registration. A callback already executing may finish; recipients that own
/// native resources must additionally serialize revocation with their work.
#[must_use = "dropping the subscription unregisters its callback"]
pub struct Subscription {
    id: u64,
    callback: Arc<Callback>,
    registry: Weak<Mutex<Registry>>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.callback.active.store(false, Ordering::Release);
        if let Some(registry) = self.registry.upgrade() {
            registry.lock().callbacks.remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn dropping_one_registration_preserves_the_other() {
        let notifications = Subscribers::new();
        let count = Arc::new(AtomicUsize::new(0));
        let callback = {
            let count = Arc::clone(&count);
            move || {
                count.fetch_add(1, Ordering::Relaxed);
            }
        };
        let first = notifications.subscribe(callback.clone());
        let second = notifications.subscribe(callback);
        drop(first);
        notifications.notify();
        assert_eq!(count.load(Ordering::Relaxed), 1);
        drop(second);
        notifications.notify();
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn copied_notification_is_revoked_before_invocation() {
        let notifications = Subscribers::new();
        let count = Arc::new(AtomicUsize::new(0));
        let token = notifications.subscribe({
            let count = Arc::clone(&count);
            move || {
                count.fetch_add(1, Ordering::Relaxed);
            }
        });
        let queued = Arc::clone(&token.callback);
        drop(token);
        queued.invoke();
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn callback_can_remove_another_registration() {
        let notifications = Subscribers::new();
        let second = Arc::new(Mutex::new(None));
        let first = notifications.subscribe({
            let second = Arc::clone(&second);
            move || drop(second.lock().take())
        });
        let count = Arc::new(AtomicUsize::new(0));
        *second.lock() = Some(notifications.subscribe({
            let count = Arc::clone(&count);
            move || {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }));
        notifications.notify();
        assert_eq!(count.load(Ordering::Relaxed), 0);
        drop(first);
    }
}
