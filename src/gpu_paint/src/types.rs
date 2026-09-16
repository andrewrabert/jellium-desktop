use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::Instant;

use crate::FrameSize;

pub enum WindowTarget {
    Xcb {
        connection: NonNull<c_void>,
        window: u32,
        screen: i32,
        visual: u32,
    },
    Wayland {
        display: NonNull<c_void>,
        surface: NonNull<c_void>,
    },
    CompositionVisual {
        visual: NonNull<c_void>,
    },
    CoreAnimationLayer {
        layer: NonNull<c_void>,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum PaintMode {
    Shared,
    Copied,
}

#[derive(Clone, Copy, Debug)]
pub struct Presented(());

impl Presented {
    pub fn issued() -> Presented {
        Presented(())
    }
}

#[must_use = "the retry this names must be scheduled"]
#[derive(Debug)]
pub struct Deferred(());

impl Deferred {
    pub fn new() -> Deferred {
        Deferred(())
    }

    pub fn retry_at(self) -> Option<Instant> {
        crate::refresh::refresh_interval().map(|interval| Instant::now() + interval)
    }
}

impl Default for Deferred {
    fn default() -> Deferred {
        Deferred::new()
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl DamageRect {
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
