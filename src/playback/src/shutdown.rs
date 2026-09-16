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
            let callback: fn() = unsafe { std::mem::transmute(handler) };
            callback();
        }
    }
}
static SIGNAL: ShutdownSignal = ShutdownSignal::new();
static WAKERS: Mutex<Vec<&'static WakeEvent>> = Mutex::new(Vec::new());

pub fn jfn_shutting_down() -> bool {
    SIGNAL.requested.load(Ordering::Acquire)
}

pub fn jfn_shutdown_set_handler(handler: Option<fn()>) {
    SIGNAL.install(handler);
}

pub fn jfn_shutdown_register_waker(ev: &'static WakeEvent) {
    WAKERS.lock().push(ev);
}

pub fn jfn_shutdown_fanout() {
    let wakers = WAKERS.lock();
    for ev in wakers.iter() {
        ev.signal();
    }
}

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
