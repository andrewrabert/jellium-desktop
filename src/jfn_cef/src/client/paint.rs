use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::Inner;
use crate::paint_scheduler::Verdict;
use crate::platform_ops::{PaintFrame, PhysicalSize, Superseded};

/// Borrow CEF's `OnPaint` buffer as pixels. `None` when the frame is unusable.
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

    /// The scale and logical view size CEF's `GetScreenInfo` answers with, or
    /// `None` before a size has been applied.
    pub(crate) fn screen_info_values(&self) -> Option<(jfn_platform_abi::Scale, i32, i32)> {
        let scale = self.scale.load()?;
        Some((
            scale,
            self.width.load(Ordering::Acquire),
            self.height.load(Ordering::Acquire),
        ))
    }

    pub(crate) fn on_paint(
        self: &Arc<Self>,
        is_popup: bool,
        dirty: &[jfn_platform_abi::JfnRect],
        buffer: *const u8,
        w: i32,
        h: i32,
    ) {
        let Some(pixels) = software_pixels(buffer, w, h) else {
            return;
        };
        let size = PhysicalSize { w, h };
        if is_popup {
            if !matches!(self.dropdown, crate::platform_ops::MenuDelivery::Composited) {
                return;
            }
            let (popup_width, popup_height) = self.popup_rect();
            let frame = PaintFrame::software(self.frame_source(), size, pixels, &[]);
            let _: Superseded = match self
                .surface()
                .popup_present(frame, popup_width, popup_height)
            {
                Ok(_presented) => return,
                Err(frame) => frame.supersede(),
            };
            return;
        }
        let navigation = self.witness_navigation();
        let frame = PaintFrame::software(self.frame_source(), size, pixels, dirty);
        let _: Superseded = match self.paint_scheduler.verdict(self) {
            Verdict::Supersede => frame.supersede(),
            Verdict::Present => match self.surface().present(frame) {
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
        if is_popup {
            if !matches!(self.dropdown, crate::platform_ops::MenuDelivery::Composited) {
                return;
            }
            let (popup_width, popup_height) = self.popup_rect();
            // Acquire last: this dups a fd per plane, and every gate above drops
            // frames.
            let Some(texture) = super::accel::acquire(info) else {
                return;
            };
            let frame = PaintFrame::accelerated(self.frame_source(), texture);
            let _: Superseded = match self
                .surface()
                .popup_present(frame, popup_width, popup_height)
            {
                Ok(_presented) => return,
                Err(frame) => frame.supersede(),
            };
            return;
        }
        // Acquire last: this dups a fd per plane, and every gate above drops
        // frames.
        let Some(texture) = super::accel::acquire(info) else {
            return;
        };
        let navigation = self.witness_navigation();
        let frame = PaintFrame::accelerated(self.frame_source(), texture);
        let _: Superseded = match self.paint_scheduler.verdict(self) {
            Verdict::Supersede => frame.supersede(),
            Verdict::Present => match self.surface().present(frame) {
                Ok(presented) => {
                    witness(navigation, presented);
                    return;
                }
                Err(frame) => frame.supersede(),
            },
        };
    }
}
