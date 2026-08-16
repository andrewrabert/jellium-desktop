//! X11 backend impl of [`jfn_platform_abi::Platform`].

#![allow(non_snake_case)]

use std::ffi::{c_int, c_void};

use crate::registry::SurfaceId;
use crate::surface;

use jfn_platform_abi::cursor::CursorShape;
pub use jfn_platform_abi::{
    DisplayBackend, IdleInhibitLevel, JfnRect, PaintFrame, Platform, Presented, SurfaceHandle,
    SurfaceSize, Visibility, VisibilityCommit, WindowDecorations, WindowGeometry, WindowPos,
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

    fn alloc_surface(&self, initial: Visibility) -> SurfaceHandle {
        surface::alloc_surface(initial).to_handle()
    }

    fn free_surface(&self, s: SurfaceHandle) {
        surface::free_surface(SurfaceId::from_handle(s));
    }

    fn surface_present<'a>(
        &self,
        s: SurfaceHandle,
        frame: PaintFrame<'a>,
    ) -> Result<Presented, PaintFrame<'a>> {
        surface::present(SurfaceId::from_handle(s), frame)
    }

    /// X11 owns the overlay's size through parent geometry; the only part of
    /// the request this backend applies is the reserved top strip.
    fn surface_resize(&self, s: SurfaceHandle, size: SurfaceSize) {
        surface::surface_set_top_inset(SurfaceId::from_handle(s), size.physical_top);
    }

    fn surface_window_target(&self, s: SurfaceHandle) -> Option<jfn_platform_abi::WindowTarget> {
        surface::window_target(SurfaceId::from_handle(s))
    }

    fn set_surface_visibility(&self, s: SurfaceHandle, visibility: Visibility) -> VisibilityCommit {
        surface::set_visibility(SurfaceId::from_handle(s), visibility)
    }

    fn apply_stack(&self, ordered: &[SurfaceHandle]) {
        let ids: Vec<SurfaceId> = ordered.iter().map(|&h| SurfaceId::from_handle(h)).collect();
        surface::apply_stack(&ids);
    }

    fn menu_delivery(&self, kind: jfn_platform_abi::MenuKind) -> jfn_platform_abi::MenuDelivery {
        match kind {
            jfn_platform_abi::MenuKind::ContextMenu => {
                jfn_platform_abi::MenuDelivery::Host(crate::menu::host())
            }
            jfn_platform_abi::MenuKind::Dropdown => jfn_platform_abi::MenuDelivery::Page,
        }
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
        let Some(snap) = crate::x11_state::parent_snapshot() else {
            tracing::warn!(target: "Platform", "no published geometry; nothing to gate");
            return;
        };
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
        let Some(snap) = crate::x11_state::parent_snapshot() else {
            tracing::warn!(target: "Platform", "no published geometry; nothing to gate");
            return;
        };
        crate::geometry::set_parent_fullscreen(!snap.fullscreen);
    }

    fn scale(&self) -> jfn_platform_abi::Scale {
        crate::scale::window_scale()
    }

    fn display_scale(&self, at: Option<WindowPos>) -> jfn_platform_abi::Scale {
        crate::scale::display_scale(at)
    }

    fn apply_boot_geometry(&self, g: &jfn_platform_abi::BootGeometry) {
        crate::lifecycle::set_boot_geometry(*g);
    }

    // The app owns its toplevel and sizes it in ensure_host_window, so mpv
    // neither sizes at boot nor reconciles on scale change.
    fn boot_mpv_geometry(&self, _g: &jfn_platform_abi::BootGeometry) -> Option<String> {
        None
    }

    // this backend owns its toplevel; mpv's sizing is never reconciled
    fn reconcile_mpv_size(
        &self,
        _saved_logical: jfn_platform_abi::LogicalSize,
        _saved_physical: jfn_platform_abi::PhysicalSize,
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
        crate::paint::resolved().is_some_and(|t| t.use_dmabuf)
    }

    fn clipboard_read_text_async(&self, on_done: Box<dyn FnOnce(Option<&str>) + Send>) {
        match crate::selection::selections() {
            Some(selections) => {
                selections.read_text_async(crate::selection::Kind::Clipboard, on_done);
            }
            None => on_done(None),
        }
    }

    fn clipboard_write_text(&self, text: &str) {
        if let Some(selections) = crate::selection::selections() {
            selections.write_text(crate::selection::Kind::Clipboard, text);
        }
    }

    fn primary_selection(&self) -> Option<&dyn jfn_platform_abi::PrimarySelection> {
        crate::selection::selections()
            .map(|_| &crate::selection::X11Primary as &dyn jfn_platform_abi::PrimarySelection)
    }

    /// CEF owns its own X11 clipboard; the shell overlay's does not reach it.
    fn web_paste_reads_clipboard(&self) -> bool {
        false
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
