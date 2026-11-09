//! macOS `Platform` backend.

pub mod scale;

#[cfg(target_os = "macos")]
mod backend;
#[cfg(target_os = "macos")]
mod cef_host;
#[cfg(target_os = "macos")]
mod cef_pump;
#[cfg(target_os = "macos")]
mod compositor;
#[cfg(target_os = "macos")]
mod dispatch;
#[cfg(target_os = "macos")]
mod init;
#[cfg(target_os = "macos")]
mod input;
#[cfg(target_os = "macos")]
mod menu;
#[cfg(target_os = "macos")]
mod mpv_host;
#[cfg(target_os = "macos")]
mod ns_menu;

#[cfg(target_os = "macos")]
pub use backend::*;
