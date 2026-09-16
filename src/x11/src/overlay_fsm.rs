pub type Geom = (i32, i32, i32, i32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OverlayState {
    pub mapped: bool,
    pub unmanaged: bool,
}

impl OverlayState {
    pub fn new_mapped(parent_fullscreen: bool) -> Self {
        Self {
            mapped: true,
            unmanaged: parent_fullscreen,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Inputs {
    pub parent_geom: Geom,
    pub parent_fullscreen: bool,
    pub want_visible: bool,
    pub observed: Option<Geom>,
    pub observed_mapped: Option<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    SetOverrideRedirect(bool),
    MapAndRaise,
    Unmap,
    Place,
}

fn rect_differs(observed: Option<Geom>, geom: Geom) -> bool {
    observed.is_none_or(|o| o != geom)
}

pub fn step(state: &mut OverlayState, inputs: &Inputs) -> Vec<Effect> {
    let mut effects = Vec::new();
    let mut force = false;

    if inputs.observed_mapped == Some(false) {
        state.mapped = false;
    }

    if state.unmanaged != inputs.parent_fullscreen {
        state.unmanaged = inputs.parent_fullscreen;
        state.mapped = false;
        effects.push(Effect::SetOverrideRedirect(inputs.parent_fullscreen));
    }

    if !inputs.want_visible {
        if state.mapped {
            state.mapped = false;
            effects.push(Effect::Unmap);
        }
        return effects;
    }

    if !state.mapped {
        state.mapped = true;
        effects.push(Effect::MapAndRaise);
        force = true;
    }

    if force || rect_differs(inputs.observed, inputs.parent_geom) {
        effects.push(Effect::Place);
    }
    effects
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: Geom = (100, 50, 800, 600);
    const FS: Geom = (0, 0, 1920, 1080);

    fn managed() -> OverlayState {
        OverlayState {
            mapped: true,
            unmanaged: false,
        }
    }

    fn unmanaged() -> OverlayState {
        OverlayState {
            mapped: true,
            unmanaged: true,
        }
    }

    fn inputs(parent: Geom, fs: bool, observed: Option<Geom>) -> Inputs {
        Inputs {
            parent_geom: parent,
            parent_fullscreen: fs,
            want_visible: true,
            observed,
            observed_mapped: Some(true),
        }
    }

    fn place_pos(effects: &[Effect]) -> Option<usize> {
        effects.iter().position(|e| matches!(e, Effect::Place))
    }

    #[test]
    fn entering_fullscreen_flips_then_places() {
        let mut s = managed();
        let e = step(&mut s, &inputs(FS, true, Some(WIN)));
        assert_eq!(e[0], Effect::SetOverrideRedirect(true));
        assert_eq!(e[1], Effect::MapAndRaise);
        assert!(e.contains(&Effect::Place));
        let or = e
            .iter()
            .position(|x| *x == Effect::SetOverrideRedirect(true));
        let map = e.iter().position(|x| *x == Effect::MapAndRaise);
        assert!(or < map && map < place_pos(&e));
        assert!(s.unmanaged && s.mapped);
    }

    #[test]
    fn leaving_fullscreen_flips_back() {
        let mut s = unmanaged();
        let e = step(&mut s, &inputs(WIN, false, Some(FS)));
        assert_eq!(e[0], Effect::SetOverrideRedirect(false));
        assert!(e.contains(&Effect::MapAndRaise));
        assert!(e.contains(&Effect::Place));
        assert!(!s.unmanaged);
    }

    #[test]
    fn server_unmapped_triggers_remap() {
        let mut s = unmanaged();
        let mut i = inputs(FS, true, Some(FS));
        i.observed_mapped = Some(false);
        let e = step(&mut s, &i);
        assert!(e.contains(&Effect::MapAndRaise));
    }

    #[test]
    fn fullscreen_converged_is_noop() {
        let mut s = unmanaged();
        let e = step(&mut s, &inputs(FS, true, Some(FS)));
        assert_eq!(e, vec![]);
    }

    #[test]
    fn windowed_converged_is_noop() {
        let mut s = managed();
        let e = step(&mut s, &inputs(WIN, false, Some(WIN)));
        assert_eq!(e, vec![]);
    }

    #[test]
    fn fullscreen_drift_replaces_without_flip() {
        let mut s = unmanaged();
        let clamped = (0, 27, 1920, 1053);
        let e = step(&mut s, &inputs(FS, true, Some(clamped)));
        assert!(
            !e.iter()
                .any(|x| matches!(x, Effect::SetOverrideRedirect(_)))
        );
        assert!(e.contains(&Effect::Place));
    }

    #[test]
    fn windowed_move_replaces() {
        let mut s = managed();
        let moved = (300, 200, 800, 600);
        let e = step(&mut s, &inputs(moved, false, Some(WIN)));
        assert!(e.contains(&Effect::Place));
    }

    #[test]
    fn size_mismatch_replaces() {
        let mut s = managed();
        let bigger = (100, 50, 1024, 768);
        let e = step(&mut s, &inputs(bigger, false, Some(WIN)));
        assert!(e.contains(&Effect::Place));
    }

    #[test]
    fn hidden_overlay_unmaps_and_stops() {
        let mut s = managed();
        let mut i = inputs(WIN, false, Some(WIN));
        i.want_visible = false;
        let e = step(&mut s, &i);
        assert_eq!(e, vec![Effect::Unmap]);
        assert!(!s.mapped);
    }

    #[test]
    fn remap_forces_placement() {
        let mut s = OverlayState {
            mapped: false,
            unmanaged: false,
        };
        let e = step(&mut s, &inputs(WIN, false, Some(WIN)));
        assert!(e.contains(&Effect::MapAndRaise));
        assert!(place_pos(&e).is_some());
    }
}
