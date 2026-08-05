//! Windows platform impl: window init/cleanup, fullscreen toggle helpers,
//! scale + geometry queries, and the WndProc hook that resamples the window.
//!
//! All `g_win` state (HWND, the minimize edge, the maximize-restore flag, the
//! WndProc hook handle, the input thread JoinHandle) lives in this module
//! behind a `Mutex<WinState>`. Scale and window mode are not stored: they come
//! from Win32, through `crate::window`.

#![allow(non_snake_case)]

use parking_lot::Mutex;
use std::ffi::{c_int, c_void};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    CWPRETSTRUCT, CallNextHookEx, GWL_STYLE, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, HHOOK, IsZoomed, SIZE_MINIMIZED, SPI_GETWORKAREA,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowsHookExW, SystemParametersInfoW,
    UnhookWindowsHookEx, WH_CALLWNDPROCRET, WM_CLOSE, WM_DPICHANGED, WM_SIZE, WM_STYLECHANGED,
    WS_CAPTION, WS_THICKFRAME,
};

use jfn_mpv::api::{
    jfn_mpv_get_property_int, jfn_mpv_set_fullscreen, jfn_mpv_set_window_maximized,
    jfn_mpv_set_window_minimized, jfn_mpv_toggle_fullscreen,
};
use jfn_mpv::boot::jfn_mpv_handle_get;
use jfn_platform_abi::geometry::{Bounds, WindowGeometry, clamp_to_bounds};
use jfn_playback::shutdown::jfn_shutdown_initiate;

// Input thread lives in `crate::input`.
use crate::input::{
    jfn_input_windows_resize_to_parent, jfn_input_windows_run_input_thread,
    jfn_input_windows_stop_input_thread,
};

// =====================================================================
// File-static state — equivalent of the C++ `static WinState g_win`.
// All access is serialized through `STATE.lock()`. HWND / HHOOK are raw
// pointer types; we are the sole writer so the Mutex is sufficient.
// =====================================================================

struct WinState {
    mpv_hwnd_raw: usize,
    was_minimized: bool,
    restore_maximized_on_unfullscreen: bool,
    wndproc_hook_raw: usize,
    input_thread: Option<JoinHandle<()>>,
}

impl WinState {
    const fn new() -> Self {
        Self {
            mpv_hwnd_raw: 0,
            was_minimized: false,
            restore_maximized_on_unfullscreen: false,
            wndproc_hook_raw: 0,
            input_thread: None,
        }
    }
}

static STATE: Mutex<WinState> = Mutex::new(WinState::new());

fn hwnd_from_raw(raw: usize) -> HWND {
    HWND(raw as *mut c_void)
}

// =====================================================================
// Narrow accessors.
// =====================================================================

/// mpv's HWND, or `None` before it has been resolved / after cleanup.
pub(crate) fn win_hwnd() -> Option<HWND> {
    let raw = STATE.lock().mpv_hwnd_raw;
    (raw != 0).then(|| hwnd_from_raw(raw))
}

/// The stored HWND, resolved from mpv's `window-id` on first use. The boot
/// wait polls the window snapshot before `win_init` runs, so resolution
/// cannot wait for init. `None` until mpv has a window.
pub(crate) fn win_ensure_hwnd() -> Option<HWND> {
    if let Some(hwnd) = win_hwnd() {
        return Some(hwnd);
    }
    if jfn_mpv_handle_get().is_null() {
        return None;
    }
    let mut wid: i64 = 0;
    let rc = unsafe { jfn_mpv_get_property_int(c"window-id".as_ptr(), &mut wid) };
    if rc < 0 || wid == 0 {
        return None;
    }
    // Two racing resolvers store the same value; the second write is benign.
    STATE.lock().mpv_hwnd_raw = wid as usize;
    Some(hwnd_from_raw(wid as usize))
}

/// True when mpv's window has neither `WS_CAPTION` nor `WS_THICKFRAME`.
///
/// Exact for every style mpv sets: `update_style` in
/// `third_party/mpv/video/out/w32_common.c` keeps `WS_THICKFRAME` in its
/// borderless-windowed set (NO_FRAME) and clears it only for fullscreen, and
/// mpv owns a top-level window here (no `--wid`), so the early-out for
/// embedded windows never applies.
pub(crate) fn win_is_fullscreen() -> bool {
    let Some(hwnd) = win_hwnd() else {
        return false;
    };
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    (style & WS_CAPTION.0) == 0 && (style & WS_THICKFRAME.0) == 0
}

// =====================================================================
// Scale lookups.
// =====================================================================

/// The window's own DPI once it exists, the system DPI before it does.
pub(crate) fn win_get_scale() -> f32 {
    match crate::window::client_scale() {
        Some(scale) => scale.or_one().0,
        None => system_scale(),
    }
}

// Called for boot geometry, before any window exists, so there is no HWND to
// ask; the system DPI is the only answer available and (x, y) is ignored.
pub(crate) fn win_get_display_scale(_x: c_int, _y: c_int) -> f32 {
    system_scale()
}

fn system_scale() -> f32 {
    let dpi = unsafe { GetDpiForSystem() };
    if dpi > 0 { dpi as f32 / 96.0 } else { 1.0 }
}

// =====================================================================
// Fullscreen toggle helpers. mpv owns the window and executes every mode
// change; the window's own style says which mode it is in.
// =====================================================================

pub(crate) fn win_set_fullscreen(fullscreen: bool) {
    if jfn_mpv_handle_get().is_null() || win_is_fullscreen() == fullscreen {
        return;
    }
    let Some(hwnd) = win_hwnd() else {
        return;
    };

    if fullscreen {
        STATE.lock().restore_maximized_on_unfullscreen = unsafe { IsZoomed(hwnd) }.as_bool();
        jfn_mpv_set_window_minimized(false);
        jfn_mpv_set_fullscreen(true);
        return;
    }

    let should_restore_maximized =
        std::mem::take(&mut STATE.lock().restore_maximized_on_unfullscreen);
    jfn_mpv_set_fullscreen(false);
    if should_restore_maximized {
        jfn_mpv_set_window_maximized(true);
    }
}

pub(crate) fn win_toggle_fullscreen() {
    if jfn_mpv_handle_get().is_null() {
        return;
    }
    let Some(hwnd) = win_hwnd() else {
        return;
    };

    if !win_is_fullscreen() {
        STATE.lock().restore_maximized_on_unfullscreen = unsafe { IsZoomed(hwnd) }.as_bool();
        jfn_mpv_set_window_minimized(false);
        jfn_mpv_toggle_fullscreen();
        return;
    }

    let should_restore_maximized =
        std::mem::take(&mut STATE.lock().restore_maximized_on_unfullscreen);
    jfn_mpv_toggle_fullscreen();
    if should_restore_maximized {
        jfn_mpv_set_window_maximized(true);
    }
}

// =====================================================================
// WndProc hook. Republish-only: it resamples the window and wakes the
// window-changed consumers, and enters nothing but `crate::window`.
// =====================================================================

unsafe extern "system" fn mpv_wndproc_hook(n_code: c_int, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if n_code >= 0 {
        let msg = unsafe { &*(lp.0 as *const CWPRETSTRUCT) };
        let target_hwnd_raw = STATE.lock().mpv_hwnd_raw;
        if (msg.hwnd.0 as usize) == target_hwnd_raw {
            match msg.message {
                WM_SIZE if msg.wParam.0 == SIZE_MINIMIZED as usize => {
                    if !std::mem::replace(&mut STATE.lock().was_minimized, true) {
                        jfn_playback::lifecycle::jfn_lifecycle_set_visible(false);
                    }
                }
                WM_SIZE => {
                    let restored = std::mem::replace(&mut STATE.lock().was_minimized, false);
                    if let Some(client) = crate::window::publish_deferred() {
                        jfn_input_windows_resize_to_parent(client.w, client.h);
                    }
                    if restored {
                        jfn_playback::lifecycle::jfn_lifecycle_set_visible(true);
                    }
                }
                // A DPI change reaches the window as its own message, and a
                // fullscreen edge that does not move the client rect reaches
                // it as a style change; neither is guaranteed to be followed
                // by WM_SIZE.
                WM_DPICHANGED | WM_STYLECHANGED => {
                    crate::window::publish_deferred();
                }
                WM_CLOSE => jfn_shutdown_initiate(),
                _ => {}
            }
        }
    }
    let hook_raw = STATE.lock().wndproc_hook_raw;
    let hook = HHOOK(hook_raw as *mut c_void);
    unsafe { CallNextHookEx(Some(hook), n_code, wp, lp) }
}

// =====================================================================
// Platform vtable entry points.
// =====================================================================

pub(crate) fn win_early_init() {
    // Nothing needed on Windows before mpv starts.
}

pub(crate) fn win_init(_mpv: *mut c_void) -> bool {
    let Some(hwnd) = win_ensure_hwnd() else {
        tracing::error!("Failed to get window-id from mpv");
        return false;
    };
    let hwnd_raw = hwnd.0 as usize;
    crate::window::republish();

    // Enable DWM transparency so DComp visuals with premultiplied alpha work.
    let margins = MARGINS {
        cxLeftWidth: -1,
        cxRightWidth: -1,
        cyTopHeight: -1,
        cyBottomHeight: -1,
    };
    unsafe {
        let _ = DwmExtendFrameIntoClientArea(hwnd_from_raw(hwnd_raw), &margins);
    }

    if !crate::render::init(hwnd_from_raw(hwnd_raw)) {
        return false;
    }

    crate::window::start_notifier();
    let mpv_tid = unsafe { GetWindowThreadProcessId(hwnd_from_raw(hwnd_raw), None) };
    let hook =
        unsafe { SetWindowsHookExW(WH_CALLWNDPROCRET, Some(mpv_wndproc_hook), None, mpv_tid) };
    match hook {
        Ok(h) => STATE.lock().wndproc_hook_raw = h.0 as usize,
        Err(e) => {
            tracing::error!("SetWindowsHookExW(WH_CALLWNDPROCRET) failed: {e:?}");
            return false;
        }
    }

    let mpv_hwnd_for_thread = hwnd_raw;
    let join = std::thread::spawn(move || {
        jfn_input_windows_run_input_thread(mpv_hwnd_for_thread as *mut c_void);
    });
    STATE.lock().input_thread = Some(join);

    // Post-hook: from here every change republishes itself.
    crate::window::republish();
    tracing::info!("Windows DirectComposition compositor initialized");
    true
}

pub(crate) fn win_cleanup() {
    jfn_input_windows_stop_input_thread();
    let join = STATE.lock().input_thread.take();
    if let Some(j) = join {
        let _ = j.join();
    }
    let hook_raw = STATE.lock().wndproc_hook_raw;
    if hook_raw != 0 {
        let hook = HHOOK(hook_raw as *mut c_void);
        unsafe {
            let _ = UnhookWindowsHookEx(hook);
        }
        STATE.lock().wndproc_hook_raw = 0;
    }
    crate::window::stop_notifier();

    crate::render::cleanup();
    crate::window::clear();
    STATE.lock().mpv_hwnd_raw = 0;
}

// =====================================================================
// Window-position / geometry helpers.
// =====================================================================

/// Query window position relative to the monitor's working area (excludes
/// taskbar), in physical pixels. Matches mpv's `--geometry +X+Y`
/// coordinate system on Windows (`vo_calc_window_geometry` uses the
/// working area).
pub(crate) fn win_query_window_position(x: &mut c_int, y: &mut c_int) -> bool {
    let Some(hwnd) = win_hwnd() else {
        return false;
    };
    let mut wr = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut wr) }.is_err() {
        return false;
    }
    let mon: HMONITOR = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(mon, &mut mi) }.as_bool() {
        return false;
    }
    *x = wr.left - mi.rcWork.left;
    *y = wr.top - mi.rcWork.top;
    true
}

/// Resolve saved geometry against the primary monitor's working area so the
/// window never opens larger than the screen or off-screen, and center any
/// unset axis.
pub(crate) fn win_clamp_window_geometry(
    w: &mut c_int,
    h: &mut c_int,
    x: &mut c_int,
    y: &mut c_int,
) {
    let mut work = RECT::default();
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut RECT as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() {
        return;
    }
    let vw = work.right - work.left;
    let vh = work.bottom - work.top;
    let mut g = WindowGeometry::from_raw(*w, *h, *x, *y);
    clamp_to_bounds(&mut g, Bounds { w: vw, h: vh });
    *w = g.w;
    *h = g.h;
    let (nx, ny) = g.raw_position();
    *x = nx;
    *y = ny;
}
