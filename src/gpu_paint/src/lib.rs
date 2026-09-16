#![deny(clippy::let_underscore_must_use)]

mod context;
mod error;
mod painter;
pub mod refresh;
mod shared;
mod shared_texture;
mod swapchain;
mod types;

pub use context::{FORMAT, Surfaces, any_adapter, surfaces};
pub use error::SurfaceLost;
pub use painter::{PresentFailed, Surface};
pub use refresh::{
    RefreshRate, RefreshSource, current_refresh_rate, refresh_interval, report_refresh,
};
pub use shared::ProducerId;
#[cfg(target_os = "linux")]
pub use shared_texture::{DmabufFormat, DmabufPlane};
pub use shared_texture::{FrameSize, SharedTexture};
pub use swapchain::{Acquired, Frame, Swapchain};
pub use types::{DamageRect, Deferred, Pixels, Presented, WindowTarget};
pub use wgpu;
