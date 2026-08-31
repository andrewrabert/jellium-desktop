use std::os::raw::c_int;
use std::sync::Arc;

use jfn_platform_abi::cursor::CursorShape;

use super::{Inner, tasks};
use crate::platform_ops;

/// CEF's `ERR_ABORTED`, taken from the generated bindings rather than its
/// value.
const ERR_ABORTED: c_int = cef::sys::cef_errorcode_t::ERR_ABORTED as c_int;

impl Inner {
    pub(crate) fn on_fullscreen_mode_change(&self, fullscreen: bool) {
        if let Some(p) = platform_ops::ops() {
            p.set_fullscreen(fullscreen);
        }
    }

    pub(crate) fn emit_cursor(&self, shape: CursorShape) {
        jfn_input::cursor::cursor_from_web(shape);
    }

    pub(crate) fn on_console_message(&self, level: c_int, msg: &str, src: &str, line: c_int) {
        const LOGSEVERITY_VERBOSE: c_int = 1;
        const LOGSEVERITY_INFO: c_int = 2;
        const LOGSEVERITY_WARNING: c_int = 3;
        const LOGSEVERITY_ERROR: c_int = 4;
        const LOGSEVERITY_DEFAULT: c_int = 0;
        let formatted = format!("{} ({}:{})", msg, src, line);
        let lvl = if level >= LOGSEVERITY_ERROR {
            jfn_logging::Level::Error
        } else if level == LOGSEVERITY_WARNING {
            jfn_logging::Level::Warn
        } else if level == LOGSEVERITY_INFO || level == LOGSEVERITY_DEFAULT {
            jfn_logging::Level::Info
        } else {
            let _ = LOGSEVERITY_VERBOSE;
            jfn_logging::Level::Debug
        };
        jfn_logging::log(jfn_logging::Category::Js, lvl, &formatted);
    }

    /// Marks which navigation's document is now producing pixels; a frame of
    /// that document still has to be presented before anything retires.
    pub(crate) fn on_load_end(&self, is_main: bool, code: c_int, url: &str) {
        let formatted = format!(
            "CefLayer::OnLoadEnd name={} main={} code={} url={}",
            self.name_str(),
            if is_main { 1 } else { 0 },
            code,
            url,
        );
        jfn_logging::log(
            jfn_logging::Category::Cef,
            jfn_logging::Level::Info,
            &formatted,
        );
        if is_main {
            self.note_main_frame_loaded(url);
        }
    }

    pub(crate) fn on_load_error(&self, is_main: bool, code: c_int, text: &str, url: &str) {
        let formatted = format!(
            "OnLoadError name={} url={} error={} {}",
            self.name_str(),
            url,
            code,
            text,
        );
        jfn_logging::log(
            jfn_logging::Category::Cef,
            jfn_logging::Level::Error,
            &formatted,
        );
        // An aborted main-frame load is another navigation replacing this one,
        // not a failure.
        if is_main
            && code != ERR_ABORTED
            && let Some(navigation) = self.load_navigation(url)
        {
            jfn_bringup::advance(jfn_bringup::Event::NavigationFailed(navigation));
        }
    }

    pub(crate) fn try_paste(self: &Arc<Self>) -> bool {
        let Some(p) = platform_ops::ops() else {
            return false;
        };
        if !p.web_paste_reads_clipboard() {
            return false;
        }
        let inner = Arc::clone(self);
        p.clipboard_read_text_async(Box::new(move |text| {
            let Some(text) = text.filter(|t| !t.is_empty()) else {
                return;
            };
            tasks::post_paste_js(Arc::clone(&inner), text.to_owned());
        }));
        true
    }
}
