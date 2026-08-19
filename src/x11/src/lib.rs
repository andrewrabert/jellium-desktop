//! X11 platform subsystem: surface management, input thread, Platform impl.

#![cfg(target_os = "linux")]

pub(crate) mod conn_source;
pub mod geometry;
pub(crate) mod input;
pub(crate) mod input_lifecycle;
pub mod lifecycle;
pub mod make_platform;
pub(crate) mod menu;
pub(crate) mod mpv_host;
pub(crate) mod mpv_proxy;
pub(crate) mod overlay_actor;
pub mod overlay_fsm;
pub(crate) mod paint;
pub mod paint_override;
pub(crate) mod registry;
pub(crate) mod scale;
pub(crate) mod selection;
pub mod shm;
pub mod surface;
pub(crate) mod window_source;
pub(crate) mod x11_state;

pub use paint_override::{X11PaintOverride, paint_override, set_paint_override};
