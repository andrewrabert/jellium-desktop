//! Seat input ownership. Destinations are registered with protocol objects;
//! application visibility never participates in event delivery.
use std::collections::HashMap;
use std::sync::Arc;

use jfn_input::buttons::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use jfn_input::{Contact, TouchPhase, TouchPoint};
use jfn_platform_abi::Generation;
use jfn_platform_abi::event_flags::{
    EVENTFLAG_LEFT_MOUSE_BUTTON, EVENTFLAG_MIDDLE_MOUSE_BUTTON, EVENTFLAG_RIGHT_MOUSE_BUTTON,
};
use smithay_client_toolkit::seat::keyboard::KeyEvent;
use wayland_client::{Proxy, backend::ObjectId, protocol::wl_surface::WlSurface};

pub(crate) trait InputTarget: Send + Sync {
    fn configured(&self) {}
    fn dismissed(&self) {}
    fn motion(&self, _position: (f64, f64), _modifiers: u32, _leave: bool) {}
    fn button(&self, _button: u32, _pressed: bool, _position: (f64, f64), _modifiers: u32) {}
    fn scroll(&self, _position: (f64, f64), _dx: i32, _dy: i32, _modifiers: u32) {}
    fn key(&self, _event: &KeyEvent, _pressed: bool, _modifiers: u32) {}
    /// A touch point. A destination without touch input takes it as its left
    /// button.
    fn touch(&self, point: TouchPoint, phase: TouchPhase, modifiers: u32) {
        let position = (point.x, point.y);
        match phase {
            TouchPhase::Pressed => {
                self.motion(position, modifiers, false);
                self.button(
                    BTN_LEFT,
                    true,
                    position,
                    modifiers | EVENTFLAG_LEFT_MOUSE_BUTTON,
                );
            }
            TouchPhase::Moved => {
                self.motion(position, modifiers | EVENTFLAG_LEFT_MOUSE_BUTTON, false);
            }
            TouchPhase::Released => {
                self.button(BTN_LEFT, false, position, modifiers);
                self.motion(position, modifiers, true);
            }
            TouchPhase::Cancelled => self.motion(position, modifiers, true),
        }
    }
}

struct Destination {
    // Retaining the full object identity distinguishes reused wire IDs.
    surface: WlSurface,
    parent: Option<ObjectId>,
    target: Arc<dyn InputTarget>,
    position: (f64, f64),
}

#[derive(Default)]
pub(crate) struct SeatInput {
    destinations: HashMap<ObjectId, Destination>,
    pointer: Option<ObjectId>,
    keyboard: Option<ObjectId>,
    buttons: HashMap<u32, Option<ObjectId>>,
    keys: HashMap<u32, (ObjectId, KeyEvent)>,
    /// Each live touch point's owner, `None` when its press was consumed, and
    /// its last state: a release reports none of its own.
    touches: HashMap<Contact, (Option<ObjectId>, TouchPoint)>,
    grabs: Vec<(ObjectId, Generation)>,
    pub(crate) focus_epoch: u64,
    pub(crate) focused: bool,
}

impl SeatInput {
    pub(crate) fn register(&mut self, surface: WlSurface, target: Arc<dyn InputTarget>) {
        self.destinations.insert(
            surface.id(),
            Destination {
                surface,
                parent: None,
                target,
                position: (0.0, 0.0),
            },
        );
    }

    pub(crate) fn register_popup(
        &mut self,
        surface: WlSurface,
        parent: ObjectId,
        generation: Generation,
        target: Arc<dyn InputTarget>,
    ) {
        let id = surface.id();
        self.destinations.insert(
            id.clone(),
            Destination {
                surface,
                parent: Some(parent),
                target,
                position: (0.0, 0.0),
            },
        );
        self.grabs.push((id, generation));
    }

    pub(crate) fn retire(&mut self, id: &ObjectId) {
        self.destinations.remove(id);
        self.grabs.retain(|(surface, _)| surface != id);
        // Keep outstanding sequence records until release. A retired recipient
        // receives nothing, and its release cannot fall through to a new one.
    }

    pub(crate) fn enter(&mut self, surface: &WlSurface, position: (f64, f64), modifiers: u32) {
        self.pointer = Some(surface.id());
        self.motion(surface, position, modifiers);
    }

    pub(crate) fn leave(&mut self, surface: &WlSurface, modifiers: u32) {
        let id = surface.id();
        let buttons: Vec<_> = self
            .buttons
            .iter()
            .filter(|(_, owner)| owner.as_ref() == Some(&id))
            .map(|(button, _)| *button)
            .collect();
        for button in buttons {
            self.buttons.insert(button, None);
            if let Some(destination) = self.destinations.get(&id) {
                destination.target.button(
                    button,
                    false,
                    destination.position,
                    modifiers | self.button_modifiers(&id),
                );
            }
        }
        if let Some(destination) = self.destinations.get(&id) {
            destination
                .target
                .motion(destination.position, modifiers, true);
        }
        if self.pointer.as_ref() == Some(&id) {
            self.pointer = None;
        }
    }

    pub(crate) fn motion(&mut self, surface: &WlSurface, position: (f64, f64), modifiers: u32) {
        let modifiers = modifiers | self.button_modifiers(&surface.id());
        if let Some(destination) = self.destinations.get_mut(&surface.id()) {
            destination.position = position;
            destination.target.motion(position, modifiers, false);
        }
    }

    fn descends_from(&self, mut id: ObjectId, ancestor: &ObjectId) -> bool {
        loop {
            if &id == ancestor {
                return true;
            }
            let Some(parent) = self.destinations.get(&id).and_then(|d| d.parent.clone()) else {
                return false;
            };
            id = parent;
        }
    }

    /// xdg_popup owner-events dismissal. Returns roles to destroy, topmost
    /// first. This is a relationship between protocol objects, not UI types.
    pub(crate) fn outside_press(&self, surface: &WlSurface) -> Vec<Generation> {
        self.grabs
            .iter()
            .rev()
            .take_while(|(popup, _)| !self.descends_from(surface.id(), popup))
            .map(|(_, generation)| *generation)
            .collect()
    }

    pub(crate) fn button(
        &mut self,
        surface: &WlSurface,
        button: u32,
        pressed: bool,
        consumed: bool,
        modifiers: u32,
    ) {
        let owner = if pressed {
            let owner = (!consumed).then(|| surface.id());
            // A fresh press starts a new sequence even if the compositor ended
            // an earlier grab without delivering its release.
            self.buttons.insert(button, owner.clone());
            owner
        } else {
            self.buttons.remove(&button).flatten()
        };
        if let Some(id) = owner
            && let Some(destination) = self.destinations.get(&id)
        {
            destination.target.button(
                button,
                pressed,
                destination.position,
                modifiers | self.button_modifiers(&id),
            );
        }
    }

    pub(crate) fn scroll(&self, dx: i32, dy: i32, modifiers: u32) {
        if let Some(destination) = self
            .pointer
            .as_ref()
            .and_then(|id| self.destinations.get(id))
        {
            destination.target.scroll(
                destination.position,
                dx,
                dy,
                modifiers | self.button_modifiers(&destination.surface.id()),
            );
        }
    }

    fn button_modifiers(&self, id: &ObjectId) -> u32 {
        self.buttons
            .iter()
            .filter(|(_, owner)| owner.as_ref() == Some(id))
            .fold(0, |flags, (button, _)| {
                flags
                    | match *button {
                        BTN_LEFT => EVENTFLAG_LEFT_MOUSE_BUTTON,
                        BTN_MIDDLE => EVENTFLAG_MIDDLE_MOUSE_BUTTON,
                        BTN_RIGHT => EVENTFLAG_RIGHT_MOUSE_BUTTON,
                        _ => 0,
                    }
            })
    }

    pub(crate) fn touch_down(
        &mut self,
        surface: &WlSurface,
        point: TouchPoint,
        consumed: bool,
        modifiers: u32,
    ) {
        // A down under a live identity means the compositor ended the old
        // sequence without an up; its owner is told it was cancelled.
        self.touch_end(point.contact, TouchPhase::Cancelled, modifiers);
        let owner = (!consumed).then(|| surface.id());
        self.touches.insert(point.contact, (owner.clone(), point));
        self.touch_deliver(owner.as_ref(), point, TouchPhase::Pressed, modifiers);
    }

    pub(crate) fn touch_motion(&mut self, point: TouchPoint, modifiers: u32) {
        let Some((owner, last)) = self.touches.get_mut(&point.contact) else {
            return;
        };
        *last = point;
        let owner = owner.clone();
        self.touch_deliver(owner.as_ref(), point, TouchPhase::Moved, modifiers);
    }

    pub(crate) fn touch_up(&mut self, contact: Contact, modifiers: u32) {
        self.touch_end(contact, TouchPhase::Released, modifiers);
    }

    pub(crate) fn touch_cancel(&mut self, contact: Contact, modifiers: u32) {
        self.touch_end(contact, TouchPhase::Cancelled, modifiers);
    }

    fn touch_end(&mut self, contact: Contact, phase: TouchPhase, modifiers: u32) {
        if let Some((owner, point)) = self.touches.remove(&contact) {
            self.touch_deliver(owner.as_ref(), point, phase, modifiers);
        }
    }

    fn touch_deliver(
        &self,
        owner: Option<&ObjectId>,
        point: TouchPoint,
        phase: TouchPhase,
        modifiers: u32,
    ) {
        if let Some(destination) = owner.and_then(|owner| self.destinations.get(owner)) {
            destination.target.touch(point, phase, modifiers);
        }
    }

    /// Cancels every live point `which` selects at its owner: the fingers on
    /// wl_touch.cancel or when the touch capability goes away, every point
    /// when the seat does.
    pub(crate) fn cancel_touch(&mut self, which: fn(&Contact) -> bool, modifiers: u32) {
        let contacts: Vec<_> = self.touches.keys().copied().filter(which).collect();
        for contact in contacts {
            self.touch_end(contact, TouchPhase::Cancelled, modifiers);
        }
    }

    pub(crate) fn keyboard_enter(&mut self, surface: &WlSurface) {
        self.focus_epoch = self.focus_epoch.wrapping_add(1);
        self.keyboard = Some(surface.id());
    }

    pub(crate) fn keyboard_leave(&mut self, surface: &WlSurface, modifiers: u32) {
        let id = surface.id();
        let keys: Vec<_> = self
            .keys
            .iter()
            .filter(|(_, (owner, _))| owner == &id)
            .map(|(code, _)| *code)
            .collect();
        for code in keys {
            if let Some((_, event)) = self.keys.remove(&code)
                && let Some(destination) = self.destinations.get(&id)
            {
                destination.target.key(&event, false, modifiers);
            }
        }
        if self.keyboard.as_ref() == Some(&id) {
            self.keyboard = None;
            self.focus_epoch = self.focus_epoch.wrapping_add(1);
        }
    }

    pub(crate) fn has_keyboard_focus(&self) -> bool {
        self.keyboard
            .as_ref()
            .is_some_and(|id| self.destinations.contains_key(id))
    }

    pub(crate) fn key(&mut self, event: &KeyEvent, pressed: bool, modifiers: u32) {
        let owner = if pressed {
            let owner = self.keyboard.clone();
            if let Some(id) = &owner {
                self.keys
                    .insert(event.raw_code, (id.clone(), event.clone()));
            }
            owner
        } else {
            self.keys.remove(&event.raw_code).map(|(id, _)| id)
        };
        if let Some(destination) = owner.and_then(|id| self.destinations.get(&id)) {
            destination.target.key(event, pressed, modifiers);
        }
    }

    pub(crate) fn cancel_pointer(&mut self, modifiers: u32) {
        let surfaces: Vec<_> = self
            .destinations
            .values()
            .map(|d| d.surface.clone())
            .collect();
        for surface in surfaces {
            self.leave(&surface, modifiers);
        }
        self.buttons.clear();
    }

    pub(crate) fn cancel_keyboard(&mut self, modifiers: u32) {
        let surfaces: Vec<_> = self
            .destinations
            .values()
            .map(|d| d.surface.clone())
            .collect();
        for surface in surfaces {
            self.keyboard_leave(&surface, modifiers);
        }
        self.keys.clear();
        self.keyboard = None;
        self.focus_epoch = self.focus_epoch.wrapping_add(1);
    }
}
