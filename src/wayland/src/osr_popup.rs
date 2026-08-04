use std::ffi::c_int;

use jfn_platform_abi::{OsrPopupSurface, PaintFrame, SurfaceHandle};

use crate::runtime::WlRuntime;
use crate::wl_ops;

pub(crate) struct WlSubsurfacePopup {
    pub(crate) rt: &'static WlRuntime,
}

impl OsrPopupSurface for WlSubsurfacePopup {
    fn show(&self, s: SurfaceHandle, x: c_int, y: c_int, lw: c_int, lh: c_int) {
        wl_ops::popup_show(
            self.rt,
            s.as_ptr() as *mut crate::wl_state::PlatformSurface,
            x,
            y,
            lw,
            lh,
        );
    }

    fn hide(&self, s: SurfaceHandle) {
        wl_ops::popup_hide(self.rt, s.as_ptr() as *mut crate::wl_state::PlatformSurface);
    }

    fn present(&self, s: SurfaceHandle, frame: PaintFrame<'_>, lw: c_int, lh: c_int) {
        let ptr = s.as_ptr() as *mut crate::wl_state::PlatformSurface;
        match frame {
            PaintFrame::Accelerated(tex) => wl_ops::popup_present(self.rt, ptr, &tex, lw, lh),
            PaintFrame::Software { size, pixels, .. } => {
                wl_ops::popup_present_software(self.rt, ptr, pixels, size.w, size.h, lw, lh);
            }
        }
    }
}
