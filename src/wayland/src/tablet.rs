//! Tablet tools (zwp_tablet_v2) on the seat's queue.
//!
//! A tool in contact with the tablet is a touch point: jellyfin-web takes it
//! as a pen or an eraser, everything else exactly as a finger. A hovering tool
//! moves the pointer, and its first and second barrel buttons are the
//! pointer's right and middle buttons. Pads are bound because the tablet seat
//! announces them, and are otherwise ignored.
//!
//! Tool events are applied per zwp_tablet_tool_v2.frame, which groups the
//! events of one hardware state change.

use std::collections::HashMap;

use jfn_input::buttons::{BTN_MIDDLE, BTN_RIGHT, BTN_STYLUS, BTN_STYLUS2};
use jfn_input::{Contact, PointerKind, TouchPoint};
use wayland_client::backend::ObjectId;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::{wl_seat::WlSeat, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, event_created_child};
use wayland_protocols::wp::tablet::zv2::client::{
    zwp_tablet_manager_v2::{self, ZwpTabletManagerV2},
    zwp_tablet_pad_group_v2::{self, ZwpTabletPadGroupV2},
    zwp_tablet_pad_ring_v2::{self, ZwpTabletPadRingV2},
    zwp_tablet_pad_strip_v2::{self, ZwpTabletPadStripV2},
    zwp_tablet_pad_v2::{self, ZwpTabletPadV2},
    zwp_tablet_seat_v2::{self, ZwpTabletSeatV2},
    zwp_tablet_tool_v2::{self, ZwpTabletToolV2},
    zwp_tablet_v2::{self, ZwpTabletV2},
};

use crate::input::State;

/// zwp_tablet_tool_v2.pressure is normalized to 0..=65535 by the protocol.
const PRESSURE_MAX: f32 = 65535.0;

/// The tablet seat and the state of every tool it announced.
#[derive(Default)]
pub(crate) struct Tablets {
    _manager: Option<ZwpTabletManagerV2>,
    _seat: Option<ZwpTabletSeatV2>,
    tools: HashMap<ObjectId, Tool>,
}

impl Tablets {
    /// Binds the tablet seat for `seat`; a compositor without the tablet
    /// manager leaves tablets unsupported.
    pub(crate) fn bind(globals: &GlobalList, qh: &QueueHandle<State>, seat: &WlSeat) -> Self {
        // Version 1 has every event used here; version 2 adds pad dials.
        let Ok(manager) = globals.bind::<ZwpTabletManagerV2, _, _>(qh, 1..=1, ()) else {
            return Self::default();
        };
        let tablet_seat = manager.get_tablet_seat(seat, qh, ());
        Self {
            _manager: Some(manager),
            _seat: Some(tablet_seat),
            tools: HashMap::new(),
        }
    }
}

/// One tool, and the events of the frame being assembled.
struct Tool {
    kind: PointerKind,
    surface: Option<WlSurface>,
    position: Option<(f64, f64)>,
    pressure: f32,
    down: bool,
    /// The barrel buttons held, as the pointer buttons they stand for.
    buttons: Vec<u32>,
    frame: Vec<ToolEvent>,
}

impl Default for Tool {
    fn default() -> Self {
        Self {
            kind: PointerKind::Pen,
            surface: None,
            position: None,
            pressure: 0.0,
            down: false,
            buttons: Vec::new(),
            frame: Vec::new(),
        }
    }
}

enum ToolEvent {
    ProximityIn(WlSurface),
    ProximityOut,
    Down(u32),
    Up,
    Motion(f64, f64),
    Pressure(f32),
    Button {
        serial: u32,
        button: u32,
        pressed: bool,
    },
}

fn contact(tool: &ZwpTabletToolV2) -> Contact {
    Contact::Tool(tool.id().protocol_id())
}

/// The pointer button a barrel button stands for.
fn pointer_button(button: u32) -> Option<u32> {
    match button {
        BTN_STYLUS => Some(BTN_RIGHT),
        BTN_STYLUS2 => Some(BTN_MIDDLE),
        _ => None,
    }
}

impl State {
    fn tool_frame(&mut self, tool: &ZwpTabletToolV2) {
        let id = tool.id();
        let Some(mut t) = self.tablet.tools.remove(&id) else {
            return;
        };
        let contact = contact(tool);
        let modifiers = self.modifiers;
        let frame = std::mem::take(&mut t.frame);
        // The axes describe the frame's hardware state as a whole, so a press
        // or release in the same frame happens at the frame's position.
        let mut axes = false;
        for event in &frame {
            match event {
                ToolEvent::Motion(x, y) => {
                    t.position = Some((*x, *y));
                    axes = true;
                }
                ToolEvent::Pressure(p) => {
                    t.pressure = *p;
                    axes = true;
                }
                _ => {}
            }
        }
        for event in frame {
            match event {
                ToolEvent::ProximityIn(surface) => t.surface = Some(surface),
                ToolEvent::ProximityOut => self.tool_leave(contact, &mut t),
                ToolEvent::Down(serial) => {
                    let (Some(surface), Some(point)) = (t.surface.clone(), point(contact, &t))
                    else {
                        continue;
                    };
                    self.note_press(serial);
                    let consumed = self.dismiss_outside(&surface);
                    self.protocol
                        .touch_down(&surface, point, consumed, modifiers);
                    t.down = true;
                    axes = false;
                }
                ToolEvent::Up => {
                    if t.down && axes {
                        if let Some(point) = point(contact, &t) {
                            self.protocol.touch_motion(point, modifiers);
                        }
                        axes = false;
                    }
                    self.protocol.touch_up(contact, modifiers);
                    t.down = false;
                }
                ToolEvent::Button {
                    serial,
                    button,
                    pressed,
                } => self.tool_button(&mut t, serial, button, pressed),
                ToolEvent::Motion(..) | ToolEvent::Pressure(_) => {}
            }
        }
        if axes {
            match (t.down, point(contact, &t), &t.surface) {
                (true, Some(point), _) => self.protocol.touch_motion(point, modifiers),
                (false, Some(point), Some(surface)) => {
                    self.protocol.motion(surface, (point.x, point.y), modifiers);
                }
                _ => {}
            }
        }
        self.tablet.tools.insert(id, t);
        self.drain_popup_commands();
    }

    fn tool_button(&mut self, t: &mut Tool, serial: u32, button: u32, pressed: bool) {
        let Some(button) = pointer_button(button) else {
            return;
        };
        let Some(surface) = t.surface.clone() else {
            return;
        };
        if pressed {
            if t.buttons.contains(&button) {
                return;
            }
            self.note_press(serial);
            let consumed = self.dismiss_outside(&surface);
            t.buttons.push(button);
            self.protocol
                .button(&surface, button, true, consumed, self.modifiers);
        } else if let Some(at) = t.buttons.iter().position(|b| *b == button) {
            t.buttons.swap_remove(at);
            self.protocol
                .button(&surface, button, false, false, self.modifiers);
        }
    }

    /// The tool left the surface. The compositor ends a contact and releases
    /// the buttons before proximity_out; whatever it left live is ended here.
    fn tool_leave(&mut self, contact: Contact, t: &mut Tool) {
        if t.down {
            self.protocol.touch_cancel(contact, self.modifiers);
            t.down = false;
        }
        if let Some(surface) = t.surface.take() {
            for button in t.buttons.drain(..) {
                self.protocol
                    .button(&surface, button, false, false, self.modifiers);
            }
        }
        t.buttons.clear();
    }

    fn tool_removed(&mut self, tool: &ZwpTabletToolV2) {
        if let Some(mut t) = self.tablet.tools.remove(&tool.id()) {
            self.tool_leave(contact(tool), &mut t);
        }
        tool.destroy();
    }
}

fn point(contact: Contact, t: &Tool) -> Option<TouchPoint> {
    let (x, y) = t.position?;
    Some(TouchPoint {
        contact,
        kind: t.kind,
        x,
        y,
        pressure: t.pressure,
    })
}

impl Dispatch<ZwpTabletManagerV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpTabletManagerV2,
        _: zwp_tablet_manager_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTabletSeatV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpTabletSeatV2,
        _: zwp_tablet_seat_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    event_created_child!(State, ZwpTabletSeatV2, [
        zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE => (ZwpTabletV2, ()),
        zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE => (ZwpTabletToolV2, ()),
        zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE => (ZwpTabletPadV2, ()),
    ]);
}

impl Dispatch<ZwpTabletV2, ()> for State {
    fn event(
        _: &mut Self,
        tablet: &ZwpTabletV2,
        event: zwp_tablet_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_tablet_v2::Event::Removed = event {
            tablet.destroy();
        }
    }
}

impl Dispatch<ZwpTabletToolV2, ()> for State {
    fn event(
        state: &mut Self,
        tool: &ZwpTabletToolV2,
        event: zwp_tablet_tool_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_tablet_tool_v2::Event;
        let pending = match event {
            Event::Frame { .. } => return state.tool_frame(tool),
            Event::Removed => return state.tool_removed(tool),
            Event::Type { tool_type } => {
                let t = state.tablet.tools.entry(tool.id()).or_default();
                t.kind = match tool_type {
                    WEnum::Value(zwp_tablet_tool_v2::Type::Eraser) => PointerKind::Eraser,
                    _ => PointerKind::Pen,
                };
                return;
            }
            Event::ProximityIn { surface, .. } => ToolEvent::ProximityIn(surface),
            Event::ProximityOut => ToolEvent::ProximityOut,
            Event::Down { serial } => ToolEvent::Down(serial),
            Event::Up => ToolEvent::Up,
            Event::Motion { x, y } => ToolEvent::Motion(x, y),
            Event::Pressure { pressure } => ToolEvent::Pressure(pressure as f32 / PRESSURE_MAX),
            Event::Button {
                serial,
                button,
                state: button_state,
            } => ToolEvent::Button {
                serial,
                button,
                pressed: button_state == WEnum::Value(zwp_tablet_tool_v2::ButtonState::Pressed),
            },
            _ => return,
        };
        state
            .tablet
            .tools
            .entry(tool.id())
            .or_default()
            .frame
            .push(pending);
    }
}

impl Dispatch<ZwpTabletPadV2, ()> for State {
    fn event(
        _: &mut Self,
        pad: &ZwpTabletPadV2,
        event: zwp_tablet_pad_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_tablet_pad_v2::Event::Removed = event {
            pad.destroy();
        }
    }

    event_created_child!(State, ZwpTabletPadV2, [
        zwp_tablet_pad_v2::EVT_GROUP_OPCODE => (ZwpTabletPadGroupV2, ()),
    ]);
}

impl Dispatch<ZwpTabletPadGroupV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpTabletPadGroupV2,
        _: zwp_tablet_pad_group_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    event_created_child!(State, ZwpTabletPadGroupV2, [
        zwp_tablet_pad_group_v2::EVT_RING_OPCODE => (ZwpTabletPadRingV2, ()),
        zwp_tablet_pad_group_v2::EVT_STRIP_OPCODE => (ZwpTabletPadStripV2, ()),
    ]);
}

impl Dispatch<ZwpTabletPadRingV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpTabletPadRingV2,
        _: zwp_tablet_pad_ring_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTabletPadStripV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpTabletPadStripV2,
        _: zwp_tablet_pad_strip_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
