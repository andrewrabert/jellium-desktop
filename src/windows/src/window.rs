//! The mpv window's client rect and DPI, and the [`WindowSource`] that
//! publishes them.
//!
//! One `GetClientRect` + `GetDpiForWindow` sample is the whole window
//! geometry: CEF's render size, the persisted geometry, and the scale the
//! context menu and the OSR popup are placed with all read it.

use std::thread::JoinHandle;

use jfn_platform_abi::{
    PhysicalSize, Scale, WindowExtent, WindowPos, WindowSnapshot, WindowSource,
    notify_window_changed,
};
use parking_lot::{Condvar, Mutex};
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, IsZoomed};

static METRICS: Mutex<Option<WindowExtent>> = Mutex::new(None);

struct NotifyState {
    dirty: bool,
    stop: bool,
}

static NOTIFY: Mutex<NotifyState> = Mutex::new(NotifyState {
    dirty: false,
    stop: false,
});
static NOTIFY_WAKE: Condvar = Condvar::new();
static NOTIFIER: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// Start the notifier thread. Idempotent; runs until [`stop_notifier`].
pub(crate) fn start_notifier() {
    let mut slot = NOTIFIER.lock();
    if slot.is_some() {
        return;
    }
    {
        let mut st = NOTIFY.lock();
        st.dirty = false;
        st.stop = false;
    }
    *slot = Some(std::thread::spawn(|| {
        loop {
            {
                let mut st = NOTIFY.lock();
                while !st.dirty && !st.stop {
                    NOTIFY_WAKE.wait(&mut st);
                }
                if st.stop {
                    return;
                }
                st.dirty = false;
            }
            notify_window_changed();
        }
    }));
}

/// Stop and join the notifier thread. Pending dirtiness is dropped; the
/// process is tearing down.
pub(crate) fn stop_notifier() {
    let handle = NOTIFIER.lock().take();
    let Some(handle) = handle else {
        return;
    };
    NOTIFY.lock().stop = true;
    NOTIFY_WAKE.notify_one();
    let _ = handle.join();
}

/// Re-read the client rect and the window DPI and store them as the window's
/// metrics. Returns the client size just stored; `None` when there is no
/// window or its client rect is empty, which leaves the stored metrics
/// untouched.
pub(crate) fn sample() -> Option<PhysicalSize> {
    let hwnd = crate::platform::win_hwnd()?;
    let mut rc = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rc) }.ok()?;
    let client = PhysicalSize {
        w: rc.right - rc.left,
        h: rc.bottom - rc.top,
    };
    if client.w <= 0 || client.h <= 0 {
        return None;
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let scale = if dpi > 0 { dpi as f32 / 96.0 } else { 1.0 };
    *METRICS.lock() = Some(WindowExtent::new(client, Scale(scale)));
    Some(client)
}

/// [`sample`], then wake every window-changed consumer synchronously.
/// For init-time publishing on the app main thread; the WndProc hook uses
/// [`publish_deferred`].
pub(crate) fn republish() -> Option<PhysicalSize> {
    let client = sample()?;
    notify_window_changed();
    Some(client)
}

/// [`sample`], then hand the wakeup to the notifier thread.
pub(crate) fn publish_deferred() -> Option<PhysicalSize> {
    let client = sample()?;
    NOTIFY.lock().dirty = true;
    NOTIFY_WAKE.notify_one();
    Some(client)
}

/// Client size and scale as of the last sample.
///
/// Before the first sample exists — the boot wait polls the snapshot before
/// `win_init` runs — this seeds itself: it resolves mpv's HWND and samples
/// directly, without waking anyone, since returning the extent *is* the pull.
pub(crate) fn client_extent() -> Option<WindowExtent> {
    if let Some(extent) = *METRICS.lock() {
        return Some(extent);
    }
    crate::platform::win_ensure_hwnd()?;
    sample()?;
    *METRICS.lock()
}

/// Window DPI scale as of the last sample.
pub(crate) fn client_scale() -> Option<Scale> {
    client_extent().map(|e| e.scale())
}

/// Forget the stored metrics.
pub(crate) fn clear() {
    *METRICS.lock() = None;
}

pub(crate) struct WinWindowSource;

pub(crate) static WIN_WINDOW_SOURCE: WinWindowSource = WinWindowSource;

impl WindowSource for WinWindowSource {
    fn snapshot(&self) -> WindowSnapshot {
        let fullscreen = crate::platform::win_is_fullscreen();
        let maximized = !fullscreen
            && crate::platform::win_hwnd().is_some_and(|hwnd| unsafe { IsZoomed(hwnd) }.as_bool());
        let (mut x, mut y) = (0, 0);
        let position = crate::platform::win_query_window_position(&mut x, &mut y)
            .then_some(WindowPos { x, y });
        WindowSnapshot {
            extent: client_extent(),
            position,
            maximized,
            fullscreen,
        }
    }
}
