#[cfg(target_os = "linux")]
pub use jfn_gpu_paint::{DmabufFormat, DmabufPlane};
pub use jfn_gpu_paint::{FrameSize, SharedTexture};
pub use jfn_platform_abi::{
    Content, DisplayBackend, FrameSource, JfnRect, MENU_DISMISSED, MenuDelivery, MenuItem,
    MenuKind, MenuRequest, MenuSelection, PaintFrame, PhysicalSize, Platform, Presented,
    Superseded, SurfaceHandle, SurfaceSize, Visibility,
};

pub fn ops() -> Option<jfn_platform_abi::PlatformLease> {
    jfn_platform_abi::try_lease()
}
