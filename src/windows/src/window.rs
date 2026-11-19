use std::thread::JoinHandle;

use jfn_platform_abi::{
    MpvCreatedWindow, PhysicalSize, Scale, WindowExtent, WindowPos, WindowSource, WindowState,
    WindowStateError, notify_window_changed,
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

use crate::scale::ClientExtentError;

static WINDOW_STATE: Mutex<Option<WindowState>> = Mutex::new(None);

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

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("the window-changed notifier thread panicked: {0}")]
pub(crate) struct NotifierPanicked(String);

/// Stop and join the notifier thread. Pending dirtiness is dropped; the
/// process is tearing down.
pub(crate) fn stop_notifier() -> Result<(), NotifierPanicked> {
    let handle = NOTIFIER.lock().take();
    let Some(handle) = handle else {
        return Ok(());
    };
    NOTIFY.lock().stop = true;
    NOTIFY_WAKE.notify_one();
    handle
        .join()
        .map_err(|payload| NotifierPanicked(panic_message(&*payload)))
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

pub(crate) fn sample() -> Result<Option<PhysicalSize>, WindowStateError> {
    let Some(hwnd) = crate::platform::win_ensure_hwnd() else {
        return Ok(None);
    };
    let mut rc = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rc) }
        .map_err(|e| ClientExtentError::ClientRect(e.code().0))?;
    let client = PhysicalSize {
        w: rc.right - rc.left,
        h: rc.bottom - rc.top,
    };
    if client.w <= 0 || client.h <= 0 {
        return Ok(None);
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let extent = crate::scale::extent(client, dpi)?;
    let fullscreen = crate::platform::win_is_fullscreen();
    let state = WindowState {
        extent,
        position: Some(window_position(hwnd)?),
        maximized: !fullscreen && unsafe { IsZoomed(hwnd) }.as_bool(),
        fullscreen,
        web_top: 0,
        basis: jfn_platform_abi::WindowBasis::FIXED,
    };
    let previous = WINDOW_STATE.lock().replace(state);
    if previous.map(|p| p.extent) != Some(extent) {
        log_sample(hwnd, dpi, extent);
    }
    Ok(Some(client))
}

/// Window position relative to the monitor's working area (excludes the
/// taskbar), in physical pixels. Matches mpv's `--geometry +X+Y` coordinate
/// system on Windows (`vo_calc_window_geometry` uses the working area).
fn window_position(hwnd: HWND) -> Result<WindowPos, WindowStateError> {
    let mut wr = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut wr) }
        .map_err(|e| WindowStateError::WindowRect(e.code().0))?;
    let mon: HMONITOR = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(mon, &mut mi) }
        .ok()
        .map_err(|e| WindowStateError::MonitorInfo(e.code().0))?;
    Ok(WindowPos {
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

pub(crate) fn republish() -> Result<Option<PhysicalSize>, WindowStateError> {
    let Some(client) = sample()? else {
        return Ok(None);
    };
    notify_window_changed();
    Ok(Some(client))
}

pub(crate) fn publish_deferred() -> Result<Option<PhysicalSize>, WindowStateError> {
    let Some(client) = sample()? else {
        return Ok(None);
    };
    NOTIFY.lock().dirty = true;
    NOTIFY_WAKE.notify_one();
    Ok(Some(client))
}

pub(crate) fn stored_window_state() -> Result<WindowState, WindowStateError> {
    if let Some(state) = *WINDOW_STATE.lock() {
        return Ok(state);
    }
    sample()?;
    let stored = *WINDOW_STATE.lock();
    stored.ok_or(WindowStateError::NoWindow)
}

pub(crate) fn existing_window_state() -> Result<Option<WindowState>, WindowStateError> {
    match stored_window_state() {
        Ok(state) => Ok(Some(state)),
        Err(WindowStateError::NoWindow | WindowStateError::WebOffsetUnstated) => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) fn client_extent() -> Result<Option<WindowExtent>, WindowStateError> {
    Ok(existing_window_state()?.map(|state| state.extent))
}

pub(crate) fn client_scale() -> Result<Option<Scale>, WindowStateError> {
    Ok(client_extent()?.as_ref().map(WindowExtent::scale))
}

pub(crate) fn clear() {
    *WINDOW_STATE.lock() = None;
}

pub(crate) struct WinWindowSource;

pub(crate) static WIN_WINDOW_SOURCE: WinWindowSource = WinWindowSource;

impl WindowSource for WinWindowSource {
    fn window_state(&self) -> Result<WindowState, WindowStateError> {
        stored_window_state()
    }
}

impl MpvCreatedWindow for WinWindowSource {}
