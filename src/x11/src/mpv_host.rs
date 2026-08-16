use jfn_platform_abi::{MpvHost, WindowDecorations};

pub struct X11MpvHost;

impl MpvHost for X11MpvHost {
    fn prepare(&self, _configured: Option<WindowDecorations>) {
        // Resolve the paint tier — creating the app's Vulkan instance — before
        // the proxy repoints DISPLAY and before mpv init, so NVIDIA's ICD lazy
        // global init runs against the real server and beats mpv's VO thread to
        // the loader scan. See `crate::paint`.
        crate::paint::resolve_and_store();
        if !crate::mpv_proxy::start() {
            tracing::error!(target: "Main", "x11 mpv proxy failed to start; mpv will connect directly");
        }
    }

    fn ensure_host_window(&self) {
        if !crate::lifecycle::ensure_host_window() {
            tracing::error!(target: "Main", "x11 host window creation failed");
        }
    }

    fn embed_wid(&self) -> Option<i64> {
        crate::x11_state::host().map(|h| i64::from(h.video_host))
    }

    // mpv is embedded and passive here; its `osd-dimensions` is the authority
    // for the size ingest reads
    fn logical_content_size(&self) -> Option<jfn_platform_abi::LogicalSize> {
        None
    }

    fn host_ready(&self) -> bool {
        crate::x11_state::host().is_some()
    }
}
