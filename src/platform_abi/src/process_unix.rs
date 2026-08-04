use std::ffi::c_int;
use std::sync::OnceLock;

use crate::SignalGuard;

// Slot stays until process exit; the guard's Drop restores the original
// dispositions.
static GUARD: OnceLock<SignalGuard> = OnceLock::new();
static SHUTDOWN_CB: OnceLock<fn()> = OnceLock::new();

pub fn install_shutdown(on_shutdown: fn()) {
    // Set before arming: the handler dereferences this, so it must be live by
    // the time a signal can fire.
    let _ = SHUTDOWN_CB.set(on_shutdown);
    let g = unsafe { SignalGuard::install(on_shutdown_signal) };
    let _ = GUARD.set(g);
}

// Runs in signal context: must stay async-signal-safe — no allocation, no
// locking, no logging.
extern "C" fn on_shutdown_signal(_sig: c_int) {
    if let Some(cb) = SHUTDOWN_CB.get() {
        cb();
    }
}
