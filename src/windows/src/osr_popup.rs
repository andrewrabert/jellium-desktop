use std::ffi::c_int;

use jfn_platform_abi::{OsrPopupSurface, PaintFrame, SurfaceHandle};

use crate::compositor::{
    win_popup_hide, win_popup_present, win_popup_present_software, win_popup_show,
};

pub(crate) struct WinOsrPopup;

impl OsrPopupSurface for WinOsrPopup {
    fn show(&self, s: SurfaceHandle, x: c_int, y: c_int, _lw: c_int, _lh: c_int) {
        win_popup_show(s.as_ptr(), x, y);
    }

    fn hide(&self, s: SurfaceHandle) {
        win_popup_hide(s.as_ptr());
    }

    fn present(&self, s: SurfaceHandle, frame: PaintFrame<'_>, lw: c_int, lh: c_int) {
        match frame {
            PaintFrame::Accelerated(tex) => win_popup_present(s.as_ptr(), &tex, lw, lh),
            PaintFrame::Software { size, pixels, .. } => {
                win_popup_present_software(s.as_ptr(), pixels, size.w, size.h, lw, lh);
            }
        }
    }
}
