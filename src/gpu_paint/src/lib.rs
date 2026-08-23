//! The only place in the process that knows wgpu exists.
//!
//! Everything wgpu forces you to decide, this crate decides; everything wgpu
//! makes you name — adapters, devices, swapchains, `configure` vs `submit`,
//! `Lost`/`Outdated`/`Suboptimal` — stops here and comes out as app-level
//! answers: *can this device take CEF's shared buffers*, *did this frame
//! present*, *is this failure worth abandoning the surface for*.
//!
//! # What it is not
//!
//! The window-system upload paths (X11 MIT-SHM, `wl_shm`, attaching a dmabuf
//! straight to a `wl_surface`) are not wgpu and are not here. From CEF's side
//! there are only two output paths — `OnPaint` and `OnAcceleratedPaint`,
//! selected once per browser by `shared_texture_enabled` — and whether CPU
//! pixels then land via wgpu or via the compositor is invisible to it. Which
//! also means the fallback belongs to the caller: this crate reports whether a
//! failure lost the surface ([`SurfaceLost`]) and the backend decides where to
//! go next.
//!
//! Creating windows, restacking them, placing them, and owning the threads that
//! drive them all stay with the backends too.

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
