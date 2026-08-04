//! Windows [`MpvHost`]: pre-create environment only.

use jfn_platform_abi::{MpvHost, WindowDecorations};

pub struct WindowsMpvHost;

impl MpvHost for WindowsMpvHost {
    fn prepare(&self, _configured: Option<WindowDecorations>) {
        // Tell mpv to load the window icon from our exe resources. mpv reads
        // this with GetEnvironmentVariableW, so the Win32 process environment
        // that set_var writes is the one it sees.
        //
        // SAFETY: called from `setup_mpv_environment` on the main thread
        // before any mpv or CEF thread exists.
        unsafe {
            std::env::set_var("MPV_WINDOW_ICON", "IDI_ICON1");
        }
    }
}
