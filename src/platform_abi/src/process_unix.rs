use std::ffi::c_int;
use std::sync::OnceLock;

use crate::SignalGuard;

static GUARD: OnceLock<SignalGuard> = OnceLock::new();
static SHUTDOWN_CB: OnceLock<fn()> = OnceLock::new();

pub fn install_shutdown(on_shutdown: fn()) {
    let _ = SHUTDOWN_CB.set(on_shutdown);
    let g = unsafe { SignalGuard::install(on_shutdown_signal) };
    let _ = GUARD.set(g);
}

extern "C" fn on_shutdown_signal(_sig: c_int) {
    if let Some(cb) = SHUTDOWN_CB.get() {
        cb();
    }
}
