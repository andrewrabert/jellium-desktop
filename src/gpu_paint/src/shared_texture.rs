//! A CEF accelerated-paint texture, selected statically per platform.
//!
//! CEF reclaims the resources backing a frame when the paint callback returns.
//! The Linux constructor therefore receives already-duplicated plane fds;
//! Windows and macOS consume their borrowed handles inline during the callback.

/// Integer texture extent used by the paint crate without depending on the
/// platform ABI's geometry types.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FrameSize {
    pub w: i32,
    pub h: i32,
}

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
    coded: FrameSize,
    visible_rect: FrameSize,
    format: DmabufFormat,
    modifier: u64,
    planes: Vec<DmabufPlane>,
}

#[cfg(target_os = "linux")]
impl SharedTexture {
    /// `planes` must already own their fds — see the module docs.
    pub fn new(
        coded: FrameSize,
        visible_rect: FrameSize,
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

    pub fn coded(&self) -> FrameSize {
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

    pub fn visible_rect(&self) -> FrameSize {
        self.visible_rect
    }

    pub fn visible(&self) -> FrameSize {
        visible_or_coded(self.visible_rect, self.coded)
    }
}

#[cfg(windows)]
pub struct SharedTexture {
    handle: *mut std::ffi::c_void,
    coded: FrameSize,
    visible_rect: FrameSize,
}

#[cfg(windows)]
impl SharedTexture {
    pub fn new(handle: *mut std::ffi::c_void, coded: FrameSize, visible_rect: FrameSize) -> Self {
        Self {
            handle,
            coded,
            visible_rect,
        }
    }

    pub fn handle(&self) -> *mut std::ffi::c_void {
        self.handle
    }

    pub fn coded(&self) -> FrameSize {
        self.coded
    }

    pub fn visible_rect(&self) -> FrameSize {
        self.visible_rect
    }

    pub fn visible(&self) -> FrameSize {
        visible_or_coded(self.visible_rect, self.coded)
    }
}

#[cfg(target_os = "macos")]
pub struct SharedTexture {
    io_surface: *mut std::ffi::c_void,
    coded: FrameSize,
    visible_rect: FrameSize,
}

#[cfg(target_os = "macos")]
impl SharedTexture {
    pub fn new(
        io_surface: *mut std::ffi::c_void,
        coded: FrameSize,
        visible_rect: FrameSize,
    ) -> Self {
        Self {
            io_surface,
            coded,
            visible_rect,
        }
    }

    pub fn io_surface(&self) -> *mut std::ffi::c_void {
        self.io_surface
    }

    pub fn coded(&self) -> FrameSize {
        self.coded
    }

    pub fn visible_rect(&self) -> FrameSize {
        self.visible_rect
    }

    pub fn visible(&self) -> FrameSize {
        visible_or_coded(self.visible_rect, self.coded)
    }
}

fn visible_or_coded(visible_rect: FrameSize, coded: FrameSize) -> FrameSize {
    if visible_rect.w > 0 && visible_rect.h > 0 {
        visible_rect
    } else {
        coded
    }
}
