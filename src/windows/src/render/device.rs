use jfn_gpu_paint::Surfaces;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

pub(crate) struct Devices {
    device: IDCompositionDevice,
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

    pub(crate) fn commit(&self) {
        if let Err(e) = unsafe { self.device.Commit() } {
            tracing::error!(target: "platform", "DirectComposition Commit failed: {e:?}");
        }
    }

    pub(crate) fn commit_and_wait(&self) {
        self.commit();
        if let Err(e) = unsafe { self.device.WaitForCommitCompletion() } {
            tracing::error!(target: "platform", "DirectComposition WaitForCommitCompletion failed: {e:?}");
        }
    }
}

pub(crate) fn gpu() -> Option<&'static Surfaces> {
    Surfaces::init(None)
}
