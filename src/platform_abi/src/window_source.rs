//! Live window geometry, sourced from whichever component owns the window,
//! plus the payload-free change wakeup. Producers update their source and
//! call [`notify_window_changed`]; consumers subscribe and pull a
//! [`WindowSnapshot`].

use parking_lot::Mutex;

use crate::geometry::{WindowExtent, WindowPos};

#[derive(Clone, Copy)]
pub struct WindowSnapshot {
    pub extent: Option<WindowExtent>,
    pub position: Option<WindowPos>,
    pub maximized: bool,
    pub fullscreen: bool,
}

pub trait WindowSource: Send + Sync {
    fn snapshot(&self) -> WindowSnapshot;
}

static WINDOW_SUBSCRIBERS: Mutex<Vec<fn()>> = Mutex::new(Vec::new());

/// Registers `f`, run inline on the thread that publishes the change — on
/// Wayland the compositor's own dispatch loop. A listener posts its work
/// elsewhere and returns; nothing it calls may block on something that loop
/// delivers.
///
/// Subscribers must not depend on invocation order.
pub fn subscribe_window_changed(f: fn()) {
    WINDOW_SUBSCRIBERS.lock().push(f);
}

/// Wake every subscriber; each pulls the current snapshot itself. Callers
/// must have already committed the state a pull would read.
pub fn notify_window_changed() {
    let subs: Vec<fn()> = WINDOW_SUBSCRIBERS.lock().clone();
    for f in subs {
        f();
    }
}
