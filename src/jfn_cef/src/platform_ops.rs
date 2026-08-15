//! Thin re-export shim over [`jfn_platform_abi`].

#[cfg(target_os = "linux")]
pub use jfn_gpu_paint::{DmabufFormat, DmabufPlane};
pub use jfn_gpu_paint::{FrameSize, SharedTexture};
pub use jfn_platform_abi::{
    Content, DisplayBackend, FrameSource, JfnRect, MENU_DISMISSED, MenuDelivery, MenuItem,
    MenuKind, MenuRequest, MenuSelection, PaintFrame, PhysicalSize, Platform, Presented,
    Superseded, SurfaceHandle, SurfaceSize, Visibility,
};

/// Returns the installed platform backend, or `None` if no backend has
/// been installed yet (e.g. early CEF helper-process boot before
/// `jfn_app_main` runs).
pub fn ops() -> Option<&'static dyn Platform> {
    jfn_platform_abi::try_get()
}
