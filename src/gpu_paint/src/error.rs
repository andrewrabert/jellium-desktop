use crate::FrameSize;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct SurfaceLost(Kind);

#[derive(Debug, Error)]
pub(crate) enum Kind {
    #[error("no usable adapter available")]
    NoAdapter,
    #[error("device request failed: {0}")]
    DeviceRequest(#[from] wgpu::RequestDeviceError),
    #[error("surface creation failed: {0}")]
    SurfaceCreate(#[from] wgpu::CreateSurfaceError),
    #[error("adapter does not support requested surface")]
    SurfaceUnsupported,
    #[error("swapchain acquire failed: {0}")]
    Acquire(&'static str),
    #[error("invalid frame dimensions: {}x{}", .0.w, .0.h)]
    BadDimensions(FrameSize),
    #[error(
        "frame buffer does not cover {}x{} at stride {stride}: {len} bytes",
        .size.w, .size.h
    )]
    BadPixelBuffer {
        size: FrameSize,
        stride: u32,
        len: usize,
    },
}

impl<E: Into<Kind>> From<E> for SurfaceLost {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
