//! Pure placement state machine for one CEF overlay tracking the mpv parent.
//!
//! No x11rb / FFI / OS calls — plain values, so it unit-tests on any host like
//! [`jfn_compositor_core::transition`]. The geometry thread is the executor: it
//! snapshots parent truth, calls [`step`] per overlay, and applies the effects.
//!
//! Fullscreen escape works by flipping the overlay to `override_redirect` (an
//! unmanaged window the WM won't strut-clamp), positioned by us at the parent's
//! fullscreen rect. Windowed overlays stay WM-managed transients.

/// Absolute screen rect: `(x, y, w, h)`.
pub type Geom = (i32, i32, i32, i32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OverlayState {
    pub mapped: bool,
    /// `override_redirect` is set (unmanaged) — true exactly while fullscreen.
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

/// Parent truth + this overlay's wants, snapshotted once per handler pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Inputs {
    /// Absolute parent geometry; in fullscreen this is the monitor rect.
    pub parent_geom: Geom,
    /// Source from the parent's `_NET_WM_STATE`, never the mpv callback flag,
    /// which can lag the WM.
    pub parent_fullscreen: bool,
    pub want_visible: bool,
    pub observed: Option<Geom>,
    /// The window's actual server map state, if known. Lets the FSM recover when
    /// the WM withdraws (unmaps) the window during the override_redirect flip —
    /// we just re-map until it sticks (the WM withdraws only once).
    pub observed_mapped: Option<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Unmap (if mapped) + set the `override_redirect` attribute; leaves the
    /// window unmapped. The WM only re-reads `override_redirect` at map time, so
    /// the flip MUST round-trip through a remap — hence a following `MapAndRaise`.
    SetOverrideRedirect(bool),
    /// Map + re-establish the passive button grab + raise above the parent.
    MapAndRaise,
    Unmap,
    /// Reassert this overlay's position AND size from the parent geometry in one
    /// configure. The geometry thread is the sole sizer, so placement and size
    /// move together; emitted only when the observed rect differs (or a just-
    /// mapped overlay has no trustworthy sample), which keeps our own
    /// ConfigureNotify from looping.
    Place,
}

fn rect_differs(observed: Option<Geom>, geom: Geom) -> bool {
    observed.is_none_or(|o| o != geom)
}

/// Re-derive this overlay's desired placement from `inputs` and emit the minimal
/// idempotent effects to converge `state`. A converged, unchanged state yields `[]`.
pub fn step(state: &mut OverlayState, inputs: &Inputs) -> Vec<Effect> {
    let mut effects = Vec::new();
    let mut force = false;

    // Server truth wins: if the window was unmapped out from under us (the WM
    // withdrawing a managed window during the flip), drop our belief so the
    // block below re-maps it.
    if inputs.observed_mapped == Some(false) {
        state.mapped = false;
    }

    // The override_redirect flip can only take effect through a remap, so it
    // drops the window to unmapped and the visibility block below maps it back.
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

    // A just-mapped overlay has no trustworthy observed geometry yet, so force
    // the placement effect rather than relying on a stale/absent sample.
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

    // Entering fullscreen flips to override_redirect (remap), then places it at
    // the fullscreen rect — ordering matters: setattr → map → place.
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

    // The WM withdrew (unmapped) the window during the flip — server truth says
    // unmapped, so re-map even though our state believed mapped.
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

    // No flip while staying fullscreen: a WM clamp shows as a rect mismatch and
    // is corrected by a plain place (we own the unmanaged geometry).
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

    // A pure size mismatch (same origin) still triggers a place — the geometry
    // thread is the sole sizer.
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
