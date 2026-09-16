#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentDecision {
    Reject,
    EndTransitionThenPresent,
    Present,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TransitionGate {
    in_transition: bool,
    expected: Option<(i32, i32)>,
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

    pub fn begin(&mut self) {
        self.in_transition = true;
    }

    pub fn begin_capturing(&mut self, captured_phys: (i32, i32)) {
        self.in_transition = true;
        self.captured = Some(captured_phys);
    }

    pub fn end(&mut self) {
        self.in_transition = false;
        self.expected = None;
        self.captured = None;
    }

    pub fn set_expected(&mut self, size: (i32, i32)) {
        if self.in_transition && self.captured == Some(size) {
            return;
        }
        self.expected = Some(size);
    }

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

    #[test]
    fn macos_expected_clears_gate_on_match() {
        let mut g = TransitionGate::new();
        g.begin();
        g.set_expected((1920, 1080));
        assert!(g.in_transition());
        assert!(!g.note_present_size((1280, 720)));
        assert!(g.in_transition());
        assert!(g.note_present_size((1920, 1080)));
        assert!(!g.in_transition());
        assert_eq!(g.expected(), None);
    }

    #[test]
    fn macos_note_present_ignores_unset_and_zero_expected() {
        let mut g = TransitionGate::new();
        assert!(!g.note_present_size((1920, 1080)));
        g.set_expected((0, 0));
        assert!(!g.note_present_size((0, 0)));
    }

    #[test]
    fn macos_set_expected_always_stores_without_capture() {
        let mut g = TransitionGate::new();
        g.begin();
        g.set_expected((800, 600));
        assert_eq!(g.expected(), Some((800, 600)));
    }

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
        g.set_expected((1280, 720));
        assert_eq!(g.expected(), None);
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
