//! The CEF ABI boundary for accelerated-paint frames.
//!
//! CEF releases the resources backing a frame to its internal pool when the
//! paint callback returns, so anything that outlives the callback — X11 and
//! Wayland both hand the frame to an actor thread — must be acquired before
//! then. This module is the one place that acquisition happens; downstream the
//! frame is an owned [`SharedTexture`] and everything is safe code.

use cef::AcceleratedPaintInfo;

use crate::platform_ops::{PhysicalSize, SharedTexture};

/// Take ownership of one accelerated-paint frame. `None` when it is unusable.
///
/// Call this only once the frame is going to be presented — it costs a `dup`
/// per plane on Linux, and the callers ahead of it drop frames.
#[cfg(target_os = "linux")]
pub(crate) fn acquire(info: &AcceleratedPaintInfo) -> Option<SharedTexture> {
    use std::os::fd::BorrowedFd;

    use crate::platform_ops::{DmabufFormat, DmabufPlane};

    let format = match info.format.into() {
        cef::sys::cef_color_type_t::CEF_COLOR_TYPE_BGRA_8888 => DmabufFormat::Bgra8,
        cef::sys::cef_color_type_t::CEF_COLOR_TYPE_RGBA_8888 => DmabufFormat::Rgba8,
        _ => return None,
    };
    let coded = PhysicalSize {
        w: info.extra.coded_size.width,
        h: info.extra.coded_size.height,
    };
    if coded.w <= 0 || coded.h <= 0 {
        return None;
    }
    // Include every memory plane the modifier uses; DCC/CCS modifiers add an
    // auxiliary plane beyond the color plane.
    let n = info.plane_count.clamp(0, info.planes.len() as i32) as usize;
    if n < 1 {
        return None;
    }
    let mut planes = Vec::with_capacity(n);
    for p in &info.planes[..n] {
        // SAFETY: `p.fd` is a live dmabuf fd for the duration of this
        // callback; CEF reclaims it when we return, which is why we dup.
        let borrowed = unsafe { BorrowedFd::borrow_raw(p.fd) };
        planes.push(DmabufPlane {
            fd: nix::unistd::dup(borrowed).ok()?,
            offset: p.offset,
            stride: p.stride,
        });
    }
    Some(SharedTexture::new(
        coded,
        PhysicalSize {
            w: info.extra.visible_rect.width.max(0),
            h: info.extra.visible_rect.height.max(0),
        },
        format,
        info.modifier,
        planes,
    ))
}

/// The compositor opens the shared handle inline, within this callback, so
/// there is nothing to acquire.
#[cfg(windows)]
pub(crate) fn acquire(info: &AcceleratedPaintInfo) -> Option<SharedTexture> {
    if info.shared_texture_handle.is_null() {
        return None;
    }
    let (coded, visible_rect) = extents(info)?;
    Some(SharedTexture::new(
        info.shared_texture_handle,
        coded,
        visible_rect,
    ))
}

/// The compositor wraps the `IOSurface` in an `MTLTexture` inline, within this
/// callback, so there is nothing to acquire.
#[cfg(target_os = "macos")]
pub(crate) fn acquire(info: &AcceleratedPaintInfo) -> Option<SharedTexture> {
    if info.shared_texture_io_surface.is_null() {
        return None;
    }
    let (coded, visible_rect) = extents(info)?;
    Some(SharedTexture::new(
        info.shared_texture_io_surface,
        coded,
        visible_rect,
    ))
}

/// The pair CEF states about every frame, on the platforms whose payload does
/// not carry its own size. `None` when the coded size is not presentable.
#[cfg(not(target_os = "linux"))]
fn extents(info: &AcceleratedPaintInfo) -> Option<(PhysicalSize, PhysicalSize)> {
    let coded = PhysicalSize {
        w: info.extra.coded_size.width,
        h: info.extra.coded_size.height,
    };
    if coded.w <= 0 || coded.h <= 0 {
        return None;
    }
    let visible_rect = PhysicalSize {
        w: info.extra.visible_rect.width.max(0),
        h: info.extra.visible_rect.height.max(0),
    };
    Some((coded, visible_rect))
}
