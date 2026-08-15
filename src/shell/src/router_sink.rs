//! The shell's half of the input router.
//!
//! The router hands the overlay window-space events; this turns them into iced
//! events and posts them to the render actor.

use std::os::raw::c_int;
use std::time::{Duration, Instant};

use iced_core::keyboard::{self, Key, Location, Modifiers, key};
use iced_core::{Event, mouse};
use jfn_platform_abi::LogicalPoint;
use jfn_platform_abi::cursor::CursorShape;
use parking_lot::Mutex;

use crate::actor::{Work, point};

/// `csd.js`'s manual double-click window.
const DOUBLE_PRESS: Duration = Duration::from_millis(400);

static LAST_DRAG_PRESS: Mutex<Option<Instant>> = Mutex::new(None);

/// The shape iced's `mouse_interaction` resolved to for the current frame.
pub(crate) fn set_interaction(interaction: mouse::Interaction) {
    jfn_input::cursor::cursor_from_shell(shape_of(interaction));
}

pub struct ShellSink;

impl jfn_input::ShellInput for ShellSink {
    fn window_gesture(&self, hit: jfn_input::ShellHit) {
        let plat = jfn_platform_abi::get();
        match hit {
            jfn_input::ShellHit::Grip(edge) => {
                *LAST_DRAG_PRESS.lock() = None;
                plat.window_start_resize(edge);
            }
            jfn_input::ShellHit::Drag => {
                let mut last = LAST_DRAG_PRESS.lock();
                if last.is_some_and(|t| t.elapsed() < DOUBLE_PRESS) {
                    *last = None;
                    drop(last);
                    plat.window_toggle_maximize();
                } else {
                    *last = Some(Instant::now());
                    drop(last);
                    plat.window_start_move();
                }
            }
            jfn_input::ShellHit::Modal
            | jfn_input::ShellHit::Controls
            | jfn_input::ShellHit::Miss => {}
        }
    }

    fn context_menu(&self, p: LogicalPoint) {
        jfn_cef::app_menu::open_at(p.x, p.y);
    }

    fn send_key(&self, pressed: bool, modifiers: u32, windows_key_code: c_int, character: u16) {
        let mods = modifiers_from(modifiers);
        let key = key_from(windows_key_code, character);
        let event = if pressed {
            keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key,
                physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
                location: Location::Standard,
                modifiers: mods,
                text: None,
                repeat: false,
            }
        } else {
            keyboard::Event::KeyReleased {
                key: key.clone(),
                modified_key: key,
                physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
                location: Location::Standard,
                modifiers: mods,
            }
        };
        crate::post(Work::Event(Event::Keyboard(event)));
    }

    fn send_text(&self, text: &str) {
        let key = Key::Character(text.into());
        crate::post(Work::Event(Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
            location: Location::Standard,
            modifiers: Modifiers::default(),
            text: Some(text.into()),
            repeat: false,
        })));
    }

    fn send_mouse_move(&self, p: LogicalPoint, _modifiers: u32, leave: bool) {
        let event = if leave {
            mouse::Event::CursorLeft
        } else {
            mouse::Event::CursorMoved {
                position: point(p.x, p.y),
            }
        };
        crate::post(Work::Event(Event::Mouse(event)));
    }

    fn send_mouse_click(
        &self,
        p: LogicalPoint,
        _modifiers: u32,
        button: c_int,
        mouse_up: bool,
        _click_count: c_int,
    ) {
        crate::post(Work::Event(Event::Mouse(mouse::Event::CursorMoved {
            position: point(p.x, p.y),
        })));
        // CEF mouse buttons: 0 = left, 1 = middle, 2 = right.
        let button = match button {
            1 => mouse::Button::Middle,
            2 => mouse::Button::Right,
            _ => mouse::Button::Left,
        };
        let event = if mouse_up {
            mouse::Event::ButtonReleased(button)
        } else {
            mouse::Event::ButtonPressed(button)
        };
        crate::post(Work::Event(Event::Mouse(event)));
    }

    fn send_mouse_wheel(&self, p: LogicalPoint, _modifiers: u32, delta_x: c_int, delta_y: c_int) {
        crate::post(Work::Event(Event::Mouse(mouse::Event::CursorMoved {
            position: point(p.x, p.y),
        })));
        crate::post(Work::Event(Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Pixels {
                x: delta_x as f32,
                y: delta_y as f32,
            },
        })));
    }

    fn set_focus(&self, focus: bool) {
        let event = if focus {
            iced_core::window::Event::Focused
        } else {
            iced_core::window::Event::Unfocused
        };
        crate::post(Work::Event(Event::Window(event)));
    }

    fn edit(&self, command: jfn_input::EditCommand) {
        crate::post(Work::Edit(command));
    }
}

fn modifiers_from(raw: u32) -> Modifiers {
    use jfn_platform_abi::event_flags as ef;
    let mut mods = Modifiers::empty();
    mods.set(Modifiers::SHIFT, raw & ef::EVENTFLAG_SHIFT_DOWN != 0);
    mods.set(Modifiers::CTRL, raw & ef::EVENTFLAG_CONTROL_DOWN != 0);
    mods.set(Modifiers::ALT, raw & ef::EVENTFLAG_ALT_DOWN != 0);
    mods.set(Modifiers::LOGO, raw & ef::EVENTFLAG_COMMAND_DOWN != 0);
    mods
}

/// Windows virtual-key codes, the same set CEF's `KeyEvent` carries.
fn key_from(windows_key_code: c_int, character: u16) -> Key {
    let named = match windows_key_code {
        0x08 => Some(key::Named::Backspace),
        0x09 => Some(key::Named::Tab),
        0x0d => Some(key::Named::Enter),
        0x10 => Some(key::Named::Shift),
        0x11 => Some(key::Named::Control),
        0x12 => Some(key::Named::Alt),
        0x1b => Some(key::Named::Escape),
        0x20 => Some(key::Named::Space),
        0x23 => Some(key::Named::End),
        0x24 => Some(key::Named::Home),
        0x25 => Some(key::Named::ArrowLeft),
        0x26 => Some(key::Named::ArrowUp),
        0x27 => Some(key::Named::ArrowRight),
        0x28 => Some(key::Named::ArrowDown),
        0x2e => Some(key::Named::Delete),
        _ => None,
    };
    match named {
        Some(named) => Key::Named(named),
        None => match char::from_u32(u32::from(character)).filter(|c| !c.is_control()) {
            Some(c) => Key::Character(c.to_string().into()),
            None => Key::Unidentified,
        },
    }
}

fn shape_of(interaction: mouse::Interaction) -> CursorShape {
    use mouse::Interaction as I;
    match interaction {
        I::None | I::Idle => CursorShape::Pointer,
        I::Hidden => CursorShape::None,
        I::ContextMenu => CursorShape::ContextMenu,
        I::Help => CursorShape::Help,
        I::Pointer => CursorShape::Hand,
        I::Progress => CursorShape::Progress,
        I::Wait => CursorShape::Wait,
        I::Cell => CursorShape::Cell,
        I::Crosshair => CursorShape::Cross,
        I::Text => CursorShape::IBeam,
        I::Alias => CursorShape::Alias,
        I::Copy => CursorShape::Copy,
        I::Move => CursorShape::Move,
        I::AllScroll => CursorShape::MiddlePanning,
        I::NoDrop => CursorShape::NoDrop,
        I::NotAllowed => CursorShape::NotAllowed,
        I::Grab => CursorShape::Grab,
        I::Grabbing => CursorShape::Grabbing,
        I::ResizingHorizontally => CursorShape::EastWestResize,
        I::ResizingVertically => CursorShape::NorthSouthResize,
        I::ResizingDiagonallyUp => CursorShape::NorthEastSouthWestResize,
        I::ResizingDiagonallyDown => CursorShape::NorthWestSouthEastResize,
        I::ResizingColumn => CursorShape::ColumnResize,
        I::ResizingRow => CursorShape::RowResize,
        I::ZoomIn => CursorShape::ZoomIn,
        I::ZoomOut => CursorShape::ZoomOut,
    }
}
