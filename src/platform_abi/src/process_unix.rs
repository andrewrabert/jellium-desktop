use std::ffi::c_int;
use std::sync::OnceLock;

use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

static GUARD: OnceLock<SignalGuard> = OnceLock::new();
static SHUTDOWN_CB: OnceLock<fn()> = OnceLock::new();

pub fn install_shutdown(on_shutdown: fn()) {
    // Set before arming: the handler dereferences this, so it must be live by
    // the time a signal can fire.
    let _ = SHUTDOWN_CB.set(on_shutdown);
    let g = unsafe { install_guard(on_shutdown_signal) };
    let _ = GUARD.set(g);
}

// Runs in signal context: must stay async-signal-safe — no allocation, no
// locking, no logging.
extern "C" fn on_shutdown_signal(_sig: c_int) {
    if let Some(cb) = SHUTDOWN_CB.get() {
        cb();
    }
}

struct SignalGuard {
    prev_int: Option<SigAction>,
    prev_term: Option<SigAction>,
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        if let Some(prev) = &self.prev_int {
            let _ = unsafe { sigaction(Signal::SIGINT, prev) };
        }
        if let Some(prev) = &self.prev_term {
            let _ = unsafe { sigaction(Signal::SIGTERM, prev) };
        }
    }
}

/// # Safety
/// `handler` must be async-signal-safe: it runs from inside a `sigaction`
/// handler installed on SIGINT/SIGTERM.
unsafe fn install_guard(handler: extern "C" fn(c_int)) -> SignalGuard {
    let sa = SigAction::new(
        SigHandler::Handler(handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    SignalGuard {
        prev_int: unsafe { sigaction(Signal::SIGINT, &sa) }.ok(),
        prev_term: unsafe { sigaction(Signal::SIGTERM, &sa) }.ok(),
    }
}
