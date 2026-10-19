//! Touch sequences.
//!
//! A touch point is routed once, on its press, and keeps that owner until it
//! is released or cancelled — a finger that slides off the titlebar into the
//! page stays the titlebar's. jellyfin-web takes real touch events, so CEF
//! runs its own gesture recognition; the shell overlay has no touch input
//! and takes a touch point as its left button.

use crate::route::ShellHit;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TouchPhase {
    Pressed,
    Moved,
    Released,
    Cancelled,
}

/// Who a live touch point belongs to.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Owner {
    Web,
    /// The shell overlay, as its left button.
    Shell,
    /// A window gesture began on the press; the compositor drives the rest.
    Gesture,
    /// Nobody: pressed before any routing state was published, or while
    /// another touch point already held the shell overlay's one button.
    Nobody,
}

/// What a touch event becomes.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Delivery {
    /// The event, unchanged, to jellyfin-web.
    Web(TouchPhase),
    /// The event as the shell overlay's left button.
    ShellButton(TouchPhase),
    /// A press on a [`ShellHit::Drag`] or [`ShellHit::Grip`].
    WindowGesture(ShellHit),
    Drop,
}

/// The owner of every live touch point, keyed by the platform's touch id.
pub struct Touches {
    owners: Vec<(i32, Owner)>,
}

impl Default for Touches {
    fn default() -> Self {
        Self::new()
    }
}

impl Touches {
    pub const fn new() -> Self {
        Self { owners: Vec::new() }
    }

    /// Routes one event of touch point `id`. `hit` is what the shell overlay
    /// owns under the point, `None` while no routing state is published; it
    /// is read only on [`TouchPhase::Pressed`].
    pub fn route(&mut self, id: i32, phase: TouchPhase, hit: Option<ShellHit>) -> Delivery {
        match phase {
            TouchPhase::Pressed => self.press(id, hit),
            TouchPhase::Moved => match self.owner(id) {
                Some(owner) => deliver(owner, phase),
                None => Delivery::Drop,
            },
            TouchPhase::Released | TouchPhase::Cancelled => match self.take(id) {
                Some(owner) => deliver(owner, phase),
                None => Delivery::Drop,
            },
        }
    }

    fn press(&mut self, id: i32, hit: Option<ShellHit>) -> Delivery {
        // A press under a live id restarts that id's sequence; the
        // platform ended the old one without saying so.
        self.take(id);
        let shell_held = self
            .owners
            .iter()
            .any(|(_, owner)| matches!(owner, Owner::Shell | Owner::Gesture));
        let (owner, delivery) = match hit {
            None => (Owner::Nobody, Delivery::Drop),
            Some(ShellHit::Miss) => (Owner::Web, Delivery::Web(TouchPhase::Pressed)),
            Some(_) if shell_held => (Owner::Nobody, Delivery::Drop),
            Some(hit @ (ShellHit::Drag | ShellHit::Grip(_))) => {
                (Owner::Gesture, Delivery::WindowGesture(hit))
            }
            Some(ShellHit::Modal | ShellHit::Controls) => {
                (Owner::Shell, Delivery::ShellButton(TouchPhase::Pressed))
            }
        };
        self.owners.push((id, owner));
        delivery
    }

    fn owner(&self, id: i32) -> Option<Owner> {
        self.owners
            .iter()
            .find(|(live, _)| *live == id)
            .map(|(_, owner)| *owner)
    }

    fn take(&mut self, id: i32) -> Option<Owner> {
        let at = self.owners.iter().position(|(live, _)| *live == id)?;
        Some(self.owners.swap_remove(at).1)
    }
}

fn deliver(owner: Owner, phase: TouchPhase) -> Delivery {
    match owner {
        Owner::Web => Delivery::Web(phase),
        Owner::Shell => Delivery::ShellButton(phase),
        Owner::Gesture | Owner::Nobody => Delivery::Drop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use TouchPhase::{Cancelled, Moved, Pressed, Released};

    #[test]
    fn a_touch_on_the_page_is_the_pages_whole_sequence() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(0, Pressed, Some(ShellHit::Miss)),
            Delivery::Web(Pressed)
        );
        assert_eq!(t.route(0, Moved, None), Delivery::Web(Moved));
        assert_eq!(t.route(0, Released, None), Delivery::Web(Released));
        assert_eq!(t.route(0, Moved, None), Delivery::Drop);
    }

    #[test]
    fn the_press_decides_the_owner_for_the_whole_sequence() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(0, Pressed, Some(ShellHit::Controls)),
            Delivery::ShellButton(Pressed)
        );
        // Later hits are ignored: the point keeps the owner it pressed on.
        assert_eq!(
            t.route(0, Moved, Some(ShellHit::Miss)),
            Delivery::ShellButton(Moved)
        );
        assert_eq!(
            t.route(0, Released, Some(ShellHit::Miss)),
            Delivery::ShellButton(Released)
        );
    }

    #[test]
    fn a_window_gesture_starts_on_the_press_and_swallows_the_rest() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(3, Pressed, Some(ShellHit::Drag)),
            Delivery::WindowGesture(ShellHit::Drag)
        );
        assert_eq!(t.route(3, Moved, None), Delivery::Drop);
        assert_eq!(t.route(3, Released, None), Delivery::Drop);
        assert_eq!(
            t.route(4, Pressed, Some(ShellHit::Grip(2))),
            Delivery::WindowGesture(ShellHit::Grip(2))
        );
    }

    #[test]
    fn the_shell_overlay_takes_one_touch_point_at_a_time() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(0, Pressed, Some(ShellHit::Modal)),
            Delivery::ShellButton(Pressed)
        );
        assert_eq!(t.route(1, Pressed, Some(ShellHit::Modal)), Delivery::Drop);
        assert_eq!(t.route(1, Released, None), Delivery::Drop);
        assert_eq!(t.route(0, Released, None), Delivery::ShellButton(Released));
        assert_eq!(
            t.route(1, Pressed, Some(ShellHit::Modal)),
            Delivery::ShellButton(Pressed)
        );
    }

    #[test]
    fn the_page_takes_every_touch_point_beside_the_shells() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(0, Pressed, Some(ShellHit::Controls)),
            Delivery::ShellButton(Pressed)
        );
        assert_eq!(
            t.route(1, Pressed, Some(ShellHit::Miss)),
            Delivery::Web(Pressed)
        );
        assert_eq!(
            t.route(2, Pressed, Some(ShellHit::Miss)),
            Delivery::Web(Pressed)
        );
        assert_eq!(t.route(2, Cancelled, None), Delivery::Web(Cancelled));
        assert_eq!(
            t.route(0, Cancelled, None),
            Delivery::ShellButton(Cancelled)
        );
    }

    #[test]
    fn a_press_with_no_routing_state_reaches_nobody() {
        let mut t = Touches::new();
        assert_eq!(t.route(0, Pressed, None), Delivery::Drop);
        assert_eq!(t.route(0, Moved, Some(ShellHit::Miss)), Delivery::Drop);
        assert_eq!(t.route(0, Released, None), Delivery::Drop);
    }

    #[test]
    fn a_reused_id_restarts_its_sequence() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(0, Pressed, Some(ShellHit::Controls)),
            Delivery::ShellButton(Pressed)
        );
        // The shell's held point was replaced, so the shell is free again.
        assert_eq!(
            t.route(0, Pressed, Some(ShellHit::Modal)),
            Delivery::ShellButton(Pressed)
        );
        assert_eq!(t.route(0, Released, None), Delivery::ShellButton(Released));
        assert_eq!(t.route(0, Released, None), Delivery::Drop);
    }
}
