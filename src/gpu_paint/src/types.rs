use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::Instant;

use crate::FrameSize;

/// Where a [`crate::Surface`] attaches its swapchain. One variant per window
/// system; new platforms are added here rather than by reshaping the API.
///
/// The variant also fixes the size policy — see [`crate::painter::SizePolicy`]
/// — because whether the swapchain *is* the window is a property of the target,
/// not a caller preference.
pub enum WindowTarget {
    /// X11 (xcb) — `connection` is an `xcb_connection_t*`, `window` is the XID.
    /// `visual` is the ARGB visual ID. `screen` is the screen index.
    Xcb {
        connection: NonNull<c_void>,
        window: u32,
        screen: i32,
        visual: u32,
    },
    /// Wayland — `display` is `wl_display*`, `surface` is `wl_surface*`. The
    /// swapchain owns every buffer this surface carries: no other client code
    /// attaches one. The surface's owner still commits it with no buffer to
    /// empty it, and blocks on that commit's acknowledgement before returning,
    /// so such a commit never interleaves with a present.
    Wayland {
        display: NonNull<c_void>,
        surface: NonNull<c_void>,
    },
    /// Windows — `visual` is an `IDCompositionVisual*`. wgpu binds its
    /// swapchain to the visual inside `configure` and nowhere else, which is
    /// what [`crate::Surface::content_detached`] exists for; the app keeps
    /// ownership of the visual and its tree.
    CompositionVisual { visual: NonNull<c_void> },
    /// macOS — `layer` is a `CAMetalLayer*`. Configuring the surface *is* the
    /// layer mutation (wgpu writes device, format, colorspace, drawable size
    /// and more), so it belongs to the layer's owner thread.
    CoreAnimationLayer { layer: NonNull<c_void> },
}

/// Which kind of frame a surface carries. Latched from the first frame and
/// never public: callers say what a frame *is* by picking the present method
/// they call, which is the same information.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum PaintMode {
    /// Frames arrive as a platform shared-buffer handle.
    Shared,
    /// Frames arrive as CPU pixels, uploaded here.
    Copied,
}

/// Proof that a frame entered its surface's commit stream, minted at the site
/// that issued the commit.
#[derive(Clone, Copy, Debug)]
pub struct Presented(());

impl Presented {
    pub fn issued() -> Presented {
        Presented(())
    }
}

/// A swapchain that had no texture this cycle: occluded, timed out, or stale
/// where the calling thread may not configure it.
///
/// It carries the instant the swapchain may be asked again, so the caller
/// schedules the retry that succeeds this attempt instead of spinning on it.
#[must_use = "the retry this names must be scheduled"]
#[derive(Debug)]
pub struct Deferred(());

impl Deferred {
    pub fn new() -> Deferred {
        Deferred(())
    }

    /// One refresh interval from now, or `None` where the display reports no
    /// refresh — then the caller asks its frame source for the wake instead.
    ///
    /// Sooner than one interval is a spin on a compositor that has not moved,
    /// later is a visibly late frame.
    pub fn retry_at(self) -> Option<Instant> {
        crate::refresh::refresh_interval().map(|interval| Instant::now() + interval)
    }
}

impl Default for Deferred {
    fn default() -> Deferred {
        Deferred::new()
    }
}

/// A borrowed CPU frame plus the regions that changed since the last one.
/// `stride` is in bytes.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl DamageRect {
    /// The part of the rect inside a `width` x `height` buffer; `None` when
    /// nothing is. Clamped in i64 so a producer-supplied `x + w` / `y + h`
    /// cannot overflow i32.
    pub fn clamped(self, width: i32, height: i32) -> Option<DamageRect> {
        let x0 = i64::from(self.x).max(0);
        let y0 = i64::from(self.y).max(0);
        let x1 = (i64::from(self.x) + i64::from(self.w)).min(i64::from(width));
        let y1 = (i64::from(self.y) + i64::from(self.h)).min(i64::from(height));
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(DamageRect {
            x: x0 as i32,
            y: y0 as i32,
            w: (x1 - x0) as i32,
            h: (y1 - y0) as i32,
        })
    }
}

/// `bgra` must cover every row: at least `(size.h - 1) * stride + size.w * 4`
/// bytes, with `stride >= size.w * 4`. [`crate::Surface::present_pixels`]
/// rejects a frame that does not (as an error, not a panic), so a producer
/// bug cannot
/// read out of bounds.
pub struct Pixels<'a> {
    pub size: FrameSize,
    pub stride: u32,
    pub bgra: &'a [u8],
    pub dirty: &'a [DamageRect],
}

#[cfg(test)]
mod tests {
    use super::DamageRect;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> DamageRect {
        DamageRect { x, y, w, h }
    }

    #[test]
    fn clamped_clamps_negative_origin() {
        assert_eq!(rect(-2, -2, 4, 4).clamped(10, 10), Some(rect(0, 0, 2, 2)));
    }

    #[test]
    fn clamped_clamps_overflow() {
        assert_eq!(rect(8, 8, 10, 10).clamped(10, 10), Some(rect(8, 8, 2, 2)));
    }

    #[test]
    fn clamped_rejects_zero_and_off_screen() {
        assert_eq!(rect(0, 0, 0, 5).clamped(10, 10), None);
        assert_eq!(rect(10, 0, 4, 4).clamped(10, 10), None);
    }

    #[test]
    fn clamped_passes_through_in_bounds() {
        assert_eq!(rect(1, 2, 3, 4).clamped(10, 10), Some(rect(1, 2, 3, 4)));
    }

    #[test]
    fn clamped_survives_extreme_extent() {
        assert_eq!(
            rect(1, 1, i32::MAX, i32::MAX).clamped(10, 10),
            Some(rect(1, 1, 9, 9))
        );
    }
}
