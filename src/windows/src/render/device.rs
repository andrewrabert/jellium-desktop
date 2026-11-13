//! The DirectComposition objects and the process's wgpu device.

use jfn_gpu_paint::Surfaces;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

/// The DComp device, the HWND composition target, and the root visual every
/// surface parents into.
///
/// Stays on the original tier-1 `DCompositionCreateDevice`: this app's
/// target hardware (confirmed on an actual TV machine, not just an RDP test
/// session) returns `E_NOINTERFACE` from `DCompositionCreateDevice3` itself
/// — DirectComposition tier 3 is unsupported outright here, not merely
/// unsupported per-visual, so `IDCompositionVisual3` (`SetVisible`) is never
/// reachable. `Layer::set_visible` uses a tier-1-compatible technique
/// instead (see its own comment).
pub(crate) struct Devices {
    device: IDCompositionDevice,
    // Keep-alive, never read: dropping the target unbinds the visual tree
    // from the HWND. `Option` so `rebind` can drop the old target *before*
    // attaching `root` to a new one — see `rebind`'s own comment.
    _target: Option<IDCompositionTarget>,
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
                _target: Some(target),
                root,
            })
        }
    }

    /// Rebinds the composition target to `hwnd` — mpv recreating its VO
    /// window (e.g. going from idle to active playback) replaces the HWND
    /// entirely, and the old target only composites onto the now-destroyed
    /// one.
    ///
    /// Confirmed on real hardware: a first version of this retried only
    /// `CreateTargetForHwnd` and never logged a single retry before the
    /// caller's failure log, meaning that call wasn't what failed —
    /// `target.SetRoot(&self.root)` or `Commit()` was, almost certainly
    /// because the *old* target (still alive in `self._target`, overwritten
    /// only on success) still held `root` as its own root at that point.
    /// DirectComposition can reject attaching a visual that's already
    /// another target's root. Dropping the old target first, then retrying
    /// the whole create+attach+commit sequence as one unit, covers both that
    /// and the plain `CreateTargetForHwnd`-not-ready-yet race.
    pub(crate) fn rebind(&mut self, hwnd: HWND) -> windows_core::Result<()> {
        self._target = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let attempt = unsafe {
                self.device
                    .CreateTargetForHwnd(hwnd, false)
                    .and_then(|target| {
                        target.SetRoot(&self.root)?;
                        self.device.Commit()?;
                        Ok(target)
                    })
            };
            match attempt {
                Ok(target) => {
                    self._target = Some(target);
                    return Ok(());
                }
                Err(e) if std::time::Instant::now() < deadline => {
                    tracing::warn!(target: "platform", "composition target rebind retrying after: {e:?}");
                    std::thread::sleep(std::time::Duration::from_millis(15));
                }
                Err(e) => return Err(e),
            }
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
