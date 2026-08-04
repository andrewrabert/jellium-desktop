use std::sync::atomic::Ordering;

use super::{Inner, platform_ops};
use crate::platform_ops::{PaintFrame, PhysicalSize};

/// Borrow CEF's `OnPaint` buffer as pixels. `None` when the frame is unusable.
///
/// The buffer is the last raw thing on this path: CEF hands over a pointer with
/// no length, and the size is only knowable from `w`/`h` — it is tightly packed
/// BGRA.
fn software_frame<'a>(
    buffer: *const u8,
    w: i32,
    h: i32,
    dirty: &'a [platform_ops::JfnRect],
) -> Option<PaintFrame<'a>> {
    if buffer.is_null() || w <= 0 || h <= 0 {
        return None;
    }
    let len = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    // SAFETY: CEF guarantees `buffer` covers `w * h * 4` bytes for the
    // duration of this callback.
    let pixels = unsafe { std::slice::from_raw_parts(buffer, len) };
    Some(PaintFrame::Software {
        size: PhysicalSize { w, h },
        pixels,
        dirty,
    })
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
        &self,
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
        if is_popup {
            let (pw, ph) = self.popup_rect();
            let Some(frame) = software_frame(buffer, w, h, &[]) else {
                return;
            };
            if matches!(self.dropdown, crate::platform_ops::MenuDelivery::Composited) {
                jfn_platform_abi::get()
                    .osr_popup_surface()
                    .present(surface, frame, pw, ph);
            }
            return;
        }
        let Some(p) = platform_ops::ops() else { return };
        if !self.should_present_paint() {
            return;
        }
        let Some(frame) = software_frame(buffer, w, h, dirty) else {
            return;
        };
        p.surface_present(surface, frame);
    }

    pub(crate) fn on_accelerated_paint(&self, is_popup: bool, info: &cef::AcceleratedPaintInfo) {
        let surface = self.surface_handle();
        if surface.is_none() {
            return;
        }
        if is_popup {
            let (pw, ph) = self.popup_rect();
            let Some(tex) = super::accel::acquire(info) else {
                return;
            };
            if matches!(self.dropdown, crate::platform_ops::MenuDelivery::Composited) {
                jfn_platform_abi::get().osr_popup_surface().present(
                    surface,
                    PaintFrame::Accelerated(tex),
                    pw,
                    ph,
                );
            }
            return;
        }
        let Some(p) = platform_ops::ops() else { return };
        if !self.should_present_paint() {
            return;
        }
        // Acquire last: this dups a fd per plane, and every gate above drops
        // frames.
        let Some(tex) = super::accel::acquire(info) else {
            return;
        };
        p.surface_present(surface, PaintFrame::Accelerated(tex));
    }

    fn should_present_paint(&self) -> bool {
        self.paint_scheduler.should_present_paint(self)
    }
}
