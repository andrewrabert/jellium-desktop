//! The process's single owner of z-order.
//!
//! Every surface that composites is a plane's occupant, and the whole order is
//! applied from this one value. Nothing else in the process places a surface
//! relative to another, so no thread can order two surfaces against each other
//! and no visibility change moves one.

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

const PLANES: [Plane; 5] = [
    Plane::Video,
    Plane::WebOverlay,
    Plane::ShellOverlay,
    Plane::WebPopup,
    Plane::MenuPopup,
];

static OCCUPANTS: Mutex<[Option<SurfaceHandle>; 5]> = Mutex::new([None; 5]);

/// Installs `s` as `plane`'s occupant, replacing any previous one, and applies
/// the whole order.
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
    let mut occupants = OCCUPANTS.lock();
    occupants[plane as usize] = occupant;
    let ordered: Vec<SurfaceHandle> = PLANES
        .iter()
        .filter_map(|p| occupants[*p as usize])
        .collect();
    if let Some(plat) = crate::try_get() {
        plat.apply_stack(&ordered);
    }
}
