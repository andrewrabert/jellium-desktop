use std::ffi::c_void;
use std::ptr::NonNull;

use crate::{FrameSize, SharedTexture};

/// Where a [`crate::Surface`] attaches its swapchain. One variant per window
/// system; new platforms are added here rather than by reshaping the API.
///
/// The variant also fixes the size policy — see [`crate::painter::SizePolicy`]
/// — because whether the swapchain *is* the window is a property of the target,
/// not a caller preference.
pub enum WindowTarget {
    /// X11 (xcb) — `connection` is an `xcb_connection_t*`, `window` is the XID.
    /// `visual` is the ARGB visual ID. `screen` is the screen index.
    Xcb {
        connection: NonNull<c_void>,
        window: u32,
        screen: i32,
        visual: u32,
    },
    /// Wayland — `display` is `wl_display*`, `surface` is `wl_surface*`.
    /// Once a wl_surface is handed to Vulkan WSI, no other client code
    /// may call `wl_surface_attach`/`commit` on it; presents go through
    /// the swapchain only.
    Wayland {
        display: NonNull<c_void>,
        surface: NonNull<c_void>,
    },
    /// Windows — `visual` is an `IDCompositionVisual*`. wgpu binds its
    /// swapchain to the visual inside `configure` and nowhere else, which is
    /// what [`crate::Surface::content_detached`] exists for; the app keeps
    /// ownership of the visual and its tree.
    CompositionVisual { visual: NonNull<c_void> },
    /// macOS — `layer` is a `CAMetalLayer*`. Configuring the surface *is* the
    /// layer mutation (wgpu writes device, format, colorspace, drawable size
    /// and more), so it belongs to the layer's owner thread.
    CoreAnimationLayer { layer: NonNull<c_void> },
}

/// Which kind of frame a surface carries. Latched from the first frame and
/// never public: callers say what a frame *is* by picking a [`Frame`] variant,
/// which is the same information.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum PaintMode {
    /// Frames arrive as a platform shared-buffer handle.
    Shared,
    /// Frames arrive as CPU pixels, uploaded here.
    Copied,
}

/// Whether a frame reached the screen. A `Skipped` is a deliberate no-op — the
/// surface was hidden, or the swapchain was transiently unavailable — not a
/// failure, and never an `Err`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Presented {
    Yes,
    Skipped,
}

/// A borrowed CPU frame plus the regions that changed since the last one.
/// `stride` is in bytes.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

pub struct Pixels<'a> {
    pub size: FrameSize,
    pub stride: u32,
    pub bgra: &'a [u8],
    pub dirty: &'a [DamageRect],
}

/// One frame handed to [`crate::Surface::present`]. The variant must match the
/// [`PaintMode`] the surface was created with.
pub enum Frame<'a> {
    Shared(&'a SharedTexture),
    Copied(Pixels<'a>),
}

impl Frame<'_> {
    pub(crate) fn mode(&self) -> PaintMode {
        match self {
            Frame::Shared(_) => PaintMode::Shared,
            Frame::Copied(_) => PaintMode::Copied,
        }
    }
}
