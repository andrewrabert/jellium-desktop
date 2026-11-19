use std::thread::JoinHandle;

use jfn_platform_abi::{
    MpvCreatedWindow, PhysicalSize, Scale, WindowExtent, WindowPlacement, WindowPos, WindowSource,
    WindowState, WindowStateError, notify_window_changed,
};
use parking_lot::{Condvar, Mutex};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{ClientToScreen, MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::Win32::UI::HiDpi::{
    GetAwarenessFromDpiAwarenessContext, GetDpiForWindow, GetThreadDpiAwarenessContext,
    GetWindowDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect, IsZoomed};

use crate::monitor::Monitor;
use crate::scale::ClientExtentError;

struct Sample {
    state: WindowState,
    placement: WindowPlacement,
}

enum Stored {
    Unsampled,
    Sampled(Sample),
}

static WINDOW_STATE: Mutex<Stored> = Mutex::new(Stored::Unsampled);

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the window-changed notifier thread panicked")]
pub(crate) struct NotifierPanicked;

/// Stop and join the notifier thread. Pending dirtiness is dropped; the
/// process is tearing down.
pub(crate) fn stop_notifier() -> Result<(), NotifierPanicked> {
    let handle = NOTIFIER.lock().take();
    let Some(handle) = handle else {
        return Ok(());
    };
    NOTIFY.lock().stop = true;
    NOTIFY_WAKE.notify_one();
    handle.join().map_err(|_| NotifierPanicked)
}

pub(crate) fn sample() -> Result<Option<PhysicalSize>, WindowStateError> {
    let Some(hwnd) = crate::platform::win_ensure_hwnd() else {
        return Ok(None);
    };
    let mut rc = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rc) }.map_err(|_| ClientExtentError::ClientRect)?;
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
    let placement = window_placement(hwnd)?;
    let state = WindowState {
        extent,
        position: Some(placement.position()),
        maximized: !fullscreen && unsafe { IsZoomed(hwnd) }.as_bool(),
        fullscreen,
        web_top: 0,
        basis: jfn_platform_abi::WindowBasis::FIXED,
    };
    let previous = std::mem::replace(
        &mut *WINDOW_STATE.lock(),
        Stored::Sampled(Sample { state, placement }),
    );
    if !matches!(previous, Stored::Sampled(p) if p.state.extent == extent) {
        log_sample(hwnd, dpi, extent);
    }
    Ok(Some(client))
}

/// Window position relative to the monitor's working area (excludes the
/// taskbar), in physical pixels. Matches mpv's `--geometry +X+Y` coordinate
/// system on Windows (`vo_calc_window_geometry` uses the working area).
fn window_placement(hwnd: HWND) -> Result<WindowPlacement, WindowStateError> {
    let mut wr = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut wr) }.map_err(|_| WindowStateError::WindowRect)?;
    let monitor = Monitor::of(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) })
        .map_err(|_| WindowStateError::MonitorInfo)?;
    let work = monitor.work_area();
    Ok(WindowPlacement::new(
        monitor.name(),
        WindowPos {
            x: wr.left - work.left,
            y: wr.top - work.top,
        },
    ))
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
    read_stored(|stored| stored.state)
}

pub(crate) fn stored_window_placement() -> Result<WindowPlacement, WindowStateError> {
    read_stored(|stored| stored.placement.clone())
}

fn read_stored<T>(read: impl Fn(&Sample) -> T) -> Result<T, WindowStateError> {
    if let Stored::Sampled(stored) = &*WINDOW_STATE.lock() {
        return Ok(read(stored));
    }
    sample()?;
    match &*WINDOW_STATE.lock() {
        Stored::Sampled(stored) => Ok(read(stored)),
        Stored::Unsampled => Err(WindowStateError::NoWindow),
    }
}

pub(crate) fn client_extent() -> Result<WindowExtent, WindowStateError> {
    Ok(stored_window_state()?.extent)
}

pub(crate) fn client_scale() -> Result<Scale, WindowStateError> {
    Ok(client_extent()?.scale())
}

pub(crate) fn clear() {
    *WINDOW_STATE.lock() = Stored::Unsampled;
}

pub(crate) struct WinWindowSource;

pub(crate) static WIN_WINDOW_SOURCE: WinWindowSource = WinWindowSource;

impl WindowSource for WinWindowSource {
    fn window_state(&self) -> Result<WindowState, WindowStateError> {
        stored_window_state()
    }

    fn window_placement(&self) -> Result<Option<WindowPlacement>, WindowStateError> {
        Ok(Some(stored_window_placement()?))
    }
}

impl MpvCreatedWindow for WinWindowSource {}
