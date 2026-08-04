use std::ffi::c_int;

use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

pub struct SignalGuard {
    prev_int: Option<SigAction>,
    prev_term: Option<SigAction>,
}

impl Default for SignalGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalGuard {
    #[must_use]
    pub fn new() -> Self {
        Self {
            prev_int: snapshot(Signal::SIGINT),
            prev_term: snapshot(Signal::SIGTERM),
        }
    }

    /// # Safety
    /// `handler` must be async-signal-safe: it runs from inside a `sigaction`
    /// handler installed on SIGINT/SIGTERM.
    #[must_use]
    pub unsafe fn install(handler: extern "C" fn(c_int)) -> Self {
        let sa = SigAction::new(
            SigHandler::Handler(handler),
            SaFlags::empty(),
            SigSet::empty(),
        );
        Self {
            prev_int: unsafe { sigaction(Signal::SIGINT, &sa) }.ok(),
            prev_term: unsafe { sigaction(Signal::SIGTERM, &sa) }.ok(),
        }
    }
}

// Reads a disposition the only way sigaction offers: install SIG_IGN, then put
// the reported action straight back.
fn snapshot(signal: Signal) -> Option<SigAction> {
    let probe = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    let prev = unsafe { sigaction(signal, &probe) }.ok()?;
    let _ = unsafe { sigaction(signal, &prev) };
    Some(prev)
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
