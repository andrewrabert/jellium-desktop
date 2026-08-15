use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{Inner, platform_ops};
use crate::paint_scheduler::Verdict;
use crate::platform_ops::{PaintFrame, PhysicalSize, Superseded};

/// Borrow CEF's `OnPaint` buffer as pixels. `None` when the frame is unusable.
///
/// The buffer is the last raw thing on this path: CEF hands over a pointer with
/// no length, and the size is only knowable from `w`/`h` — it is tightly packed
/// BGRA.
fn software_pixels<'a>(buffer: *const u8, w: i32, h: i32) -> Option<&'a [u8]> {
    if buffer.is_null() || w <= 0 || h <= 0 {
        return None;
    }
    let len = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    // SAFETY: CEF guarantees `buffer` covers `w * h * 4` bytes for the
    // duration of this callback.
    Some(unsafe { std::slice::from_raw_parts(buffer, len) })
}

/// A presented frame of the requested document is bring-up's witness. A frame
/// produced before that document finished loading witnesses nothing.
fn witness(navigation: Option<jfn_bringup::Navigation>, presented: jfn_bringup::Presented) {
    if let Some(navigation) = navigation {
        jfn_bringup::advance(jfn_bringup::Event::Operational(
            jfn_bringup::Operational::witnessed(navigation, presented),
        ));
    }
}

impl Inner {
    pub(crate) fn view_size(&self) -> (i32, i32) {
        (
            self.width.load(Ordering::Acquire),
            self.height.load(Ordering::Acquire),
        )
    }

    pub(crate) fn screen_info_values(&self) -> (f32, i32, i32) {
        let w = self.width.load(Ordering::Acquire);
        let h = self.height.load(Ordering::Acquire);
        let pw = self.physical_w.load(Ordering::Acquire);
        let scale = if pw > 0 && w > 0 {
            pw as f32 / w as f32
        } else {
            1.0
        };
        (scale, w, h)
    }

    pub(crate) fn on_paint(
        self: &Arc<Self>,
        is_popup: bool,
        dirty: &[platform_ops::JfnRect],
        buffer: *const u8,
        w: i32,
        h: i32,
    ) {
        let surface = self.surface_handle();
        if surface.is_none() {
            return;
        }
        let Some(pixels) = software_pixels(buffer, w, h) else {
            return;
        };
        let size = PhysicalSize { w, h };
        if is_popup {
            if !matches!(self.dropdown, crate::platform_ops::MenuDelivery::Composited) {
                return;
            }
            let (pw, ph) = self.popup_rect();
            let frame = PaintFrame::software(self.frame_source(), size, pixels, &[]);
            let _: Superseded = match jfn_platform_abi::get()
                .osr_popup_surface()
                .present(surface, frame, pw, ph)
            {
                Ok(_presented) => return,
                Err(frame) => frame.supersede(),
            };
            return;
        }
        let Some(p) = platform_ops::ops() else { return };
        let navigation = self.witness_navigation();
        let frame = PaintFrame::software(self.frame_source(), size, pixels, dirty);
        let _: Superseded = match self.paint_scheduler.verdict(self) {
            Verdict::Supersede => frame.supersede(),
            Verdict::Present => match p.surface_present(surface, frame) {
                Ok(presented) => {
                    witness(navigation, presented);
                    return;
                }
                Err(frame) => frame.supersede(),
            },
        };
    }

    pub(crate) fn on_accelerated_paint(
        self: &Arc<Self>,
        is_popup: bool,
        info: &cef::AcceleratedPaintInfo,
    ) {
        let surface = self.surface_handle();
        if surface.is_none() {
            return;
        }
        if is_popup {
            if !matches!(self.dropdown, crate::platform_ops::MenuDelivery::Composited) {
                return;
            }
            let (pw, ph) = self.popup_rect();
            // Acquire last: this dups a fd per plane, and every gate above drops
            // frames.
            let Some(tex) = super::accel::acquire(info) else {
                return;
            };
            let frame = PaintFrame::accelerated(self.frame_source(), tex);
            let _: Superseded = match jfn_platform_abi::get()
                .osr_popup_surface()
                .present(surface, frame, pw, ph)
            {
                Ok(_presented) => return,
                Err(frame) => frame.supersede(),
            };
            return;
        }
        let Some(p) = platform_ops::ops() else { return };
        // Acquire last: this dups a fd per plane, and every gate above drops
        // frames.
        let Some(tex) = super::accel::acquire(info) else {
            return;
        };
        let navigation = self.witness_navigation();
        let frame = PaintFrame::accelerated(self.frame_source(), tex);
        let _: Superseded = match self.paint_scheduler.verdict(self) {
            Verdict::Supersede => frame.supersede(),
            Verdict::Present => match p.surface_present(surface, frame) {
                Ok(presented) => {
                    witness(navigation, presented);
                    return;
                }
                Err(frame) => frame.supersede(),
            },
        };
    }
}
