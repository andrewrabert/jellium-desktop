use std::ffi::c_int;

use crate::{PaintFrame, SurfaceHandle};

pub trait OsrPopupSurface: Send + Sync {
    fn show(&self, _s: SurfaceHandle, _x: c_int, _y: c_int, _lw: c_int, _lh: c_int) {}

    fn hide(&self, _s: SurfaceHandle) {}

    /// `lw`/`lh` are the parent layer's logical size; the frame carries its own
    /// extent.
    fn present(&self, _s: SurfaceHandle, _frame: PaintFrame<'_>, _lw: c_int, _lh: c_int) {}
}

pub struct NoOsrPopup;

impl OsrPopupSurface for NoOsrPopup {}
