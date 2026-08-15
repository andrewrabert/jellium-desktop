use crate::FrameSize;
use thiserror::Error;

/// This surface can no longer present; the caller should abandon it.
///
/// Its *existence* is the whole signal — there is no severity to interrogate,
/// which is why it is opaque. Anything recoverable comes back as another
/// [`crate::PresentFailed`] arm, each naming what its producer still owes: a
/// stale, occluded or timed-out swapchain, and a shared-texture import that
/// failed.
///
/// The detail below exists for the log line, and reaches callers only through
/// `Display`.
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
