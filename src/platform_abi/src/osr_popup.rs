use std::ffi::c_int;

use crate::{PaintFrame, Presented, SurfaceHandle};

pub trait OsrPopupSurface: Send + Sync {
    fn show(&self, _s: SurfaceHandle, _x: c_int, _y: c_int, _lw: c_int, _lh: c_int) {}

    fn hide(&self, _s: SurfaceHandle) {}

    /// `lw`/`lh` are the parent layer's logical size; the frame carries its own
    /// extent. A backend with no popup surface hands the frame back
    /// undischarged, so its producer owes the successor.
    fn present<'a>(
        &self,
        s: SurfaceHandle,
        frame: PaintFrame<'a>,
        lw: c_int,
        lh: c_int,
    ) -> Result<Presented, PaintFrame<'a>> {
        let _ = (s, lw, lh);
        Err(frame)
    }
}

pub struct NoOsrPopup;

impl OsrPopupSurface for NoOsrPopup {}
