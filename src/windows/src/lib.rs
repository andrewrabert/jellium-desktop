//! Windows `Platform` backend.

pub mod scale;

#[cfg(target_os = "windows")]
mod backend;
#[cfg(target_os = "windows")]
mod input;
#[cfg(target_os = "windows")]
mod menu;
#[cfg(target_os = "windows")]
mod mpv_host;
#[cfg(target_os = "windows")]
mod osr_popup;
#[cfg(target_os = "windows")]
mod platform;
#[cfg(target_os = "windows")]
mod process;
#[cfg(target_os = "windows")]
mod render;
#[cfg(target_os = "windows")]
mod window;

#[cfg(target_os = "windows")]
pub use backend::*;
