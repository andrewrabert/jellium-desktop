//! Process-wide shutdown signal.
//!
//! A single atomic flag (`SIGNAL.requested`) gates teardown across the whole
//! process. Shared between SIGINT/SIGTERM, UI close, hotkeys, and CEF
//! window-close paths. `jfn_shutdown_initiate` is idempotent and
//! async-signal-safe up to whatever the registered handler does — the call
//! itself just CAS's the flag and runs the registered handler.
//!
//! A handler registered via `jfn_shutdown_set_handler` runs on the first
//! call — it runs *inline on the calling thread*, so it MUST only signal or
//! wake (e.g. signal the shutdown manager); it must never block, close a
//! browser, or reenter CEF. The actual teardown is orchestrated off-thread by
//! the manager, which then calls `jfn_shutdown_fanout` to wake every
//! subsystem thread that registered via `jfn_shutdown_register_waker`.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use jfn_wake_event::WakeEvent;

struct ShutdownSignal {
    requested: AtomicBool,
    handler: AtomicPtr<()>,
}
impl ShutdownSignal {
    const fn new() -> Self {
        Self {
            requested: AtomicBool::new(false),
            handler: AtomicPtr::new(std::ptr::null_mut()),
        }
    }
    fn install(&self, handler: Option<fn()>) {
        self.handler.store(
            handler.map_or(std::ptr::null_mut(), |f| f as *mut ()),
            Ordering::SeqCst,
        );
        // The store/load pairs participate in one total order, so registration
        // and initiation cannot both miss the other. Duplicate wakes are fine.
        if self.requested.load(Ordering::SeqCst)
            && let Some(handler) = handler
        {
            handler();
        }
    }
    fn initiate(&self) {
        if self.requested.swap(true, Ordering::SeqCst) {
            return;
        }
        let handler = self.handler.load(Ordering::SeqCst);
        if !handler.is_null() {
            // SAFETY: install only stores fn() pointers, which remain valid
            // throughout the process. This performs no allocation or locking.
            let callback: fn() = unsafe { std::mem::transmute(handler) };
            callback();
        }
    }
}
static SIGNAL: ShutdownSignal = ShutdownSignal::new();
static WAKERS: Mutex<Vec<&'static WakeEvent>> = Mutex::new(Vec::new());

/// Returns true if [`jfn_shutdown_initiate`] has been called at least once.
pub fn jfn_shutting_down() -> bool {
    SIGNAL.requested.load(Ordering::Acquire)
}

/// Install (or clear, with `None`) the idempotent wake callback invoked on the
/// first [`jfn_shutdown_initiate`] call. The callback runs inline on the
/// calling thread (possibly a signal handler or a CEF dispatch), so it MUST
/// only signal/wake — never block, close a browser, or reenter CEF.
/// Registration replays an existing request; a race can deliver duplicate wakes.
pub fn jfn_shutdown_set_handler(handler: Option<fn()>) {
    SIGNAL.install(handler);
}

/// Register a wake event that will be signaled when the shutdown manager
/// fans out (`jfn_shutdown_fanout`). One uniform observation pattern across
/// long-lived threads — each subsystem owns its own `WakeEvent`, polls its
/// own fd/handle alongside its native event source, and exits on signal.
///
/// `ev` must remain live for the rest of the process.
pub fn jfn_shutdown_register_waker(ev: &'static WakeEvent) {
    WAKERS.lock().push(ev);
}

/// Signal every registered waker. Called from the manager once it observes
/// shutdown — never from a signal handler (this locks a mutex).
pub fn jfn_shutdown_fanout() {
    let wakers = WAKERS.lock();
    for ev in wakers.iter() {
        ev.signal();
    }
}

/// Idempotent. First call: sets the flag and runs the registered handler.
/// Subsequent calls are no-ops. Async-signal-safe up to whatever the
/// handler does.
pub fn jfn_shutdown_initiate() {
    SIGNAL.initiate();
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, atomic::AtomicUsize};
    #[test]
    fn registration_replays_shutdown_and_racing_registration_never_loses_it() {
        static WAKES: AtomicUsize = AtomicUsize::new(0);
        fn wake() {
            WAKES.fetch_add(1, Ordering::SeqCst);
        }
        let signal = ShutdownSignal::new();
        signal.initiate();
        signal.install(Some(wake));
        assert_eq!(WAKES.load(Ordering::SeqCst), 1);
        signal.initiate();
        assert_eq!(WAKES.load(Ordering::SeqCst), 1);
        for _ in 0..64 {
            WAKES.store(0, Ordering::SeqCst);
            let signal = Arc::new(ShutdownSignal::new());
            let barrier = Arc::new(Barrier::new(2));
            let other = Arc::clone(&signal);
            let worker_barrier = Arc::clone(&barrier);
            let worker = std::thread::spawn(move || {
                worker_barrier.wait();
                other.install(Some(wake));
            });
            barrier.wait();
            signal.initiate();
            worker.join().unwrap();
            assert!(WAKES.load(Ordering::SeqCst) >= 1);
        }
    }
}
