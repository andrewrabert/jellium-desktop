#![allow(non_snake_case)]

use parking_lot::Mutex;
use std::ffi::{c_int, c_void};
use std::thread::JoinHandle;

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
use jfn_platform_abi::geometry::{Bounds, WindowGeometry, clamp_to_bounds};
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

pub(crate) fn win_is_fullscreen() -> bool {
    let Some(hwnd) = win_hwnd() else {
        return false;
    };
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    (style & WS_CAPTION.0) == 0 && (style & WS_THICKFRAME.0) == 0
}

pub(crate) fn win_get_scale() -> jfn_platform_abi::Scale {
    match crate::window::client_scale() {
        Some(scale) => scale,
        None => win_display_scale(),
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
                WM_MOVE => {
                    crate::window::sample();
                }
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

pub(crate) fn win_early_init() {}

pub(crate) fn win_init(_mpv: *mut c_void) -> Result<(), jfn_platform_abi::PlatformInitError> {
    let Some(hwnd) = win_ensure_hwnd() else {
        return Err(jfn_platform_abi::PlatformInitError::backend(
            "Windows window acquisition",
            "no observed mpv window-id",
        ));
    };
    let hwnd_raw = hwnd.0 as usize;
    crate::window::republish();

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

    crate::window::republish();
    tracing::info!("Windows DirectComposition compositor initialized");
    Ok(())
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
