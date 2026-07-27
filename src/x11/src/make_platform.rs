//! X11 backend impl of [`jfn_platform_abi::Platform`].

#![allow(non_snake_case)]
// Platform trait carries raw-pointer args (dirty rects, accel-paint info)
// from CEF; trait impls forward them unchanged to unsafe FFI fns.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_int, c_void};
use std::os::fd::BorrowedFd;

use jfn_gpu_paint::{DmabufFormat, DmabufFrame, DmabufPlane};

use crate::registry::SurfaceId;
use crate::surface;

/// CEF reclaims the original fd when the paint callback returns, so each plane
/// fd is dup'd into an `OwnedFd` the presenter worker can outlive.
unsafe fn to_dmabuf_frame(info: *const c_void) -> Option<DmabufFrame> {
    let info = info as *const cef::sys::_cef_accelerated_paint_info_t;
    if info.is_null() {
        return None;
    }
    let info = unsafe { &*info };
    if info.plane_count < 1 {
        return None;
    }
    let format = match info.format {
        cef::sys::cef_color_type_t::CEF_COLOR_TYPE_BGRA_8888 => DmabufFormat::Bgra8,
        cef::sys::cef_color_type_t::CEF_COLOR_TYPE_RGBA_8888 => DmabufFormat::Rgba8,
        _ => return None,
    };
    let w = info.extra.coded_size.width;
    let h = info.extra.coded_size.height;
    if w <= 0 || h <= 0 {
        return None;
    }
    let vw = info.extra.visible_rect.width.max(0);
    let vh = info.extra.visible_rect.height.max(0);
    // Include every memory plane the modifier uses; DCC/CCS modifiers add an
    // auxiliary plane beyond the color plane.
    let n = info.plane_count.clamp(0, info.planes.len() as i32) as usize;
    if n < 1 {
        return None;
    }
    let mut planes = Vec::with_capacity(n);
    for p in &info.planes[..n] {
        let fd = nix::unistd::dup(unsafe { BorrowedFd::borrow_raw(p.fd) }).ok()?;
        planes.push(DmabufPlane {
            fd,
            offset: p.offset,
            stride: p.stride,
        });
    }
    Some(DmabufFrame {
        width: w as u32,
        height: h as u32,
        visible_w: vw as u32,
        visible_h: vh as u32,
        format,
        modifier: info.modifier,
        planes,
    })
}

use jfn_platform_abi::cursor::CursorShape;
pub use jfn_platform_abi::{
    DisplayBackend, IdleInhibitLevel, JfnContextMenuRequest, JfnPopupRequest, JfnRect, Platform,
    SurfaceHandle, SurfaceSize, WindowDecorations, WindowGeometry, WindowPos,
};

pub struct X11Platform;

impl Platform for X11Platform {
    fn display(&self) -> DisplayBackend {
        DisplayBackend::X11
    }

    fn default_window_decorations(&self) -> WindowDecorations {
        jfn_linux_util::default_window_decorations()
    }

    fn resolve_window_decorations(
        &self,
        configured: Option<WindowDecorations>,
    ) -> WindowDecorations {
        match configured.unwrap_or_else(|| self.default_window_decorations()) {
            WindowDecorations::Csd => WindowDecorations::Server,
            other => other,
        }
    }

    fn init(&self, _mpv: *mut c_void) -> bool {
        crate::lifecycle::init()
    }

    fn cleanup(&self) {
        crate::lifecycle::cleanup();
    }

    // Runs after mpv_terminate_destroy: mpv's embedded window is gone, so the
    // top-level's connection can finally close.
    fn post_window_cleanup(&self) {
        crate::geometry::drop_toplevel_connection();
    }

    fn alloc_surface(&self) -> SurfaceHandle {
        surface::alloc_surface().to_handle()
    }

    fn free_surface(&self, s: SurfaceHandle) {
        surface::free_surface(SurfaceId::from_handle(s));
    }

    fn surface_present(&self, s: SurfaceHandle, info: *const c_void) -> bool {
        let Some(frame) = (unsafe { to_dmabuf_frame(info) }) else {
            return false;
        };
        surface::surface_present_dmabuf(SurfaceId::from_handle(s), frame)
    }

    fn surface_present_software(
        &self,
        s: SurfaceHandle,
        dirty: &[JfnRect],
        buffer: *const c_void,
        w: c_int,
        h: c_int,
    ) -> bool {
        unsafe {
            surface::surface_present_software(
                SurfaceId::from_handle(s),
                dirty.as_ptr(),
                dirty.len(),
                buffer,
                w,
                h,
            )
        }
    }

    fn surface_resize(&self, s: SurfaceHandle, size: SurfaceSize) {
        surface::surface_resize(SurfaceId::from_handle(s), size.physical_w, size.physical_h);
    }

    fn surface_set_visible(&self, s: SurfaceHandle, visible: bool) {
        surface::surface_set_visible(SurfaceId::from_handle(s), visible);
    }

    fn restack(&self, handles: &[SurfaceHandle]) {
        let ids: Vec<SurfaceId> = handles.iter().map(|&h| SurfaceId::from_handle(h)).collect();
        surface::restack(&ids);
    }

    fn dropdown_backend(&self) -> &'static dyn jfn_platform_abi::DropdownBackend {
        &jfn_platform_abi::JsMenuDropdown
    }

    fn context_menu_backend(&self) -> &'static dyn jfn_platform_abi::ContextMenuBackend {
        crate::context_menu::backend()
    }

    fn media_session(&self) -> &dyn jfn_platform_abi::MediaSink {
        &jfn_mpris::MprisSink
    }

    fn mpv_host(&self) -> &dyn jfn_platform_abi::MpvHost {
        &crate::mpv_host::X11MpvHost
    }

    fn cef_paths(&self) -> jfn_platform_abi::CefPaths {
        jfn_linux_util::cef_paths()
    }

    fn window_decorations_supported(&self) -> bool {
        true
    }

    fn window_decoration_options(&self) -> jfn_platform_abi::DecorationOptions {
        jfn_platform_abi::DecorationOptions::with_server(false)
    }

    fn begin_transition(&self) {
        let snap = crate::x11_state::parent_snapshot();
        crate::x11_state::GATE
            .lock()
            .begin_capturing((snap.width, snap.height));
    }

    fn end_transition(&self) {
        // Only end the gate; the geometry thread is the sole owner of overlay
        // structure, so do not re-apply it here.
        crate::x11_state::GATE.lock().end();
    }

    fn in_transition(&self) -> bool {
        crate::x11_state::GATE.lock().in_transition()
    }

    fn set_expected_size(&self, w: c_int, h: c_int) {
        crate::x11_state::GATE.lock().set_expected((w, h));
    }

    fn set_fullscreen(&self, fullscreen: bool) {
        // The app owns fullscreen: drive the toplevel's `_NET_WM_STATE` and
        // reconcile; WM-initiated flips flow back via the geometry thread.
        crate::geometry::set_parent_fullscreen(fullscreen);
    }

    fn toggle_fullscreen(&self) {
        let fullscreen = crate::x11_state::parent_snapshot().fullscreen;
        crate::geometry::set_parent_fullscreen(!fullscreen);
    }

    fn get_scale(&self) -> f32 {
        // App-owned scale (Xft.dpi probe), seeded at host-window creation and
        // refreshed by the geometry thread on RESOURCE_MANAGER changes.
        let scale = crate::x11_state::parent_snapshot().scale;
        if scale > 0.0 { scale } else { 1.0 }
    }

    // The app owns the toplevel and the display scale; mpv's
    // `display-hidpi-scale` is not authoritative here.
    fn effective_scale(&self, _mpv_display_hidpi_scale: f64) -> f32 {
        self.get_scale()
    }

    fn get_display_scale(&self, _x: c_int, _y: c_int) -> f32 {
        crate::scale::query_display_scale().unwrap_or(1.0)
    }

    fn apply_boot_geometry(&self, g: &jfn_platform_abi::BootGeometry) {
        crate::lifecycle::set_boot_geometry(*g);
    }

    // The app owns its toplevel and sizes it in ensure_host_window, so mpv
    // neither sizes at boot nor reconciles on scale change.
    fn boot_mpv_geometry(&self, _g: &jfn_platform_abi::BootGeometry) -> Option<String> {
        None
    }

    fn reconcile_mpv_size(
        &self,
        _display_hidpi_scale: f64,
        _saved_scale: f32,
        _saved_logical: jfn_platform_abi::LogicalSize,
        _locked: bool,
    ) -> Option<jfn_platform_abi::PhysicalSize> {
        None
    }

    fn window_source(&self) -> &'static dyn jfn_platform_abi::WindowSource {
        &crate::window_source::X11_WINDOW_SOURCE
    }

    fn query_window_position(&self) -> Option<WindowPos> {
        let conn = crate::x11_state::x11rb_conn()?;
        let host = crate::x11_state::host()?;
        let (x, y, _, _) =
            crate::lifecycle::query_parent_geometry_x11rb(&conn, host.toplevel, host.root)?;
        Some(WindowPos { x, y })
    }

    fn clamp_window_geometry(&self, g: WindowGeometry) -> WindowGeometry {
        // X11 constrains only the size; position is left to the WM.
        let (mut w, mut h) = (g.w, g.h);
        crate::lifecycle::clamp_window_geometry(&mut w, &mut h);
        WindowGeometry {
            w,
            h,
            position: g.position,
        }
    }

    fn set_cursor(&self, shape: CursorShape) {
        crate::input_lifecycle::set_cursor_active(shape);
    }

    fn set_idle_inhibit(&self, level: IdleInhibitLevel) {
        jfn_linux_util::idle_inhibit::set(level as u32);
    }

    fn shared_texture_supported(&self) -> bool {
        crate::x11_state::paint().is_some_and(|p| p.use_dmabuf)
    }

    fn clipboard_text_supported(&self) -> bool {
        false
    }

    fn clipboard_read_text_async(&self, on_done: Box<dyn FnOnce(&str) + Send>) {
        // X11 has no native clipboard read path here — fire empty result.
        on_done("");
    }

    fn open_external_url(&self, url: &str) {
        jfn_linux_util::open_url::open(url);
    }

    fn open_path(&self, path: &std::path::Path) {
        jfn_linux_util::open_url::open(&path.to_string_lossy());
    }
}

pub fn make_x11_platform() -> Box<dyn Platform> {
    Box::new(X11Platform)
}
