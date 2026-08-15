use std::ffi::c_int;

use jfn_platform_abi::{OsrPopupSurface, PaintFrame, Presented, SurfaceHandle};

use crate::render::Part;

pub(crate) struct WinOsrPopup;

impl OsrPopupSurface for WinOsrPopup {
    fn show(&self, s: SurfaceHandle, x: c_int, y: c_int, _lw: c_int, _lh: c_int) {
        crate::render::popup_show(s, x, y);
    }

    fn hide(&self, s: SurfaceHandle) {
        crate::render::popup_hide(s);
    }

    fn present<'a>(
        &self,
        s: SurfaceHandle,
        frame: PaintFrame<'a>,
        _lw: c_int,
        _lh: c_int,
    ) -> Result<Presented, PaintFrame<'a>> {
        crate::render::present(s, Part::Popup, frame)
    }
}
