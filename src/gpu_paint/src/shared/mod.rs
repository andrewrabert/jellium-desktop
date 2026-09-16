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

#[derive(Debug, thiserror::Error)]
#[error("shared-texture import failed: {0}")]
pub(crate) struct ImportFailed(pub(crate) &'static str);

pub(crate) struct Imported {
    pub(crate) texture: wgpu::Texture,
    pub(crate) acquire: Option<u64>,
    pub(crate) reused: bool,
}

pub(crate) struct Opened {
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) import_capable: bool,
}
