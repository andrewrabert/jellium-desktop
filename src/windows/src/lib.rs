//! Windows `Platform` backend.

#![cfg(target_os = "windows")]

use std::ffi::{OsStr, c_int, c_void};
use std::os::windows::ffi::OsStrExt;

use cef::rc::Rc;
use cef::{ImplTask, Task, ThreadId, WrapTask, post_task, wrap_task};
use windows::Win32::Foundation::HGLOBAL;
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

use jfn_platform_abi::{DisplayBackend, PaintFrame, Platform, WindowDecorations};

mod input;
mod menu;
mod mpv_host;
mod osr_popup;
mod platform;
mod process;
mod render;
mod window;

use crate::input::jfn_input_windows_set_cursor;
use crate::platform::{
    win_clamp_window_geometry, win_cleanup, win_early_init, win_get_display_scale, win_get_scale,
    win_init, win_query_window_position, win_set_fullscreen, win_toggle_fullscreen,
};

fn win_pump() {
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
fn win_set_theme_color(rgb: u32) {
    let Some(hwnd) = crate::platform::win_hwnd() else {
        return;
    };
    let r = (rgb >> 16) & 0xFF;
    let g = (rgb >> 8) & 0xFF;
    let b = rgb & 0xFF;
    let colorref: u32 = r | (g << 8) | (b << 16);
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            std::ptr::from_ref(&colorref).cast(),
            size_of::<u32>() as u32,
        )
    };
}

/// Map IdleInhibitLevel (None=0, System=1, Display=2) to execution-state
/// flags and post the call onto TID_UI so it lives on a stable thread.
fn win_set_idle_inhibit(level: c_int) {
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
// Clipboard (Win32 CF_UNICODETEXT) — read only; writes go through CEF's
// own frame->Copy() path which works correctly on Windows. Win32
// clipboard is synchronous; callback fires inline on the calling thread.
// =====================================================================

fn win_clipboard_read_text_async(on_done: Box<dyn FnOnce(&str) + Send>) {
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
fn win_open_external_url(url: &str) {
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
    IdleInhibitLevel, MenuDelivery, MenuKind, SurfaceHandle, WindowGeometry, WindowPos,
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
        render::alloc()
    }

    fn free_surface(&self, s: SurfaceHandle) {
        render::free(s);
    }

    fn surface_present(&self, s: SurfaceHandle, frame: PaintFrame<'_>) -> bool {
        render::present(s, render::Part::Content, frame)
    }

    fn surface_set_visible(&self, s: SurfaceHandle, visible: bool) {
        render::set_visible(s, visible);
    }

    fn restack(&self, ordered: &[SurfaceHandle]) {
        render::restack(ordered);
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

    fn get_scale(&self) -> f32 {
        win_get_scale()
    }

    fn get_display_scale(&self, x: c_int, y: c_int) -> f32 {
        win_get_display_scale(x, y)
    }

    fn window_source(&self) -> &'static dyn jfn_platform_abi::WindowSource {
        &crate::window::WIN_WINDOW_SOURCE
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
