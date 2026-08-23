//! CEF process bootstrap + App handlers.

mod app;
pub mod app_menu;
mod business_common;
pub mod business_web;
mod cef_string;
pub mod client;
mod client_impl;
mod embedded_js;
pub mod ffi;
mod frame_rate;
pub mod injection;
mod ipc;
mod menu_ownership;
mod paint_scheduler;
pub mod platform_ops;
pub mod ready;
mod resource;
pub mod server_probe;
mod state;
mod v8_handler;
pub mod version;
mod web_input;
pub mod web_overlay;

pub use client::{ContextBuilderFn, ContextDispatcherFn, CreatedFn};
pub use ffi::*;
pub use web_overlay::{CloseDeliveryError, WebOverlay, WebOverlayConfig};

pub const APP_VERSION: &str = env!("JFN_APP_VERSION");
pub const APP_VERSION_FULL: &str = env!("JFN_APP_VERSION_FULL");
pub use version::cef_version;
