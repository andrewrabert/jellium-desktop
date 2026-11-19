//! Seat input ownership. Destinations are registered with protocol objects;
//! application visibility never participates in event delivery.
use std::collections::HashMap;
use std::sync::Arc;

use jfn_input::TouchPhase;
use jfn_input::buttons::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use jfn_platform_abi::Generation;
use jfn_platform_abi::event_flags::{
    EVENTFLAG_LEFT_MOUSE_BUTTON, EVENTFLAG_MIDDLE_MOUSE_BUTTON, EVENTFLAG_RIGHT_MOUSE_BUTTON,
};
use smithay_client_toolkit::seat::keyboard::KeyEvent;
use wayland_client::{Proxy, backend::ObjectId, protocol::wl_surface::WlSurface};

#[derive(Clone, Debug)]
pub(crate) struct SurfacePoint {
    surface: WlSurface,
    x: f64,
    y: f64,
}

impl SurfacePoint {
    pub(crate) fn from_event(surface: &WlSurface, position: (f64, f64)) -> Self {
        Self {
            surface: surface.clone(),
            x: position.0,
            y: position.1,
        }
    }

    pub(crate) fn surface(&self) -> &WlSurface {
        &self.surface
    }

    fn moved(&self, position: (f64, f64)) -> Self {
        Self {
            surface: self.surface.clone(),
            x: position.0,
            y: position.1,
        }
    }

    pub(crate) fn window_point(&self, root: &WlSurface) -> Option<jfn_platform_abi::WindowPoint> {
        (&self.surface == root).then(|| jfn_platform_abi::WindowPoint::from_window(self.x, self.y))
    }

    pub(crate) fn menu_point(&self, popup: &WlSurface) -> Option<jfn_linux_util::menu::MenuPoint> {
        (&self.surface == popup).then(|| jfn_linux_util::menu::MenuPoint::Logical {
            x: self.x.floor() as i32,
            y: self.y.floor() as i32,
        })
    }
}

pub(crate) trait InputTarget: Send + Sync {
    fn registered(&self, _surface: &WlSurface) {}
    fn configured(&self, _size: jfn_platform_abi::LogicalSize) {}
    fn dismissed(&self) {}
    fn motion(&self, _at: &SurfacePoint, _modifiers: u32) {}
    fn leave(&self, _at: &SurfacePoint, _modifiers: u32) {}
    fn button(&self, _button: u32, _pressed: bool, _at: &SurfacePoint, _modifiers: u32) {}
    fn pointer_cancel(&self) {}
    fn scroll(&self, _at: &SurfacePoint, _dx: i32, _dy: i32, _modifiers: u32) {}
    fn key(&self, _event: &KeyEvent, _pressed: bool, _modifiers: u32) {}
    /// A touch point. A destination without touch input takes it as its left
    /// button.
    fn touch(&self, _id: i32, phase: TouchPhase, at: &SurfacePoint, modifiers: u32) {
        match phase {
            TouchPhase::Pressed => {
                self.motion(at, modifiers);
                self.button(BTN_LEFT, true, at, modifiers | EVENTFLAG_LEFT_MOUSE_BUTTON);
            }
            TouchPhase::Moved => {
                self.motion(at, modifiers | EVENTFLAG_LEFT_MOUSE_BUTTON);
            }
            TouchPhase::Released => {
                self.button(BTN_LEFT, false, at, modifiers);
                self.leave(at, modifiers);
            }
            TouchPhase::Cancelled => self.leave(at, modifiers),
        }
    }
}

struct Destination {
    // Retaining the full object identity distinguishes reused wire IDs.
    surface: WlSurface,
    parent: Option<ObjectId>,
    target: Arc<dyn InputTarget>,
}

#[derive(Default)]
pub(crate) struct SeatInput {
    destinations: HashMap<ObjectId, Destination>,
    keyboard: Option<ObjectId>,
    buttons: HashMap<u32, Option<ObjectId>>,
    keys: HashMap<u32, (ObjectId, KeyEvent)>,
    touches: HashMap<i32, (Option<ObjectId>, SurfacePoint)>,
    grabs: Vec<(ObjectId, Generation)>,
    pub(crate) focus_epoch: u64,
    pub(crate) focused: bool,
}

impl SeatInput {
    pub(crate) fn register(&mut self, surface: WlSurface, target: Arc<dyn InputTarget>) {
        target.registered(&surface);
        self.destinations.insert(
            surface.id(),
            Destination {
                surface,
                parent: None,
                target,
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
        target.registered(&surface);
        self.destinations.insert(
            id.clone(),
            Destination {
                surface,
                parent: Some(parent),
                target,
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

    pub(crate) fn leave(&mut self, at: &SurfacePoint, modifiers: u32) {
        let id = at.surface().id();
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
                    at,
                    modifiers | self.button_modifiers(&id),
                );
            }
        }
        if let Some(destination) = self.destinations.get(&id) {
            destination.target.leave(at, modifiers);
        }
    }

    pub(crate) fn motion(&self, at: &SurfacePoint, modifiers: u32) {
        let id = at.surface().id();
        let modifiers = modifiers | self.button_modifiers(&id);
        if let Some(destination) = self.destinations.get(&id) {
            destination.target.motion(at, modifiers);
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
        at: &SurfacePoint,
        button: u32,
        pressed: bool,
        consumed: bool,
        modifiers: u32,
    ) {
        let event = at.surface().id();
        let owner = if pressed {
            let owner = (!consumed).then(|| event.clone());
            // A fresh press starts a new sequence even if the compositor ended
            // an earlier grab without delivering its release.
            self.buttons.insert(button, owner.clone());
            owner
        } else {
            self.buttons.remove(&button).flatten()
        };
        let Some(id) = owner else { return };
        let Some(destination) = self.destinations.get(&id) else {
            return;
        };
        if id == event {
            destination
                .target
                .button(button, pressed, at, modifiers | self.button_modifiers(&id));
        } else {
            destination.target.pointer_cancel();
        }
    }

    pub(crate) fn scroll(&self, at: &SurfacePoint, dx: i32, dy: i32, modifiers: u32) {
        let id = at.surface().id();
        if let Some(destination) = self.destinations.get(&id) {
            destination
                .target
                .scroll(at, dx, dy, modifiers | self.button_modifiers(&id));
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

    pub(crate) fn touch_down(&mut self, at: SurfacePoint, id: i32, consumed: bool, modifiers: u32) {
        // A down under a live id means the compositor ended the old sequence
        // without an up; its owner is told it was cancelled.
        self.touch_end(id, TouchPhase::Cancelled, modifiers);
        let owner = (!consumed).then(|| at.surface().id());
        self.touch_deliver(owner.as_ref(), id, TouchPhase::Pressed, &at, modifiers);
        self.touches.insert(id, (owner, at));
    }

    pub(crate) fn touch_motion(&mut self, id: i32, position: (f64, f64), modifiers: u32) {
        let Some((owner, last)) = self.touches.get_mut(&id) else {
            return;
        };
        *last = last.moved(position);
        let (owner, at) = (owner.clone(), last.clone());
        self.touch_deliver(owner.as_ref(), id, TouchPhase::Moved, &at, modifiers);
    }

    pub(crate) fn touch_up(&mut self, id: i32, modifiers: u32) {
        self.touch_end(id, TouchPhase::Released, modifiers);
    }

    fn touch_end(&mut self, id: i32, phase: TouchPhase, modifiers: u32) {
        if let Some((owner, at)) = self.touches.remove(&id) {
            self.touch_deliver(owner.as_ref(), id, phase, &at, modifiers);
        }
    }

    fn touch_deliver(
        &self,
        owner: Option<&ObjectId>,
        id: i32,
        phase: TouchPhase,
        at: &SurfacePoint,
        modifiers: u32,
    ) {
        if let Some(destination) = owner.and_then(|owner| self.destinations.get(owner)) {
            destination.target.touch(id, phase, at, modifiers);
        }
    }

    /// wl_touch.cancel, or the touch capability going away: every live point
    /// is cancelled at its owner.
    pub(crate) fn cancel_touch(&mut self, modifiers: u32) {
        let ids: Vec<_> = self.touches.keys().copied().collect();
        for id in ids {
            self.touch_end(id, TouchPhase::Cancelled, modifiers);
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

    pub(crate) fn cancel_pointer(&mut self) {
        for destination in self.destinations.values() {
            destination.target.pointer_cancel();
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
