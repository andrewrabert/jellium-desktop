//! A GPU texture the app owns, handed over by CEF's accelerated paint path.
//!
//! The contents are per-platform because CEF's own struct is: dmabuf planes on
//! Linux, a shared NT handle on Windows, an `IOSurface` on macOS. What is
//! common is the ownership rule — CEF reclaims its resources when the paint
//! callback returns, so anything that outlives the callback must be acquired
//! before then. Acquiring is `jfn_cef`'s job; by the time a value of this type
//! exists, that has already happened and everything here is safe to use.

use crate::PhysicalSize;

#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;

#[cfg(target_os = "linux")]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DmabufFormat {
    Bgra8,
    Rgba8,
}

/// One plane of a dmabuf. Owns its fd, closed on drop; an importer that needs
/// to hand the fd to a driver consumes a dup of it.
#[cfg(target_os = "linux")]
pub struct DmabufPlane {
    pub fd: OwnedFd,
    pub offset: u64,
    pub stride: u32,
}

#[cfg(target_os = "linux")]
pub struct SharedTexture {
    coded: PhysicalSize,
    visible_rect: PhysicalSize,
    format: DmabufFormat,
    modifier: u64,
    planes: Vec<DmabufPlane>,
}

#[cfg(target_os = "linux")]
impl SharedTexture {
    /// `planes` must already own their fds — see the module docs.
    pub fn new(
        coded: PhysicalSize,
        visible_rect: PhysicalSize,
        format: DmabufFormat,
        modifier: u64,
        planes: Vec<DmabufPlane>,
    ) -> Self {
        Self {
            coded,
            visible_rect,
            format,
            modifier,
            planes,
        }
    }

    pub fn coded(&self) -> PhysicalSize {
        self.coded
    }

    pub fn format(&self) -> DmabufFormat {
        self.format
    }

    pub fn modifier(&self) -> u64 {
        self.modifier
    }

    pub fn planes(&self) -> &[DmabufPlane] {
        &self.planes
    }

    /// CEF's visible rect exactly as given — may be zero when it supplied none.
    /// Prefer [`Self::visible`] unless you specifically need the raw value.
    pub fn visible_rect(&self) -> PhysicalSize {
        self.visible_rect
    }

    /// The extent to gate and damage against: CEF's visible rect when it gave
    /// one, else the coded size, which may be padded larger.
    pub fn visible(&self) -> PhysicalSize {
        if self.visible_rect.w > 0 && self.visible_rect.h > 0 {
            self.visible_rect
        } else {
            self.coded
        }
    }
}

#[cfg(windows)]
pub struct SharedTexture {
    handle: *mut std::ffi::c_void,
}

#[cfg(windows)]
impl SharedTexture {
    pub fn new(handle: *mut std::ffi::c_void) -> Self {
        Self { handle }
    }

    /// The D3D11 shared NT handle. Valid only for the duration of the paint
    /// callback; the compositor opens it and uses it inline.
    pub fn handle(&self) -> *mut std::ffi::c_void {
        self.handle
    }
}

#[cfg(target_os = "macos")]
pub struct SharedTexture {
    io_surface: *mut std::ffi::c_void,
}

#[cfg(target_os = "macos")]
impl SharedTexture {
    pub fn new(io_surface: *mut std::ffi::c_void) -> Self {
        Self { io_surface }
    }

    /// The `IOSurfaceRef`. Unretained — valid only for the duration of the
    /// paint callback, so the compositor must not cache it past that.
    pub fn io_surface(&self) -> *mut std::ffi::c_void {
        self.io_surface
    }
}
