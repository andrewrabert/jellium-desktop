//! Windows [`MpvHost`]: pre-create environment only.

use jfn_platform_abi::{MpvHost, WindowDecorations};

pub(crate) struct WindowsMpvHost;

impl MpvHost for WindowsMpvHost {
    fn prepare(&self, _configured: Option<WindowDecorations>) {
        unsafe {
            std::env::set_var("MPV_WINDOW_ICON", "IDI_ICON1");
        }
    }
}
