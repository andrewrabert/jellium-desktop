//! Importing CEF's shared-texture frames, and the device selection that makes
//! importing possible at all.
//!
//! One implementation per platform behind one interface: a dmabuf on Linux, a
//! shared NT handle on Windows, an `IOSurface` on macOS. What the platforms
//! share is the shape — name the device CEF produces on, open a device that can
//! take its buffers, and turn one frame into a `wgpu::Texture`.

#[cfg(target_os = "linux")]
#[path = "vulkan.rs"]
mod backend;

#[cfg(windows)]
#[path = "dx12.rs"]
mod backend;

#[cfg(target_os = "macos")]
#[path = "metal.rs"]
mod backend;

pub use backend::ProducerId;
#[cfg(windows)]
pub(crate) use backend::adapter_luid;
pub(crate) use backend::{Importer, acquire_barrier, adapter_matches, open_device};

/// A shared-texture import that did not work.
///
/// Deliberately not a [`crate::SurfaceLost`]: a shared frame has no CPU pixels,
/// so there is nothing to fall back to. The frame is logged and dropped and the
/// surface stays usable, which is why this never reaches a caller.
#[derive(Debug, thiserror::Error)]
#[error("shared-texture import failed: {0}")]
pub(crate) struct ImportFailed(pub(crate) &'static str);

/// One imported frame, ready to sample.
pub(crate) struct Imported {
    pub(crate) texture: wgpu::Texture,
    /// The raw image handle [`acquire_barrier`] needs, where the platform has
    /// a queue-family transfer to do. `None` where it does not.
    pub(crate) acquire: Option<u64>,
    /// `texture` is the importer's cached wrapper from an earlier frame, so
    /// anything the painter built over it (its bind group) is reusable too.
    pub(crate) reused: bool,
}

/// A device opened for painting, plus whether it is capable of importing at
/// all — the consumer half of the shared-texture question, before it is ANDed
/// with whether this is the adapter CEF produces on.
pub(crate) struct Opened {
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) import_capable: bool,
}
