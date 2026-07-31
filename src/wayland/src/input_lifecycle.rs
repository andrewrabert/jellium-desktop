// lifecycle_init forwards a wl_display* the app already owns to the
// unsafe input thread init; the function exists for callers that don't
// want to mark themselves unsafe just to pass a pointer through.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

//! Lifecycle wrapper around the Wayland input thread.
//!
//! Owns the static `InputThread` handle, builds the input thread's
//! `Callbacks` struct from the input crate's dispatch shims, and exposes
//! the Platform-vtable cursor setter.

use std::ffi::c_void;

use crate::input::Callbacks;
use jfn_platform_abi::cursor::CursorShape;

use jfn_input::{
    jfn_input_dispatch_char, jfn_input_dispatch_history_nav, jfn_input_dispatch_keyboard_focus,
    jfn_input_dispatch_mouse_button, jfn_input_dispatch_mouse_move, jfn_input_dispatch_scroll,
};
use jfn_linux_util::input::jfn_input_dispatch_key_raw;

const CALLBACKS: Callbacks = Callbacks {
    mouse_move: Some(jfn_input_dispatch_mouse_move),
    mouse_button: Some(jfn_input_dispatch_mouse_button),
    scroll: Some(jfn_input_dispatch_scroll),
    history_nav: Some(jfn_input_dispatch_history_nav),
    kb_focus: Some(jfn_input_dispatch_keyboard_focus),
    key: Some(jfn_input_dispatch_key_raw),
    char_: Some(jfn_input_dispatch_char),
};

pub fn lifecycle_init(rt: &'static crate::runtime::WlRuntime, display: *mut c_void) {
    let Some(thread) = crate::input::init(rt, display, &CALLBACKS) else {
        return;
    };
    let _ = rt.set_input(thread);
}

pub fn lifecycle_cleanup(rt: &'static crate::runtime::WlRuntime) {
    if let Some(input) = rt.input() {
        input.shutdown(rt);
    }
}

pub fn set_cursor_active(rt: &crate::runtime::WlRuntime, shape: CursorShape) {
    if let Some(input) = rt.input() {
        input.set_cursor(shape.as_raw() as u32);
    }
}
