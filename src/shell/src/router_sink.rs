//! The shell's half of the input router.
//!
//! The router hands the overlay window-space events; this turns them into iced
//! events and posts them to the render actor.

use std::os::raw::c_int;
use std::time::{Duration, Instant};

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
        let Some(controls) = jfn_platform_abi::get().titlebar_controls() else {
            return;
        };
        match hit {
            jfn_input::ShellHit::Grip(edge) => {
                *LAST_DRAG_PRESS.lock() = None;
                controls.start_resize(edge);
            }
            jfn_input::ShellHit::Drag => {
                let mut last = LAST_DRAG_PRESS.lock();
                if last.is_some_and(|t| t.elapsed() < DOUBLE_PRESS) {
                    *last = None;
                    drop(last);
                    controls.toggle_maximize();
                } else {
                    *last = Some(Instant::now());
                    drop(last);
                    controls.start_move();
                }
            }
            jfn_input::ShellHit::Modal
            | jfn_input::ShellHit::Controls
            | jfn_input::ShellHit::Miss => {}
        }
    }

    fn context_menu(&self, p: LogicalPoint) {
        crate::post(Work::ContextMenu(p));
    }

    fn send_key(&self, key: jfn_input::key::ShellKey) {
        let backend = jfn_platform_abi::get().display();
        if crate::key::opens_edit_menu(backend, key) {
            crate::post(Work::EditMenuAtCaret);
            return;
        }
        crate::post(Work::Event(Event::Keyboard(crate::key::key_event(key))));
    }

    fn send_text(&self, text: &str) {
        for ch in text.chars() {
            crate::post(Work::Event(Event::Keyboard(crate::key::text_event(ch))));
        }
    }

    fn primary_paste(&self, p: LogicalPoint) {
        crate::post(Work::PrimaryPaste(p));
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
        crate::post(Work::EditAt {
            field: crate::actor::Target::Focused,
            command,
        });
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
