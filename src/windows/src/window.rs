//! The mpv window's client rect, DPI, position, and mode, and the
//! [`WindowSource`] that publishes them.
//!
//! One sample pass is the whole window state: CEF's render size, the
//! persisted geometry and position, the scale the context menu and the OSR
//! popup are placed with, and the mode playback reconciles against all read
//! the stored snapshot. No pull issues an mpv call, and a pull with a stored
//! snapshot issues no Win32 query either.

use std::thread::JoinHandle;

use jfn_platform_abi::{
    MpvCreatedWindow, PhysicalSize, Scale, WindowExtent, WindowPos, WindowSnapshot, WindowSource,
    notify_window_changed,
};
use parking_lot::{Condvar, Mutex};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{
    GetAwarenessFromDpiAwarenessContext, GetDpiForWindow, GetThreadDpiAwarenessContext,
    GetWindowDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect, IsZoomed};

static SNAPSHOT: Mutex<Option<WindowSnapshot>> = Mutex::new(None);

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

/// Re-read mpv's window in one pass — client rect, DPI, position, maximized,
/// fullscreen — and store it as the window's snapshot. Returns the client
/// size just stored; `None` when there is no window or its client rect is
/// empty, which leaves the stored snapshot untouched.
pub(crate) fn sample() -> Option<PhysicalSize> {
    let hwnd = crate::platform::win_ensure_hwnd()?;
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
    let extent = crate::scale::extent(client, dpi)?;
    let fullscreen = crate::platform::win_is_fullscreen();
    let snap = WindowSnapshot {
        extent: Some(extent),
        position: window_position(hwnd),
        maximized: !fullscreen && unsafe { IsZoomed(hwnd) }.as_bool(),
        fullscreen,
    };
    let previous = SNAPSHOT.lock().replace(snap);
    if previous.and_then(|p| p.extent) != Some(extent) {
        log_sample(hwnd, dpi, extent);
    }
    Some(client)
}

/// Window position relative to the monitor's working area (excludes the
/// taskbar), in physical pixels. Matches mpv's `--geometry +X+Y` coordinate
/// system on Windows (`vo_calc_window_geometry` uses the working area).
fn window_position(hwnd: HWND) -> Option<WindowPos> {
    let mut wr = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut wr) }.ok()?;
    let mon: HMONITOR = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(mon, &mut mi) }.as_bool() {
        return None;
    }
    Some(WindowPos {
        x: wr.left - mi.rcWork.left,
        y: wr.top - mi.rcWork.top,
    })
}

/// The client size, the client origin in screen coordinates, the window rect,
/// the DPI the extent was built from, and the DPI awareness of both the window
/// and this thread.
fn log_sample(hwnd: HWND, dpi: u32, extent: WindowExtent) {
    let physical = extent.physical();
    let logical = extent.logical();
    let mut origin = POINT::default();
    let _ = unsafe { ClientToScreen(hwnd, &mut origin) };
    let mut wr = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut wr) };
    let window_awareness =
        unsafe { GetAwarenessFromDpiAwarenessContext(GetWindowDpiAwarenessContext(hwnd)) }.0;
    let thread_awareness =
        unsafe { GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()) }.0;
    tracing::debug!(
        target: "platform",
        "window sample: client={}x{} client_origin=({},{}) window=({},{},{},{}) \
         dpi={} logical={}x{} window_awareness={} thread_awareness={}",
        physical.w, physical.h, origin.x, origin.y,
        wr.left, wr.top, wr.right, wr.bottom,
        dpi, logical.w, logical.h, window_awareness, thread_awareness,
    );
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

/// The stored snapshot, seeded by one [`sample`] when nothing is stored yet —
/// the boot wait pulls it before `win_init` runs.
pub(crate) fn snapshot() -> Option<WindowSnapshot> {
    if let Some(snap) = *SNAPSHOT.lock() {
        return Some(snap);
    }
    sample()?;
    *SNAPSHOT.lock()
}

/// Client size and scale as of the last sample.
pub(crate) fn client_extent() -> Option<WindowExtent> {
    snapshot()?.extent
}

/// The scale the client window's DPI named at the last sample, or `None`
/// before the window exists.
///
/// One sample names the extent and the scale together, so a menu anchor and
/// the extent the pointer maps through can never come from two DPIs.
pub(crate) fn client_scale() -> Option<Scale> {
    Some(client_extent()?.scale())
}

/// Forget the stored snapshot.
pub(crate) fn clear() {
    *SNAPSHOT.lock() = None;
}

pub(crate) struct WinWindowSource;

pub(crate) static WIN_WINDOW_SOURCE: WinWindowSource = WinWindowSource;

impl WindowSource for WinWindowSource {
    fn snapshot(&self) -> WindowSnapshot {
        crate::window::snapshot().unwrap_or(WindowSnapshot {
            extent: None,
            position: None,
            maximized: false,
            fullscreen: false,
        })
    }
}

impl MpvCreatedWindow for WinWindowSource {}
