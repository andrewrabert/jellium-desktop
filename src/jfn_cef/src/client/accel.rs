use cef::AcceleratedPaintInfo;

use crate::platform_ops::{FrameSize, SharedTexture};

#[cfg(target_os = "linux")]
pub(crate) fn acquire(info: &AcceleratedPaintInfo) -> Option<SharedTexture> {
    use std::os::fd::BorrowedFd;

    use crate::platform_ops::{DmabufFormat, DmabufPlane};

    let format = match info.format.into() {
        cef::sys::cef_color_type_t::CEF_COLOR_TYPE_BGRA_8888 => DmabufFormat::Bgra8,
        cef::sys::cef_color_type_t::CEF_COLOR_TYPE_RGBA_8888 => DmabufFormat::Rgba8,
        _ => return None,
    };
    let coded = FrameSize {
        w: info.extra.coded_size.width,
        h: info.extra.coded_size.height,
    };
    if coded.w <= 0 || coded.h <= 0 {
        return None;
    }
    let n = info.plane_count.clamp(0, info.planes.len() as i32) as usize;
    if n < 1 {
        return None;
    }
    let mut planes = Vec::with_capacity(n);
    for p in &info.planes[..n] {
        let borrowed = unsafe { BorrowedFd::borrow_raw(p.fd) };
        planes.push(DmabufPlane {
            fd: nix::unistd::dup(borrowed).ok()?,
            offset: p.offset,
            stride: p.stride,
        });
    }
    Some(SharedTexture::new(
        coded,
        FrameSize {
            w: info.extra.visible_rect.width.max(0),
            h: info.extra.visible_rect.height.max(0),
        },
        format,
        info.modifier,
        planes,
    ))
}

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

#[cfg(not(target_os = "linux"))]
fn extents(info: &AcceleratedPaintInfo) -> Option<(FrameSize, FrameSize)> {
    let coded = FrameSize {
        w: info.extra.coded_size.width,
        h: info.extra.coded_size.height,
    };
    if coded.w <= 0 || coded.h <= 0 {
        return None;
    }
    let visible_rect = FrameSize {
        w: info.extra.visible_rect.width.max(0),
        h: info.extra.visible_rect.height.max(0),
    };
    Some((coded, visible_rect))
}
