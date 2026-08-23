//! The process's single owner of z-order.
//!
//! Every non-web surface that composites is a plane's occupant. The web
//! overlay's owner is the only code that can add its handle to the order.

use std::sync::Weak;

use parking_lot::Mutex;

use crate::SurfaceHandle;

/// The composited planes, bottom first. The order is this declaration's order
/// and is never data a caller supplies.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Plane {
    /// mpv's video, pinned below every app surface by the backend.
    Video,
    WebOverlay,
    ShellOverlay,
    /// CEF's off-screen popup.
    WebPopup,
    /// The native menu popup.
    MenuPopup,
}

/// Applies an order containing the web overlay without exposing its handle to
/// the stack registry.
pub trait WebOverlayStacker: Send + Sync {
    fn apply_web_overlay_stack(&self, lower: &[SurfaceHandle], upper: &[SurfaceHandle]);
}

#[derive(Default)]
struct StackState {
    lower: [Option<SurfaceHandle>; 1],
    upper: [Option<SurfaceHandle>; 3],
    web_overlay: Option<Weak<dyn WebOverlayStacker>>,
}

static STATE: Mutex<StackState> = Mutex::new(StackState {
    lower: [None; 1],
    upper: [None; 3],
    web_overlay: None,
});

/// Installs the live web-overlay surface owner and applies the whole order.
pub fn install_web_overlay_stacker(stacker: Weak<dyn WebOverlayStacker>) {
    let mut state = STATE.lock();
    state.web_overlay = Some(stacker);
    apply(&mut state);
}

/// Removes the web-overlay surface owner and reapplies the non-web order.
pub fn remove_web_overlay_stacker() {
    let mut state = STATE.lock();
    state.web_overlay = None;
    apply(&mut state);
}

/// Installs `s` as `plane`'s occupant, replacing any previous one, and applies
/// the whole order. The web-overlay plane is owned exclusively through
/// [`install_web_overlay_stacker`], so its handle is never retained here.
pub fn occupy(plane: Plane, s: SurfaceHandle) {
    write(plane, (!s.is_none()).then_some(s));
}

/// Empties `plane` and applies the whole order.
pub fn vacate(plane: Plane) {
    write(plane, None);
}

fn write(plane: Plane, occupant: Option<SurfaceHandle>) {
    // The lock is held across the apply, so two writers cannot interleave and
    // leave the older order on screen.
    let mut state = STATE.lock();
    match plane {
        Plane::Video => state.lower[0] = occupant,
        Plane::WebOverlay => {}
        Plane::ShellOverlay => state.upper[0] = occupant,
        Plane::WebPopup => state.upper[1] = occupant,
        Plane::MenuPopup => state.upper[2] = occupant,
    }
    apply(&mut state);
}

fn apply(state: &mut StackState) {
    let lower: Vec<SurfaceHandle> = state.lower.iter().flatten().copied().collect();
    let upper: Vec<SurfaceHandle> = state.upper.iter().flatten().copied().collect();

    let stacker = state.web_overlay.as_ref().and_then(Weak::upgrade);
    if let Some(stacker) = stacker {
        stacker.apply_web_overlay_stack(&lower, &upper);
    } else {
        state.web_overlay = None;
        apply_non_web_stack(lower, &upper);
    }
}

fn apply_non_web_stack(mut lower: Vec<SurfaceHandle>, upper: &[SurfaceHandle]) {
    lower.extend_from_slice(upper);
    if let Some(platform) = crate::try_get() {
        platform.apply_stack(&lower);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Default)]
    struct RecordingStacker {
        applications: Mutex<Vec<(Vec<SurfaceHandle>, Vec<SurfaceHandle>)>>,
    }

    impl WebOverlayStacker for RecordingStacker {
        fn apply_web_overlay_stack(&self, lower: &[SurfaceHandle], upper: &[SurfaceHandle]) {
            self.applications
                .lock()
                .push((lower.to_vec(), upper.to_vec()));
        }
    }

    fn reset() {
        *STATE.lock() = StackState::default();
    }

    fn install_recorder() -> Arc<RecordingStacker> {
        let recorder = Arc::new(RecordingStacker::default());
        let owner: Arc<dyn WebOverlayStacker> = recorder.clone();
        install_web_overlay_stacker(Arc::downgrade(&owner));
        recorder
    }

    #[test]
    fn stack_state_never_retains_the_web_overlay_handle() {
        let _serial = TEST_LOCK.lock();
        reset();
        occupy(Plane::WebOverlay, SurfaceHandle::from_id(7));

        let state = STATE.lock();
        assert_eq!(state.lower, [None]);
        assert_eq!(state.upper, [None; 3]);
    }

    #[test]
    fn non_web_changes_are_applied_by_the_live_web_overlay_owner() {
        let _serial = TEST_LOCK.lock();
        reset();
        let recorder = install_recorder();
        let shell = SurfaceHandle::from_id(11);

        occupy(Plane::ShellOverlay, shell);

        assert_eq!(
            recorder.applications.lock().last(),
            Some(&(Vec::new(), vec![shell]))
        );
    }

    #[test]
    fn removing_the_web_overlay_reapplies_the_non_web_order() {
        let _serial = TEST_LOCK.lock();
        reset();
        let recorder = install_recorder();
        let shell = SurfaceHandle::from_id(13);
        occupy(Plane::ShellOverlay, shell);

        remove_web_overlay_stacker();

        let state = STATE.lock();
        assert!(state.web_overlay.is_none());
        assert_eq!(state.upper, [Some(shell), None, None]);
        assert_eq!(recorder.applications.lock().len(), 2);
    }
}
