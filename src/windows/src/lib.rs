//! Windows `Platform` backend.

#![cfg(target_os = "windows")]

use std::ffi::{OsStr, c_int, c_void};
use std::os::windows::ffi::OsStrExt;

use cef::rc::Rc;
use cef::{ImplTask, Task, ThreadId, WrapTask, post_task, wrap_task};
use windows::Win32::Foundation::{HGLOBAL, HWND};
use windows::Win32::Graphics::Dwm::{DWMWA_CAPTION_COLOR, DwmSetWindowAttribute};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Power::{
    ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, EXECUTION_STATE,
    SetThreadExecutionState,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

pub use jfn_platform_abi::{DisplayBackend, JfnRect, PaintFrame, Platform, WindowDecorations};

mod compositor;
mod input;
mod menu;
mod mpv_host;
mod osr_popup;
mod platform;
mod process;
pub use compositor::{
    jfn_win_begin_transition_locked, jfn_win_cleanup_compositor, jfn_win_init_compositor,
    jfn_win_update_surface_size, jfn_win_wndproc_begin_transition_locked,
    jfn_win_wndproc_end_transition_locked, win_alloc_surface, win_end_transition, win_free_surface,
    win_restack, win_set_expected_size, win_surface_present, win_surface_present_software,
    win_surface_resize, win_surface_set_visible,
};
pub use input::{
    jfn_input_windows_resize_to_parent, jfn_input_windows_run_input_thread,
    jfn_input_windows_set_cursor, jfn_input_windows_stop_input_thread,
};
pub use platform::{
    jfn_win_get_hwnd, win_clamp_window_geometry, win_cleanup, win_early_init,
    win_get_display_scale, win_get_scale, win_init, win_query_window_position, win_set_fullscreen,
    win_toggle_fullscreen,
};

pub fn win_pump() {
    // Input handled by dedicated input-thread message loop.
}

// =====================================================================
// State-bound bodies ported to native Rust.
// =====================================================================

// =====================================================================
// CEF task bouncer — posts SetThreadExecutionState(flags) onto TID_UI so
// the assertion lives on a stable CEF UI thread. Per-thread state is
// released when that thread calls ES_CONTINUOUS alone. CEF owns the
// refcount through the `cef` crate's Task wrapper.
// =====================================================================

wrap_task! {
    struct ExecutionStateTask {
        flags: EXECUTION_STATE,
    }
    impl Task {
        fn execute(&self) {
            unsafe { SetThreadExecutionState(self.flags) };
        }
    }
}

/// Tint the DWM titlebar so it matches the current theme color.
/// rgb is 0x00RRGGBB; DWMWA_CAPTION_COLOR wants 0x00BBGGRR (COLORREF).
pub fn win_set_theme_color(rgb: u32) {
    let hwnd = jfn_win_get_hwnd();
    if hwnd.is_null() {
        return;
    }
    let r = (rgb >> 16) & 0xFF;
    let g = (rgb >> 8) & 0xFF;
    let b = rgb & 0xFF;
    let colorref: u32 = r | (g << 8) | (b << 16);
    let _ = unsafe {
        DwmSetWindowAttribute(
            HWND(hwnd),
            DWMWA_CAPTION_COLOR,
            std::ptr::from_ref(&colorref).cast(),
            size_of::<u32>() as u32,
        )
    };
}

/// Map IdleInhibitLevel (None=0, System=1, Display=2) to execution-state
/// flags and post the call onto TID_UI so it lives on a stable thread.
pub fn win_set_idle_inhibit(level: c_int) {
    let mut flags = ES_CONTINUOUS;
    match level {
        2 => flags |= ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED,
        1 => flags |= ES_SYSTEM_REQUIRED,
        _ => {}
    }
    let mut task = ExecutionStateTask::new(flags);
    let _ = post_task(ThreadId::UI, Some(&mut task));
}

// =====================================================================
// Fullscreen-transition gating lives in a jfn-compositor-core
// TransitionGate held inside the compositor's STATE lock (see
// compositor::gate_in_transition); these are thin entry points.
// =====================================================================

pub fn win_begin_transition() {
    jfn_win_begin_transition_locked();
}

pub fn win_in_transition() -> bool {
    crate::compositor::gate_in_transition()
}

// =====================================================================
// Clipboard (Win32 CF_UNICODETEXT) — read only; writes go through CEF's
// own frame->Copy() path which works correctly on Windows. Win32
// clipboard is synchronous; callback fires inline on the calling thread.
// =====================================================================

pub fn win_clipboard_read_text_async(on_done: Box<dyn FnOnce(&str) + Send>) {
    let mut text = String::new();
    unsafe {
        if OpenClipboard(None).is_ok() {
            if let Ok(handle) = GetClipboardData(u32::from(CF_UNICODETEXT.0)) {
                let mem = HGLOBAL(handle.0);
                let wide = PCWSTR::from_raw(GlobalLock(mem).cast::<u16>());
                if !wide.is_null() {
                    text = String::from_utf16_lossy(wide.as_wide());
                    // GlobalUnlock reports FALSE with no error once the lock
                    // count reaches zero; the Err is expected.
                    let _ = GlobalUnlock(mem);
                }
            }
            let _ = CloseClipboard();
        }
    }
    on_done(&text);
}

/// Open an external URL via `ShellExecuteW(open)`.
pub fn win_open_external_url(url: &str) {
    if url.is_empty() {
        return;
    }
    let wurl: Vec<u16> = OsStr::new(url)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let _ = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR::from_raw(wurl.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
}

// =====================================================================
// Backend impl
// =====================================================================

use jfn_platform_abi::{
    IdleInhibitLevel, MenuDelivery, MenuKind, SurfaceHandle, SurfaceSize, WindowGeometry, WindowPos,
};

/// SMTC-backed [`jfn_platform_abi::MediaSink`].
struct SmtcSink;

impl jfn_platform_abi::MediaSink for SmtcSink {
    fn start(&self, _instance: &jfn_platform_abi::Instance) {
        jfn_windows_sink::jfn_windows_sink_start();
    }

    fn stop(&self) {
        jfn_windows_sink::jfn_windows_sink_stop();
    }
}

pub struct WindowsPlatform;

impl Platform for WindowsPlatform {
    fn display(&self) -> DisplayBackend {
        DisplayBackend::Windows
    }

    fn default_window_decorations(&self) -> WindowDecorations {
        WindowDecorations::ServerThemed
    }

    fn early_init(&self) {
        win_early_init();
    }

    fn init(&self, mpv: *mut c_void) -> bool {
        win_init(mpv)
    }

    fn cleanup(&self) {
        win_cleanup();
    }

    fn alloc_surface(&self) -> SurfaceHandle {
        SurfaceHandle::from_ptr(win_alloc_surface())
    }

    fn free_surface(&self, s: SurfaceHandle) {
        win_free_surface(s.as_ptr());
    }

    fn surface_present(&self, s: SurfaceHandle, frame: PaintFrame<'_>) -> bool {
        match frame {
            PaintFrame::Accelerated(tex) => win_surface_present(s.as_ptr(), &tex),
            // Only reachable with --disable-gpu-compositing: the painter draws
            // both frame kinds, so there is nothing to gain by refusing one.
            PaintFrame::Software {
                size,
                pixels,
                dirty,
            } => win_surface_present_software(s.as_ptr(), pixels, size, dirty),
        }
    }

    fn surface_resize(&self, s: SurfaceHandle, size: SurfaceSize) {
        win_surface_resize(
            s.as_ptr(),
            size.logical_w,
            size.logical_h,
            size.physical_w,
            size.physical_h,
        );
    }

    fn surface_set_visible(&self, s: SurfaceHandle, visible: bool) {
        win_surface_set_visible(s.as_ptr(), visible);
    }

    fn restack(&self, ordered: &[SurfaceHandle]) {
        // `SurfaceHandle` is `#[repr(transparent)]` over `*mut c_void`, so the
        // slice pointer reinterprets directly.
        win_restack(ordered.as_ptr() as *const *mut c_void, ordered.len());
    }

    fn menu_delivery(&self, kind: MenuKind) -> MenuDelivery {
        match kind {
            MenuKind::ContextMenu => MenuDelivery::Host(&menu::WinMenuHost),
            MenuKind::Dropdown => MenuDelivery::Composited,
        }
    }

    fn osr_popup_surface(&self) -> &dyn jfn_platform_abi::OsrPopupSurface {
        &osr_popup::WinOsrPopup
    }

    fn mpv_host(&self) -> &dyn jfn_platform_abi::MpvHost {
        &mpv_host::WindowsMpvHost
    }

    fn media_session(&self) -> &dyn jfn_platform_abi::MediaSink {
        &SmtcSink
    }

    fn cef_paths(&self) -> jfn_platform_abi::CefPaths {
        let exe = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .unwrap_or_default();
        let dir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        jfn_platform_abi::CefPaths {
            browser_subprocess_path: Some(exe),
            resources_dir_path: Some(dir.clone()),
            locales_dir_path: Some(dir.join("locales")),
            ..Default::default()
        }
    }

    fn set_fullscreen(&self, v: bool) {
        win_set_fullscreen(v);
    }

    fn toggle_fullscreen(&self) {
        win_toggle_fullscreen();
    }

    fn begin_transition(&self) {
        win_begin_transition();
    }

    fn end_transition(&self) {
        win_end_transition();
    }

    fn in_transition(&self) -> bool {
        win_in_transition()
    }

    fn set_expected_size(&self, w: c_int, h: c_int) {
        win_set_expected_size(w, h);
    }

    fn get_scale(&self) -> f32 {
        win_get_scale()
    }

    fn get_display_scale(&self, x: c_int, y: c_int) -> f32 {
        win_get_display_scale(x, y)
    }

    fn window_source(&self) -> &'static dyn jfn_platform_abi::WindowSource {
        &jfn_playback::window_source::MPV_WINDOW_SOURCE
    }

    fn query_window_position(&self) -> Option<WindowPos> {
        let (mut x, mut y) = (0, 0);
        if win_query_window_position(&mut x, &mut y) {
            Some(WindowPos { x, y })
        } else {
            None
        }
    }

    fn clamp_window_geometry(&self, g: WindowGeometry) -> WindowGeometry {
        let (mut w, mut h) = (g.w, g.h);
        let (mut x, mut y) = g.raw_position();
        win_clamp_window_geometry(&mut w, &mut h, &mut x, &mut y);
        WindowGeometry::from_raw(w, h, x, y)
    }

    fn pump(&self) {
        win_pump();
    }

    fn set_cursor(&self, shape: jfn_platform_abi::cursor::CursorShape) {
        jfn_input_windows_set_cursor(shape.as_raw());
    }

    fn set_idle_inhibit(&self, level: IdleInhibitLevel) {
        win_set_idle_inhibit(level as c_int);
    }

    fn set_theme_color(&self, rgb: u32) {
        win_set_theme_color(rgb);
    }

    fn clipboard_read_text_async(&self, on_done: Box<dyn FnOnce(&str) + Send>) {
        win_clipboard_read_text_async(on_done);
    }

    fn open_external_url(&self, url: &str) {
        win_open_external_url(url);
    }

    fn open_path(&self, path: &std::path::Path) {
        // explorer.exe wants native backslash-separated paths.
        let native: String = path
            .to_string_lossy()
            .chars()
            .map(|c| if c == '/' { '\\' } else { c })
            .collect();
        let _ = std::process::Command::new("explorer").arg(native).spawn();
    }

    fn install_shutdown_handler(&self, on_shutdown: fn()) {
        process::install_shutdown(on_shutdown);
    }
}

pub fn make_windows_platform() -> Box<dyn Platform> {
    Box::new(WindowsPlatform)
}
