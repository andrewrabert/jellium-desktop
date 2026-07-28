use std::ffi::c_int;

use jfn_platform_abi::{DropdownBackend, JfnPopupRequest, PaintFrame, SurfaceHandle};

use crate::wl_ops;

pub(super) struct SubsurfaceDropdown;

impl DropdownBackend for SubsurfaceDropdown {
    fn show(&self, s: SurfaceHandle, req: JfnPopupRequest) {
        wl_ops::popup_show(
            s.as_ptr() as *mut crate::wl_state::PlatformSurface,
            req.x,
            req.y,
            req.lw,
            req.lh,
        );
    }

    fn hide(&self, s: SurfaceHandle) {
        wl_ops::popup_hide(s.as_ptr() as *mut crate::wl_state::PlatformSurface);
    }

    fn present(&self, s: SurfaceHandle, frame: PaintFrame<'_>, lw: c_int, lh: c_int) {
        let ptr = s.as_ptr() as *mut crate::wl_state::PlatformSurface;
        match frame {
            PaintFrame::Accelerated(tex) => wl_ops::popup_present(ptr, &tex, lw, lh),
            PaintFrame::Software { size, pixels, .. } => {
                wl_ops::popup_present_software(ptr, pixels, size.w, size.h, lw, lh);
            }
        }
    }
}
