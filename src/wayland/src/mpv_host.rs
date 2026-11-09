//! Wayland [`MpvHost`]: starts the proxy mpv connects to in place of the
//! compositor, and drives the app-owned root window.

use crate::mpv_proxy::start;
use crate::runtime::WlRuntime;
use jfn_platform_abi::{MpvHost, WindowDecorations};

pub struct WaylandMpvHost {
    rt: &'static WlRuntime,
}

impl WaylandMpvHost {
    pub(crate) fn new(rt: &'static WlRuntime) -> Self {
        Self { rt }
    }
}

impl MpvHost for WaylandMpvHost {
    fn prepare(&self, configured: Option<WindowDecorations>) {
        start_proxy(self.rt, configured);
    }

    fn host_ready(&self) -> bool {
        self.rt.window().stated_scale().is_some()
    }

    // the compositor's configure, not the OS, is the authority for this
    // window's size; mpv's `osd-dimensions` is what ingest reads
    fn logical_content_size(&self) -> Option<jfn_platform_abi::LogicalSize> {
        None
    }

    fn ensure_host_window(&self) {
        crate::root_window::ensure_started(self.rt);
    }

    fn detach(&self) {}
}

fn start_proxy(rt: &'static WlRuntime, configured: Option<WindowDecorations>) {
    let Some(proxy) = start(rt) else {
        tracing::error!(target: "Main", "proxy start failed; continuing without proxy");
        return;
    };
    let Ok(proxy) = rt.proxy().set_proxy(proxy) else {
        tracing::error!(target: "Main", "proxy already started");
        return;
    };
    let disp = proxy.display_name().to_string_lossy().into_owned();
    if disp.is_empty() {
        tracing::error!(target: "Main", "proxy display name empty; aborting proxy");
        rt.proxy().stop();
        return;
    }
    tracing::info!(target: "Main", "proxy listening on {disp}");
    rt.root().set_decorations(configured);
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &disp) };
}
