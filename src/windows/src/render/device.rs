//! The DirectComposition objects and the process's wgpu device.

use std::sync::OnceLock;

use jfn_gpu_paint::{SharedTexture, Surfaces};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

/// The DComp device, the HWND composition target, and the root visual every
/// surface parents into.
pub(crate) struct Devices {
    device: IDCompositionDevice,
    // Held only to keep the composition target (and the root bound to it)
    // alive; never read after construction.
    #[allow(dead_code)]
    target: IDCompositionTarget,
    root: IDCompositionVisual,
}

impl Devices {
    pub(crate) fn create(hwnd: HWND) -> windows_core::Result<Devices> {
        unsafe {
            // NULL rendering device: this module builds a visual tree and
            // nothing else; the swapchains under it belong to wgpu.
            let device: IDCompositionDevice = DCompositionCreateDevice(None::<&IDXGIDevice>)?;
            let target = device.CreateTargetForHwnd(hwnd, false)?;
            let root = device.CreateVisual()?;
            target.SetRoot(&root)?;
            device.Commit()?;
            Ok(Devices {
                device,
                target,
                root,
            })
        }
    }

    pub(crate) fn root(&self) -> &IDCompositionVisual {
        &self.root
    }

    pub(crate) fn new_visual(&self) -> windows_core::Result<IDCompositionVisual> {
        unsafe { self.device.CreateVisual() }
    }

    /// Publishes every tree change since the last call, including the
    /// `SetContent` wgpu issues from inside `configure`.
    pub(crate) fn commit(&self) {
        unsafe {
            let _ = self.device.Commit();
        }
    }
}

static GPU: OnceLock<Option<Surfaces>> = OnceLock::new();

/// The process's wgpu device, opened on the adapter that produced `sample` —
/// on Windows a shared handle carries the LUID of its creating adapter and
/// nothing else names it.
///
/// One-shot in both directions: a painter borrows the device for `'static`,
/// so it can never be replaced, and a failure to open it means this machine
/// has no adapter that can take CEF's buffers, which no later frame changes.
pub(crate) fn gpu(sample: Option<&SharedTexture>) -> Option<&'static Surfaces> {
    GPU.get_or_init(|| Surfaces::init(sample, None)).as_ref()
}
