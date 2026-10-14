//! Touch sequences.
//!
//! A touch point is routed once, on its press, and keeps that owner until it
//! is released or cancelled — a finger that slides off the titlebar into the
//! page stays the titlebar's. jellyfin-web takes real touch events, so CEF
//! runs its own gesture recognition; the shell overlay has no touch input
//! and takes a touch point as its left button.
//!
//! A tablet tool in contact with the tablet is a touch point too: jellyfin-web
//! takes it as a pen, everything else exactly as a finger.

use crate::route::ShellHit;
use std::os::raw::c_int;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TouchPhase {
    Pressed,
    Moved,
    Released,
    Cancelled,
}

/// The identity of a touch point. Fingers and tablet tools number their
/// points independently, so the two never share an identity.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Contact {
    /// A finger, by the platform's touch id.
    Finger(i32),
    /// A tablet tool, by an id the platform backend assigns per tool.
    Tool(u32),
}

/// What is touching, as jellyfin-web sees it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PointerKind {
    Touch,
    Pen,
    Eraser,
}

/// One touch point at a window-space position.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct TouchPoint {
    pub contact: Contact,
    pub kind: PointerKind,
    pub x: f64,
    pub y: f64,
    /// Normalized to 0..=1; 0 where the platform reports none.
    pub pressure: f32,
}

/// A touch event for jellyfin-web, in the web overlay's coordinates.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct WebTouch {
    /// Small and unique among the points jellyfin-web currently holds; see
    /// [`Touches`].
    pub id: c_int,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub kind: PointerKind,
    pub phase: TouchPhase,
    pub modifiers: u32,
}

/// Who a live touch point belongs to.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Owner {
    /// jellyfin-web, under the id it knows the point by.
    Web(c_int),
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
    /// The event, unchanged, to jellyfin-web under the given id.
    Web(TouchPhase, c_int),
    /// The event as the shell overlay's left button.
    ShellButton(TouchPhase),
    /// A press on a [`ShellHit::Drag`] or [`ShellHit::Grip`].
    WindowGesture(ShellHit),
    Drop,
}

/// The owner of every live touch point.
///
/// jellyfin-web knows a point by the lowest id no other point it holds uses,
/// not by the platform's identity: the two identity spaces would collide, and
/// Chromium's gesture recognition tracks points in a small fixed id range.
pub struct Touches {
    owners: Vec<(Contact, Owner)>,
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

    /// Routes one event of `contact`. `hit` is what the shell overlay owns
    /// under the point, `None` while no routing state is published; it is
    /// read only on [`TouchPhase::Pressed`].
    pub fn route(
        &mut self,
        contact: Contact,
        phase: TouchPhase,
        hit: Option<ShellHit>,
    ) -> Delivery {
        match phase {
            TouchPhase::Pressed => self.press(contact, hit),
            TouchPhase::Moved => match self.owner(contact) {
                Some(owner) => deliver(owner, phase),
                None => Delivery::Drop,
            },
            TouchPhase::Released | TouchPhase::Cancelled => match self.take(contact) {
                Some(owner) => deliver(owner, phase),
                None => Delivery::Drop,
            },
        }
    }

    fn press(&mut self, contact: Contact, hit: Option<ShellHit>) -> Delivery {
        // A press under a live identity restarts that sequence; the
        // platform ended the old one without saying so.
        self.take(contact);
        let shell_held = self
            .owners
            .iter()
            .any(|(_, owner)| matches!(owner, Owner::Shell | Owner::Gesture));
        let (owner, delivery) = match hit {
            None => (Owner::Nobody, Delivery::Drop),
            Some(ShellHit::Miss) => {
                let id = self.free_web_id();
                (Owner::Web(id), Delivery::Web(TouchPhase::Pressed, id))
            }
            Some(_) if shell_held => (Owner::Nobody, Delivery::Drop),
            Some(hit @ (ShellHit::Drag | ShellHit::Grip(_))) => {
                (Owner::Gesture, Delivery::WindowGesture(hit))
            }
            Some(ShellHit::Modal | ShellHit::Controls) => {
                (Owner::Shell, Delivery::ShellButton(TouchPhase::Pressed))
            }
        };
        self.owners.push((contact, owner));
        delivery
    }

    fn free_web_id(&self) -> c_int {
        let mut id = 0;
        while self
            .owners
            .iter()
            .any(|(_, owner)| *owner == Owner::Web(id))
        {
            id += 1;
        }
        id
    }

    fn owner(&self, contact: Contact) -> Option<Owner> {
        self.owners
            .iter()
            .find(|(live, _)| *live == contact)
            .map(|(_, owner)| *owner)
    }

    fn take(&mut self, contact: Contact) -> Option<Owner> {
        let at = self.owners.iter().position(|(live, _)| *live == contact)?;
        Some(self.owners.swap_remove(at).1)
    }
}

fn deliver(owner: Owner, phase: TouchPhase) -> Delivery {
    match owner {
        Owner::Web(id) => Delivery::Web(phase, id),
        Owner::Shell => Delivery::ShellButton(phase),
        Owner::Gesture | Owner::Nobody => Delivery::Drop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use Contact::{Finger, Tool};
    use TouchPhase::{Cancelled, Moved, Pressed, Released};

    #[test]
    fn a_touch_on_the_page_is_the_pages_whole_sequence() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(Finger(0), Pressed, Some(ShellHit::Miss)),
            Delivery::Web(Pressed, 0)
        );
        assert_eq!(t.route(Finger(0), Moved, None), Delivery::Web(Moved, 0));
        assert_eq!(
            t.route(Finger(0), Released, None),
            Delivery::Web(Released, 0)
        );
        assert_eq!(t.route(Finger(0), Moved, None), Delivery::Drop);
    }

    #[test]
    fn the_press_decides_the_owner_for_the_whole_sequence() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(Finger(0), Pressed, Some(ShellHit::Controls)),
            Delivery::ShellButton(Pressed)
        );
        // Later hits are ignored: the point keeps the owner it pressed on.
        assert_eq!(
            t.route(Finger(0), Moved, Some(ShellHit::Miss)),
            Delivery::ShellButton(Moved)
        );
        assert_eq!(
            t.route(Finger(0), Released, Some(ShellHit::Miss)),
            Delivery::ShellButton(Released)
        );
    }

    #[test]
    fn a_window_gesture_starts_on_the_press_and_swallows_the_rest() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(Finger(3), Pressed, Some(ShellHit::Drag)),
            Delivery::WindowGesture(ShellHit::Drag)
        );
        assert_eq!(t.route(Finger(3), Moved, None), Delivery::Drop);
        assert_eq!(t.route(Finger(3), Released, None), Delivery::Drop);
        assert_eq!(
            t.route(Finger(4), Pressed, Some(ShellHit::Grip(2))),
            Delivery::WindowGesture(ShellHit::Grip(2))
        );
    }

    #[test]
    fn the_shell_overlay_takes_one_touch_point_at_a_time() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(Finger(0), Pressed, Some(ShellHit::Modal)),
            Delivery::ShellButton(Pressed)
        );
        assert_eq!(
            t.route(Tool(0), Pressed, Some(ShellHit::Modal)),
            Delivery::Drop
        );
        assert_eq!(t.route(Tool(0), Released, None), Delivery::Drop);
        assert_eq!(
            t.route(Finger(0), Released, None),
            Delivery::ShellButton(Released)
        );
        assert_eq!(
            t.route(Tool(0), Pressed, Some(ShellHit::Modal)),
            Delivery::ShellButton(Pressed)
        );
    }

    #[test]
    fn the_page_takes_every_touch_point_beside_the_shells() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(Finger(0), Pressed, Some(ShellHit::Controls)),
            Delivery::ShellButton(Pressed)
        );
        assert_eq!(
            t.route(Finger(1), Pressed, Some(ShellHit::Miss)),
            Delivery::Web(Pressed, 0)
        );
        assert_eq!(
            t.route(Finger(2), Pressed, Some(ShellHit::Miss)),
            Delivery::Web(Pressed, 1)
        );
        assert_eq!(
            t.route(Finger(2), Cancelled, None),
            Delivery::Web(Cancelled, 1)
        );
        assert_eq!(
            t.route(Finger(0), Cancelled, None),
            Delivery::ShellButton(Cancelled)
        );
    }

    #[test]
    fn the_page_knows_each_point_by_the_lowest_free_id() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(Finger(7), Pressed, Some(ShellHit::Miss)),
            Delivery::Web(Pressed, 0)
        );
        assert_eq!(
            t.route(Tool(7), Pressed, Some(ShellHit::Miss)),
            Delivery::Web(Pressed, 1)
        );
        assert_eq!(
            t.route(Finger(9), Pressed, Some(ShellHit::Miss)),
            Delivery::Web(Pressed, 2)
        );
        assert_eq!(
            t.route(Finger(7), Released, None),
            Delivery::Web(Released, 0)
        );
        // The freed id is the next point's; the live ones keep theirs.
        assert_eq!(
            t.route(Finger(1), Pressed, Some(ShellHit::Miss)),
            Delivery::Web(Pressed, 0)
        );
        assert_eq!(t.route(Tool(7), Moved, None), Delivery::Web(Moved, 1));
        assert_eq!(t.route(Finger(9), Moved, None), Delivery::Web(Moved, 2));
    }

    #[test]
    fn a_press_with_no_routing_state_reaches_nobody() {
        let mut t = Touches::new();
        assert_eq!(t.route(Finger(0), Pressed, None), Delivery::Drop);
        assert_eq!(
            t.route(Finger(0), Moved, Some(ShellHit::Miss)),
            Delivery::Drop
        );
        assert_eq!(t.route(Finger(0), Released, None), Delivery::Drop);
    }

    #[test]
    fn a_reused_identity_restarts_its_sequence() {
        let mut t = Touches::new();
        assert_eq!(
            t.route(Finger(0), Pressed, Some(ShellHit::Controls)),
            Delivery::ShellButton(Pressed)
        );
        // The shell's held point was replaced, so the shell is free again.
        assert_eq!(
            t.route(Finger(0), Pressed, Some(ShellHit::Modal)),
            Delivery::ShellButton(Pressed)
        );
        assert_eq!(
            t.route(Finger(0), Released, None),
            Delivery::ShellButton(Released)
        );
        assert_eq!(t.route(Finger(0), Released, None), Delivery::Drop);
    }
}
