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
    // Keep-alive, never read: dropping the target unbinds the visual tree
    // from the HWND.
    _target: IDCompositionTarget,
    root: IDCompositionVisual,
}

impl Devices {
    pub(crate) fn create(hwnd: HWND) -> windows_core::Result<Devices> {
        unsafe {
            let device: IDCompositionDevice = DCompositionCreateDevice(None::<&IDXGIDevice>)?;
            let target = device.CreateTargetForHwnd(hwnd, false)?;
            let root = device.CreateVisual()?;
            target.SetRoot(&root)?;
            device.Commit()?;
            Ok(Devices {
                device,
                _target: target,
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
    ///
    /// A failed commit is loud even though it cannot be handled here: it
    /// usually means the composition device was lost, after which no tree
    /// change ever reaches the screen again.
    pub(crate) fn commit(&self) {
        if let Err(e) = unsafe { self.device.Commit() } {
            tracing::error!(target: "platform", "DirectComposition Commit failed: {e:?}");
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
