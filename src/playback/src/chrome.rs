//! Page-derived chrome inputs.
//!
//! Held below both jfn-cef, which writes them from jellyfin-web's bindings,
//! and jfn-shell, which draws the titlebar from them.

use parking_lot::Mutex;

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct ChromeState {
    pub video_active: bool,
    pub osd_visible: bool,
}

type Listener = Box<dyn Fn() + Send + Sync>;

static STATE: Mutex<ChromeState> = Mutex::new(ChromeState {
    video_active: false,
    osd_visible: false,
});
static LISTENERS: Mutex<Vec<Listener>> = Mutex::new(Vec::new());

pub fn set_video_active(active: bool) {
    update(|s| s.video_active = active);
}

pub fn set_osd_visible(visible: bool) {
    update(|s| s.osd_visible = visible);
}

pub fn chrome_state() -> ChromeState {
    *STATE.lock()
}

/// Registered once at boot; fired on every change.
pub fn subscribe_chrome<F: Fn() + Send + Sync + 'static>(f: F) {
    LISTENERS.lock().push(Box::new(f));
}

fn update(f: impl FnOnce(&mut ChromeState)) {
    let changed = {
        let mut state = STATE.lock();
        let before = *state;
        f(&mut state);
        *state != before
    };
    if changed {
        for l in LISTENERS.lock().iter() {
            l();
        }
    }
}
