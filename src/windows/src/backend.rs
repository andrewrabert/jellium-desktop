#![allow(non_snake_case)]

use std::ffi::{OsStr, c_int, c_void};
use std::os::windows::ffi::OsStrExt;

use cef::rc::Rc;
use cef::{ImplTask, Task, ThreadId, WrapTask, post_task, wrap_task};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::Graphics::Dwm::{DWMWA_CAPTION_COLOR, DwmSetWindowAttribute};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Power::{
    ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, EXECUTION_STATE,
    SetThreadExecutionState,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use jfn_platform_abi::{
    DisplayBackend, PaintFrame, Platform, Presented, Visibility, VisibilityCommit,
    WindowDecorations,
};

use crate::input::jfn_input_windows_set_cursor;
use crate::platform::{
    win_clamp_window_geometry, win_cleanup, win_early_init, win_get_scale, win_init,
    win_set_fullscreen, win_toggle_fullscreen,
};

fn win_pump() {}

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

/// The `CF_UNICODETEXT` on the already-open clipboard; `None` when it holds
/// none, or the handle could not be locked.
unsafe fn win_clipboard_text() -> Option<String> {
    unsafe {
        let handle = GetClipboardData(u32::from(CF_UNICODETEXT.0)).ok()?;
        let mem = HGLOBAL(handle.0);
        let wide = PCWSTR::from_raw(GlobalLock(mem).cast::<u16>());
        if wide.is_null() {
            return None;
        }
        let text = String::from_utf16_lossy(wide.as_wide());
        let _ = GlobalUnlock(mem);
        Some(text)
    }
}

/// Hands the OS a `GMEM_MOVEABLE` copy of `text` as `CF_UNICODETEXT` on the
/// already-open, already-emptied clipboard. `false` leaves the clipboard
/// holding nothing this call put there, and the block freed.
unsafe fn win_clipboard_offer(text: &str) -> bool {
    let wide: Vec<u16> = OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let bytes = std::mem::size_of_val(wide.as_slice());
    unsafe {
        let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
            return false;
        };
        let dst = GlobalLock(mem).cast::<u16>();
        if dst.is_null() {
            let _ = GlobalFree(Some(mem));
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        let _ = GlobalUnlock(mem);
        // Ownership of the block passes to the OS only once SetClipboardData
        // succeeds; a failure leaves it ours to free.
        if SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(mem.0))).is_err() {
            let _ = GlobalFree(Some(mem));
            return false;
        }
        true
    }
}

/// `None` when the clipboard holds no `CF_UNICODETEXT`, or it could not be
/// opened.
fn win_clipboard_read_text_async(on_done: OnText) {
    let mut text: Option<String> = None;
    unsafe {
        if OpenClipboard(None).is_ok() {
            text = win_clipboard_text();
            let _ = CloseClipboard();
        }
    }
    on_done(text.as_deref());
}

/// Places `text` on the clipboard as `CF_UNICODETEXT`. A hand-off the OS
/// refused puts back the text the clipboard held, which the emptying required
/// to take ownership had just removed.
fn win_clipboard_write_text(text: &str) {
    unsafe {
        if OpenClipboard(None).is_err() {
            return;
        }
        let previous = win_clipboard_text();
        if EmptyClipboard().is_ok()
            && !win_clipboard_offer(text)
            && let Some(previous) = previous.as_deref()
        {
            let _ = win_clipboard_offer(previous);
        }
        let _ = CloseClipboard();
    }
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

use jfn_platform_abi::{
    IdleInhibitLevel, MenuDelivery, MenuKind, OnText, SurfaceHandle, WindowGeometry, WindowPos,
};

/// SMTC-backed [`jfn_platform_abi::MediaSink`].
struct SmtcSink;

impl jfn_platform_abi::MediaSink for SmtcSink {
    fn start(&self, _instance: &jfn_platform_abi::Instance) {
        let Some(hwnd) = crate::platform::win_hwnd() else {
            tracing::error!(target: "Media", "[SMTC] mpv window unresolved; SMTC not started");
            return;
        };
        jfn_windows_sink::jfn_windows_sink_start_for(hwnd.0 as isize);
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

    fn cef_init_precedes_mpv_window(&self) -> bool {
        true
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

    // mpv's window is gone by the time this runs and the compositor devices
    // are already released
    fn post_window_cleanup(&self) {}

    fn window_decoration_options(&self) -> jfn_platform_abi::DecorationOptions {
        jfn_platform_abi::DecorationOptions::all()
    }

    // the decorations setting has no effect here
    fn window_decorations_supported(&self) -> bool {
        false
    }

    // DWM draws the titlebar; the app draws none
    fn effective_decorations(&self) -> jfn_platform_abi::EffectiveDecorations {
        jfn_platform_abi::EffectiveDecorations::ServerSide
    }

    // CEF runs hardware-accelerated on Windows
    fn shared_texture_supported(&self) -> bool {
        true
    }

    // the shared-texture answer is fixed; nothing revises it
    fn set_shared_texture_unsupported(&self) {}

    // the clipboard is not readable by another app without focus
    fn web_paste_reads_clipboard(&self) -> bool {
        true
    }

    fn alloc_surface(&self, initial: Visibility) -> SurfaceHandle {
        let s = crate::render::alloc();
        crate::render::set_visibility(s, initial).acknowledged();
        s
    }

    fn free_surface(&self, s: SurfaceHandle) {
        crate::render::free(s);
    }

    fn surface_present<'a>(
        &self,
        s: SurfaceHandle,
        frame: PaintFrame<'a>,
    ) -> Result<Presented, PaintFrame<'a>> {
        crate::render::present(s, crate::render::Part::Content, frame)
    }

    fn surface_resize(&self, s: SurfaceHandle, size: jfn_platform_abi::SurfaceSize) {
        crate::render::resize(s, size);
    }

    fn surface_window_target(&self, s: SurfaceHandle) -> Option<jfn_platform_abi::WindowTarget> {
        crate::render::window_target(s)
    }

    fn set_surface_visibility(&self, s: SurfaceHandle, visibility: Visibility) -> VisibilityCommit {
        crate::render::set_visibility(s, visibility)
    }

    fn apply_stack(&self, ordered: &[SurfaceHandle]) {
        crate::render::apply_stack(ordered);
    }

    fn menu_delivery(&self, kind: MenuKind) -> MenuDelivery {
        match kind {
            MenuKind::ContextMenu => MenuDelivery::Host(&crate::menu::WinMenuHost),
            MenuKind::Dropdown => MenuDelivery::Composited,
        }
    }

    fn osr_popup_surface(&self) -> &dyn jfn_platform_abi::OsrPopupSurface {
        &crate::osr_popup::WinOsrPopup
    }

    fn mpv_host(&self) -> &dyn jfn_platform_abi::MpvHost {
        &crate::mpv_host::WindowsMpvHost
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

    // mpv's own WndProc settles the size; nothing here gates a frame
    fn resize_gate(&self) -> Option<&dyn jfn_platform_abi::ResizeGate> {
        None
    }

    // DWM draws the titlebar
    fn titlebar_controls(&self) -> Option<&dyn jfn_platform_abi::TitlebarControls> {
        None
    }

    fn scale(&self) -> jfn_platform_abi::Scale {
        win_get_scale()
    }

    fn display_scale(&self, at: Option<WindowPos>) -> jfn_platform_abi::Scale {
        crate::scale::display_scale(at)
    }

    // mpv creates the HWND; the Win32 sample of it is its live geometry
    fn window_owner(&self) -> jfn_platform_abi::WindowOwner<'_> {
        jfn_platform_abi::WindowOwner::Mpv(&crate::window::WIN_WINDOW_SOURCE)
    }

    fn query_window_position(&self) -> Option<WindowPos> {
        crate::window::snapshot()?.position
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

    fn clipboard_read_text_async(&self, on_done: OnText) {
        win_clipboard_read_text_async(on_done);
    }

    fn clipboard_write_text(&self, text: &str) {
        win_clipboard_write_text(text);
    }

    fn open_external_url(&self, url: &str) {
        win_open_external_url(url);
    }

    fn open_path(&self, path: &std::path::Path) {
        let native: String = path
            .to_string_lossy()
            .chars()
            .map(|c| if c == '/' { '\\' } else { c })
            .collect();
        let _ = std::process::Command::new("explorer").arg(native).spawn();
    }

    fn install_shutdown_handler(&self, on_shutdown: fn()) {
        crate::process::install_shutdown(on_shutdown);
    }
}

pub fn make_windows_platform() -> Box<dyn Platform> {
    Box::new(WindowsPlatform)
}
