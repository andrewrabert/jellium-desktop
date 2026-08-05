//! Fullscreen/resize transition gate.
//!
//! The compositor detaches the main surface's content while the window is
//! resizing (so a stale-size frame isn't stretched), then re-presents once
//! the new size lands. This is a pure value type: it holds no atomics or
//! locks. The OS-bound compositor keeps its own `Mutex`/`AtomicBool` and
//! stores a [`TransitionGate`] inside it — which keeps a backend's own
//! locking intact and lets the gating logic be tested on any host.
//!
//! The two backends drive the gate through different (faithful) entry
//! points:
//! - **macOS** (`G_IN_TRANSITION` + `G_EXPECTED_SIZE`): [`begin`], [`end`],
//!   [`in_transition`], [`set_expected`], [`note_present_size`]. It never
//!   captures a pre-resize size.
//! - **X11** (parent-snapshot capture + expected size):
//!   [`begin_capturing`], [`end`], [`in_transition`], [`set_expected`],
//!   [`main_present_decision`].
//!
//! [`begin`]: TransitionGate::begin
//! [`begin_capturing`]: TransitionGate::begin_capturing
//! [`end`]: TransitionGate::end
//! [`in_transition`]: TransitionGate::in_transition
//! [`set_expected`]: TransitionGate::set_expected
//! [`note_present_size`]: TransitionGate::note_present_size
//! [`main_present_decision`]: TransitionGate::main_present_decision

/// What the main-surface present path should do with an incoming
/// frame. See [`TransitionGate::main_present_decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentDecision {
    /// Drop the frame — still mid-transition at the pre-resize size.
    Reject,
    /// The resize has landed: the gate has been cleared; present this frame.
    EndTransitionThenPresent,
    /// Not transitioning — present normally.
    Present,
}

/// Transition state for one compositor. A physical size is a `(w, h)` pair
/// in pixels.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TransitionGate {
    in_transition: bool,
    /// Post-transition target size set via [`set_expected`]. `None` = unset.
    ///
    /// [`set_expected`]: TransitionGate::set_expected
    expected: Option<(i32, i32)>,
    /// Pre-resize physical size captured at [`begin_capturing`] (X11 only).
    /// `None` on macOS, which never captures.
    ///
    /// [`begin_capturing`]: TransitionGate::begin_capturing
    captured: Option<(i32, i32)>,
}

impl TransitionGate {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            in_transition: false,
            expected: None,
            captured: None,
        }
    }

    #[must_use]
    pub fn in_transition(&self) -> bool {
        self.in_transition
    }

    #[must_use]
    pub fn expected(&self) -> Option<(i32, i32)> {
        self.expected
    }

    /// macOS: enter the transition without capturing a size.
    pub fn begin(&mut self) {
        self.in_transition = true;
    }

    /// X11: enter the transition and record the pre-resize physical size
    /// that [`main_present_decision`] compares against to detect when the
    /// resize has landed.
    ///
    /// [`main_present_decision`]: TransitionGate::main_present_decision
    pub fn begin_capturing(&mut self, captured_phys: (i32, i32)) {
        self.in_transition = true;
        self.captured = Some(captured_phys);
    }

    /// Clear all transition state.
    pub fn end(&mut self) {
        self.in_transition = false;
        self.expected = None;
        self.captured = None;
    }

    /// Record the post-transition target size.
    ///
    /// While transitioning, a target equal to the captured pre-resize size is
    /// ignored — this is X11's `set_expected_size` guard that avoids arming
    /// the gate on the size we're transitioning *away* from. macOS never
    /// captures, so the guard is inert there and the size is always stored.
    pub fn set_expected(&mut self, size: (i32, i32)) {
        if self.in_transition && self.captured == Some(size) {
            return;
        }
        self.expected = Some(size);
    }

    /// macOS present path: when a presented frame matches the expected
    /// post-transition size, clear the gate. Returns `true` if it just
    /// cleared. Mirrors `macos_surface_present`'s clear-on-match: only fires
    /// when an expected size with a positive width has been armed.
    pub fn note_present_size(&mut self, size: (i32, i32)) -> bool {
        if let Some(exp) = self.expected
            && exp.0 > 0
            && exp == size
        {
            self.expected = None;
            self.in_transition = false;
            return true;
        }
        false
    }

    /// Deliberately does not gate on `set_expected` (never armed on X11);
    /// re-adding that condition strands the detached main visual blank.
    pub fn main_present_decision(&mut self, frame: (i32, i32)) -> PresentDecision {
        if !self.in_transition {
            return PresentDecision::Present;
        }
        if frame.0 <= 0 || frame.1 <= 0 || self.captured == Some(frame) {
            return PresentDecision::Reject;
        }
        self.end();
        PresentDecision::EndTransitionThenPresent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_idle() {
        let g = TransitionGate::new();
        assert!(!g.in_transition());
        assert_eq!(g.expected(), None);
        assert_eq!(TransitionGate::default(), g);
    }

    // ---- macOS model ------------------------------------------------

    #[test]
    fn macos_expected_clears_gate_on_match() {
        let mut g = TransitionGate::new();
        g.begin();
        g.set_expected((1920, 1080));
        assert!(g.in_transition());
        // A non-matching frame leaves the gate armed.
        assert!(!g.note_present_size((1280, 720)));
        assert!(g.in_transition());
        // The matching frame clears expected *and* the in_transition flag.
        assert!(g.note_present_size((1920, 1080)));
        assert!(!g.in_transition());
        assert_eq!(g.expected(), None);
    }

    #[test]
    fn macos_note_present_ignores_unset_and_zero_expected() {
        let mut g = TransitionGate::new();
        // Nothing armed.
        assert!(!g.note_present_size((1920, 1080)));
        // A zero-width expected size is treated as unset (mirrors exp.0 > 0).
        g.set_expected((0, 0));
        assert!(!g.note_present_size((0, 0)));
    }

    #[test]
    fn macos_set_expected_always_stores_without_capture() {
        let mut g = TransitionGate::new();
        g.begin(); // no capture
        g.set_expected((800, 600));
        assert_eq!(g.expected(), Some((800, 600)));
    }

    // ---- X11 model --------------------------------------------------

    #[test]
    fn x11_present_recovers_without_expected_armed() {
        let mut g = TransitionGate::new();
        g.begin_capturing((1280, 720));
        assert_eq!(
            g.main_present_decision((1920, 1080)),
            PresentDecision::EndTransitionThenPresent
        );
        assert!(!g.in_transition());
    }

    #[test]
    fn x11_rejects_frame_matching_captured_size() {
        let mut g = TransitionGate::new();
        g.begin_capturing((1280, 720));
        assert_eq!(
            g.main_present_decision((1280, 720)),
            PresentDecision::Reject
        );
        assert!(g.in_transition());
    }

    #[test]
    fn x11_rejects_non_positive_frame_during_transition() {
        let mut g = TransitionGate::new();
        g.begin_capturing((1280, 720));
        assert_eq!(g.main_present_decision((0, 1080)), PresentDecision::Reject);
        assert!(g.in_transition());
    }

    #[test]
    fn x11_matching_post_resize_frame_ends_then_presents() {
        let mut g = TransitionGate::new();
        g.begin_capturing((1280, 720));
        g.set_expected((1920, 1080));
        assert_eq!(
            g.main_present_decision((1920, 1080)),
            PresentDecision::EndTransitionThenPresent
        );
        assert!(!g.in_transition());
        assert_eq!(g.expected(), None);
    }

    #[test]
    fn x11_set_expected_guard_ignores_captured_size() {
        let mut g = TransitionGate::new();
        g.begin_capturing((1280, 720));
        // Arming the expected size to the captured pre-resize size is a no-op.
        g.set_expected((1280, 720));
        assert_eq!(g.expected(), None);
        // A real target still arms.
        g.set_expected((1920, 1080));
        assert_eq!(g.expected(), Some((1920, 1080)));
    }

    #[test]
    fn x11_present_passes_through_when_idle() {
        let mut g = TransitionGate::new();
        assert_eq!(
            g.main_present_decision((1920, 1080)),
            PresentDecision::Present
        );
    }

    #[test]
    fn end_is_idempotent() {
        let mut g = TransitionGate::new();
        g.begin_capturing((100, 100));
        g.set_expected((200, 200));
        g.end();
        g.end();
        assert!(!g.in_transition());
        assert_eq!(g.expected(), None);
    }
}
