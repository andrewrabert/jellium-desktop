//! Windows platform impl: window init/cleanup, fullscreen toggle helpers,
//! scale + geometry queries, and the WndProc hook that resamples the window.
//!
//! All `g_win` state (HWND, the minimize edge, the maximize-restore flag, the
//! WndProc hook handle, the input thread JoinHandle) lives in this module
//! behind a `Mutex<WinState>`. Scale, position, and window mode are not stored
//! here: they come from Win32, through `crate::window`'s sample.

#![allow(non_snake_case)]

use parking_lot::Mutex;
use std::ffi::{c_int, c_void};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    CWPRETSTRUCT, CallNextHookEx, GWL_STYLE, GetWindowLongPtrW, GetWindowThreadProcessId, HHOOK,
    IsZoomed, SIZE_MINIMIZED, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SetWindowsHookExW, SystemParametersInfoW, UnhookWindowsHookEx, WH_CALLWNDPROCRET, WM_CLOSE,
    WM_DPICHANGED, WM_MOVE, WM_SIZE, WM_STYLECHANGED, WS_CAPTION, WS_THICKFRAME,
};

use jfn_mpv::api::{
    jfn_mpv_set_fullscreen, jfn_mpv_set_window_maximized, jfn_mpv_set_window_minimized,
    jfn_mpv_toggle_fullscreen,
};
use jfn_mpv::boot::jfn_mpv_handle_get;
use jfn_platform_abi::WindowStateError;
use jfn_platform_abi::geometry::{Bounds, ClampError, WindowGeometry, clamp_to_bounds};
use jfn_playback::shutdown::jfn_shutdown_initiate;

use crate::input::{
    jfn_input_windows_resize_to_parent, jfn_input_windows_run_input_thread,
    jfn_input_windows_stop_input_thread,
};

struct WinState {
    mpv_hwnd_raw: usize,
    was_minimized: bool,
    restore_maximized_on_unfullscreen: bool,
    wndproc_hook_raw: usize,
    input_thread: Option<JoinHandle<()>>,
    initialized: bool,
}

impl WinState {
    const fn new() -> Self {
        Self {
            mpv_hwnd_raw: 0,
            was_minimized: false,
            restore_maximized_on_unfullscreen: false,
            wndproc_hook_raw: 0,
            input_thread: None,
            initialized: false,
        }
    }
}

static STATE: Mutex<WinState> = Mutex::new(WinState::new());

fn hwnd_from_raw(raw: usize) -> HWND {
    HWND(raw as *mut c_void)
}

/// mpv's HWND, or `None` before it has been resolved / after cleanup.
pub(crate) fn win_hwnd() -> Option<HWND> {
    let raw = STATE.lock().mpv_hwnd_raw;
    (raw != 0).then(|| hwnd_from_raw(raw))
}

pub(crate) fn win_ensure_hwnd() -> Option<HWND> {
    if let Some(hwnd) = win_hwnd() {
        return Some(hwnd);
    }
    let raw = jfn_playback::ingest_driver::jfn_playback_window_id()? as usize;
    if raw == 0 {
        return None;
    }
    STATE.lock().mpv_hwnd_raw = raw;
    Some(hwnd_from_raw(raw))
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

/// `GetDpiForWindow`/96 once the window exists, `GetDpiForSystem`/96 before
/// it does.
pub(crate) fn win_get_scale() -> Result<jfn_platform_abi::Scale, WindowStateError> {
    match crate::window::client_scale() {
        Err(WindowStateError::NoWindow) => Ok(win_display_scale()),
        sampled => sampled,
    }
}

pub(crate) fn win_display_scale() -> jfn_platform_abi::Scale {
    crate::scale::report_dpi("GetDpiForSystem", unsafe { GetDpiForSystem() })
}

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

unsafe extern "system" fn mpv_wndproc_hook(n_code: c_int, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if n_code >= 0 {
        let msg = unsafe { &*(lp.0 as *const CWPRETSTRUCT) };
        let target_hwnd_raw = STATE.lock().mpv_hwnd_raw;
        if (msg.hwnd.0 as usize) == target_hwnd_raw {
            let sampled = match msg.message {
                WM_SIZE if msg.wParam.0 == SIZE_MINIMIZED as usize => {
                    if !std::mem::replace(&mut STATE.lock().was_minimized, true) {
                        jfn_playback::lifecycle::jfn_lifecycle_set_visible(false);
                    }
                    Ok(())
                }
                WM_SIZE => {
                    let restored = std::mem::replace(&mut STATE.lock().was_minimized, false);
                    let published = crate::window::publish_deferred();
                    if let Ok(Some(client)) = published {
                        jfn_input_windows_resize_to_parent(client.w, client.h);
                    }
                    if restored {
                        jfn_playback::lifecycle::jfn_lifecycle_set_visible(true);
                    }
                    published.map(|_| ())
                }
                WM_MOVE => crate::window::sample().map(|_| ()),
                WM_DPICHANGED | WM_STYLECHANGED => crate::window::publish_deferred().map(|_| ()),
                WM_CLOSE => {
                    jfn_shutdown_initiate();
                    Ok(())
                }
                _ => Ok(()),
            };
            if let Err(e) = sampled {
                tracing::error!(
                    target: "platform",
                    "window sample: {e}; the stored window state stands"
                );
            }
        }
    }
    let hook_raw = STATE.lock().wndproc_hook_raw;
    let hook = HHOOK(hook_raw as *mut c_void);
    unsafe { CallNextHookEx(Some(hook), n_code, wp, lp) }
}

pub(crate) fn win_early_init() {}

/// Poll interval for [`wait_for_hwnd`]'s bounded retry.
const HWND_POLL_INTERVAL: Duration = Duration::from_millis(15);

/// Blocks up to 5s for mpv to create its window and publish its window-id,
/// by retrying a sync libmpv property read — safe here specifically because
/// this runs as ordinary sequential boot code, not from inside an mpv event
/// callback (the deadlock risk the sync-API rule in AGENTS.md is about is
/// reentrancy from a callback, which this isn't).
///
/// Deliberately never touches `jfn_mpv::api::wait_event_owned`/mpv's shared
/// event queue, unlike an earlier version of this function: mpv's event
/// queue is a single destructive-read FIFO, and the boot-critical captured-
/// background-color handshake (`BACKGROUND_COLOR_REPLY` /
/// `STARTUP_BACKGROUND_SET_REPLY` in `app.rs`'s `consume_boot_event`) can
/// land on it at any point during boot. Forwarding events pulled here to
/// generic ingest — as the earlier version did — would silently swallow
/// either of those two instead of `wait_for_vo_window` seeing them, which
/// leaves `BackgroundCapture` stuck below `Applied` forever: `boot_progress`
/// requires it before anything else, so the whole boot hangs permanently,
/// not merely falls back to a default background color. A short sleep-poll
/// is the one safe option against a shared, destructively-read queue.
fn wait_for_hwnd() -> Option<HWND> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let Ok(window_id_prop) = std::ffi::CString::new("window-id") else {
        return None;
    };
    loop {
        if let Some(hwnd) = win_ensure_hwnd() {
            return Some(hwnd);
        }
        let mut raw: i64 = 0;
        let rc =
            unsafe { jfn_mpv::api::jfn_mpv_get_property_int(window_id_prop.as_ptr(), &mut raw) };
        if rc == 0 && raw != 0 {
            STATE.lock().mpv_hwnd_raw = raw as usize;
            return Some(hwnd_from_raw(raw as usize));
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(HWND_POLL_INTERVAL);
    }
}

pub(crate) fn win_init(_mpv: *mut c_void) -> Result<(), jfn_platform_abi::PlatformInitError> {
    let hwnd = match win_ensure_hwnd().or_else(wait_for_hwnd) {
        Some(hwnd) => hwnd,
        None => {
            return Err(jfn_platform_abi::PlatformInitError::backend(
                "Windows window acquisition",
                "no observed mpv window-id",
            ));
        }
    };
    let hwnd_raw = hwnd.0 as usize;
    crate::window::republish()
        .map_err(|e| jfn_platform_abi::PlatformInitError::backend("Windows window sample", e))?;

    crate::render::init(hwnd_from_raw(hwnd_raw))?;

    crate::window::start_notifier();
    let mpv_tid = unsafe { GetWindowThreadProcessId(hwnd_from_raw(hwnd_raw), None) };
    let hook =
        unsafe { SetWindowsHookExW(WH_CALLWNDPROCRET, Some(mpv_wndproc_hook), None, mpv_tid) };
    match hook {
        Ok(h) => STATE.lock().wndproc_hook_raw = h.0 as usize,
        Err(e) => {
            return Err(jfn_platform_abi::PlatformInitError::backend(
                "SetWindowsHookExW(WH_CALLWNDPROCRET)",
                e,
            ));
        }
    }

    let mpv_hwnd_for_thread = hwnd_raw;
    let join = std::thread::Builder::new()
        .name("jfn-input".into())
        .spawn(move || {
            jfn_input_windows_run_input_thread(mpv_hwnd_for_thread as *mut c_void);
        })
        .map_err(|e| {
            jfn_platform_abi::PlatformInitError::backend("Windows input thread spawn", e)
        })?;
    STATE.lock().input_thread = Some(join);

    crate::window::republish()
        .map_err(|e| jfn_platform_abi::PlatformInitError::backend("Windows window sample", e))?;
    STATE.lock().initialized = true;
    tracing::info!("Windows DirectComposition compositor initialized");
    Ok(())
}

/// mpv replaced its VO window (a `d3d11-flip` change recreates it). Called on
/// the mpv ingest worker, never from a window-message callback. Keeps the
/// composition tree and surfaces alive and rebinds everything attached to the
/// old HWND: the WndProc hook, the composition target, the input child window
/// and the media session.
pub(crate) fn win_on_window_handle_changed(id: i64) {
    if id <= 0 || jfn_playback::shutdown::jfn_shutting_down() {
        return;
    }
    let raw = id as usize;
    let old_hook = {
        let mut state = STATE.lock();
        if !state.initialized || state.mpv_hwnd_raw == raw {
            return;
        }
        tracing::info!(
            "mpv VO window replaced: {:#x} -> {raw:#x}",
            state.mpv_hwnd_raw
        );
        state.mpv_hwnd_raw = raw;
        std::mem::take(&mut state.wndproc_hook_raw)
    };
    let hwnd = hwnd_from_raw(raw);
    if old_hook != 0 {
        let _ = unsafe { UnhookWindowsHookEx(HHOOK(old_hook as *mut c_void)) };
    }
    let tid = unsafe { GetWindowThreadProcessId(hwnd, None) };
    match unsafe { SetWindowsHookExW(WH_CALLWNDPROCRET, Some(mpv_wndproc_hook), None, tid) } {
        Ok(hook) => STATE.lock().wndproc_hook_raw = hook.0 as usize,
        Err(error) => tracing::error!("VO window hook rebind failed: {error}"),
    }
    if let Err(error) = crate::render::rebind(hwnd) {
        tracing::error!("VO composition target rebind failed: {error}");
    }
    crate::input::jfn_input_windows_recreate(raw);
    jfn_windows_sink::jfn_windows_sink_stop();
    jfn_windows_sink::jfn_windows_sink_start_for(raw as isize);
    if let Err(error) = crate::window::republish() {
        tracing::error!("VO window sample failed: {error}");
    }
}

pub(crate) fn win_cleanup() -> Result<(), crate::window::NotifierPanicked> {
    STATE.lock().initialized = false;
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
    let notifier = crate::window::stop_notifier();

    crate::render::cleanup();
    crate::window::clear();
    STATE.lock().mpv_hwnd_raw = 0;
    notifier
}

pub(crate) fn win_clamp_window_geometry(
    mut g: WindowGeometry,
) -> Result<WindowGeometry, ClampError> {
    let mut work = RECT::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut RECT as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .map_err(|e| ClampError::WorkArea(e.code().0))?;
    let vw = work.right - work.left;
    let vh = work.bottom - work.top;
    clamp_to_bounds(&mut g, Bounds { w: vw, h: vh });
    Ok(g)
}
