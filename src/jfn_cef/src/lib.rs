//! CEF process bootstrap + App handlers.

mod app;
mod app_menu;
mod business_common;
pub mod business_web;
mod cef_string;
pub mod client;
mod client_impl;
mod embedded_js;
mod ffi;
mod frame_rate;
pub mod injection;
mod ipc;
mod menu_ownership;
mod paint_scheduler;
pub mod platform_ops;
mod ready;
mod resource;
mod runtime;
mod server_probe;
mod state;
mod v8_handler;
pub mod version;
mod web_input;
pub mod web_overlay;

pub use app_menu::ApplicationMenu;
pub use client::{ContextBuilderFn, ContextDispatcherFn, CreatedFn};
pub use web_overlay::{CloseDeliveryError, WebOverlay, WebOverlayConfig};

pub const APP_VERSION: &str = env!("JFN_APP_VERSION");
pub const APP_VERSION_FULL: &str = env!("JFN_APP_VERSION_FULL");
pub use runtime::{
    BrowserCef, DebugPort, DebuggingPort, InitError, InitOptions, InitializedCef,
    InvalidDebuggingPort, LoadError, LoadedCef, LogSeverity, ProcessDispatch, ShutdownError,
};

mod navigation;
pub use navigation::{Navigation, NavigationPresented, WebEvent, WebEventHandler};

pub use ready::ReadinessError as WebOperationError;
