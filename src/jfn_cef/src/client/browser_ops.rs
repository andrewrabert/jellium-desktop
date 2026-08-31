use cef::{
    Browser, BrowserHost, BrowserSettings, CefString, Frame, ImplBrowser, ImplBrowserHost,
    ImplFrame, PaintElementType, ProcessId, WindowInfo, browser_host_create_browser,
    process_message_create, sys,
};
use std::sync::Arc;

use super::Inner;
use crate::frame_rate::FrameRate;

impl Inner {
    pub(super) fn browser_clone(&self) -> Option<Browser> {
        self.browser.lock().browser.clone()
    }

    pub(super) fn host(&self) -> Option<BrowserHost> {
        self.browser_clone().and_then(|b| b.host())
    }

    pub(crate) fn close_browser_force(&self) {
        if let Some(host) = self.host() {
            host.close_browser(1);
        }
    }

    pub(super) fn focused_or_main(&self) -> Option<Frame> {
        let b = self.browser_clone()?;
        b.focused_frame().or_else(|| b.main_frame())
    }

    pub(super) fn notify_screen_info_changed(&self) {
        if let Some(h) = self.host() {
            h.notify_screen_info_changed();
        }
    }

    pub(super) fn cef_was_resized(&self) {
        if let Some(h) = self.host() {
            h.was_resized();
        }
    }

    pub(crate) fn invalidate_view(&self) {
        if let Some(h) = self.host() {
            h.invalidate(PaintElementType::VIEW);
        }
    }

    /// Dead on platforms whose `CefHost` doesn't enable external
    /// BeginFrame — callers gate on it.
    pub(crate) fn send_external_begin_frame(&self) {
        if let Some(h) = self.host() {
            h.send_external_begin_frame();
        }
    }

    pub(super) fn cef_set_windowless_frame_rate(&self, hz: i32) {
        if let Some(h) = self.host() {
            h.set_windowless_frame_rate(hz);
        }
    }

    pub(crate) fn cef_was_hidden(&self, hidden: bool) {
        if let Some(h) = self.host() {
            h.was_hidden(if hidden { 1 } else { 0 });
        }
    }

    pub(crate) fn exec_js(&self, js: &str) {
        let Some(b) = self.browser_clone() else {
            return;
        };
        let Some(f) = b.main_frame() else { return };
        let code = CefString::from(js);
        f.execute_java_script(Some(&code), Some(&CefString::from("")), 0);
    }

    pub(super) fn send_process_message_named(&self, name: &str) {
        let Some(f) = self.focused_or_main() else {
            return;
        };
        let Some(mut msg) = process_message_create(Some(&CefString::from(name))) else {
            return;
        };
        f.send_process_message(
            ProcessId::from(sys::cef_process_id_t::PID_RENDERER),
            Some(&mut msg),
        );
    }

    pub(super) fn cef_create_browser(self: &Arc<Self>, url: &str) -> bool {
        let shared = self.paint_mode.shared_textures();
        let parent: sys::cef_window_handle_t = unsafe { std::mem::zeroed() };
        let mut wi = WindowInfo::default().set_as_windowless(parent);
        wi.shared_texture_enabled = if shared { 1 } else { 0 };
        let external_bf = jfn_platform_abi::try_get()
            .and_then(|p| p.cef_host())
            .is_some_and(|h| h.external_begin_frame());
        wi.external_begin_frame_enabled = if external_bf { 1 } else { 0 };

        // Zero is CEF's own default.
        let fr = self.frame_rate.load().map_or(0, FrameRate::get);
        let bs = BrowserSettings {
            background_color: 0,
            windowless_frame_rate: fr,
            ..BrowserSettings::default()
        };
        jfn_logging::log(
            jfn_logging::Category::Cef,
            jfn_logging::Level::Debug,
            &format!("create browser windowless_frame_rate={fr} shared_textures={shared}"),
        );

        let extra = crate::injection::build_web(shared);

        let mut client = crate::client_impl::make_client(Arc::clone(self));
        let url_cef = CefString::from(url);
        let mut extra_opt = extra.into_dictionary();
        browser_host_create_browser(
            Some(&wi),
            Some(&mut client),
            Some(&url_cef),
            Some(&bs),
            extra_opt.as_mut(),
            None,
        ) != 0
    }

    pub(super) fn exec_js_focused(&self, js: &str) {
        let Some(f) = self.focused_or_main() else {
            return;
        };
        let code = CefString::from(js);
        let url_uf = f.url();
        let url = CefString::from(&url_uf);
        f.execute_java_script(Some(&code), Some(&url), 0);
    }

    pub(crate) fn frame_paste(&self) {
        if let Some(f) = self.focused_or_main() {
            f.paste();
        }
    }

    pub(crate) fn frame_undo(&self) {
        if let Some(f) = self.focused_or_main() {
            f.undo();
        }
    }

    pub(crate) fn frame_redo(&self) {
        if let Some(f) = self.focused_or_main() {
            f.redo();
        }
    }

    pub(crate) fn frame_cut(&self) {
        if let Some(f) = self.focused_or_main() {
            f.cut();
        }
    }

    pub(crate) fn frame_copy(&self) {
        if let Some(f) = self.focused_or_main() {
            f.copy();
        }
    }

    pub(crate) fn frame_select_all(&self) {
        if let Some(f) = self.focused_or_main() {
            f.select_all();
        }
    }

    pub(crate) fn browser_alive(&self) -> bool {
        self.browser.lock().browser.is_some()
    }
}
