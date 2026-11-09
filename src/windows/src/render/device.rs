//! The DirectComposition objects and the process's wgpu device.

use jfn_gpu_paint::Surfaces;
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

    /// Commits and blocks until the composition engine has processed it, so
    /// the change is on screen before the caller returns.
    pub(crate) fn commit_and_wait(&self) {
        self.commit();
        if let Err(e) = unsafe { self.device.WaitForCommitCompletion() } {
            tracing::error!(target: "platform", "DirectComposition WaitForCommitCompletion failed: {e:?}");
        }
    }
}

/// The process's wgpu device, opened at boot on the adapter this app picked.
/// Chromium's GPU process is pinned to the same adapter on the command line,
/// so no frame ever has to name it.
pub(crate) fn gpu() -> Option<&'static Surfaces> {
    Surfaces::init(None)
}
