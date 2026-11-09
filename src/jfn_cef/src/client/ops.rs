//! Input and navigation operations on the browser the web overlay drives.

use cef::{ImplBrowser, ImplBrowserHost, KeyEvent, MouseButtonType, MouseEvent, sys};
use std::os::raw::c_int;

use super::Inner;

impl Inner {
    pub(crate) fn can_go_back(&self) -> bool {
        self.browser_clone().is_some_and(|b| b.can_go_back() == 1)
    }

    pub(crate) fn can_go_forward(&self) -> bool {
        self.browser_clone()
            .is_some_and(|b| b.can_go_forward() == 1)
    }

    pub(crate) fn go_back(&self) {
        if let Some(b) = self.browser_clone() {
            b.go_back();
        }
    }

    pub(crate) fn go_forward(&self) {
        if let Some(b) = self.browser_clone() {
            b.go_forward();
        }
    }

    pub(crate) fn set_focus(&self, focus: bool) {
        if let Some(host) = self.host() {
            host.set_focus(if focus { 1 } else { 0 });
        }
    }

    #[allow(clippy::too_many_arguments)] // mirrors CEF's KeyEvent layout 1:1
    pub(crate) fn send_key_event(
        &self,
        type_: c_int,
        modifiers: u32,
        windows_key_code: c_int,
        native_key_code: c_int,
        is_system_key: bool,
        character: u16,
        unmodified_character: u16,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        let raw_type: sys::cef_key_event_type_t = unsafe { std::mem::transmute(type_ as u32) };
        let ev = KeyEvent {
            type_: raw_type.into(),
            modifiers,
            windows_key_code,
            native_key_code,
            is_system_key: if is_system_key { 1 } else { 0 },
            character,
            unmodified_character,
            ..KeyEvent::default()
        };
        host.send_key_event(Some(&ev));
    }

    pub(crate) fn send_mouse_click(
        &self,
        x: c_int,
        y: c_int,
        modifiers: u32,
        button: c_int,
        mouse_up: bool,
        click_count: c_int,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        let me = MouseEvent { x, y, modifiers };
        let raw_btn: sys::cef_mouse_button_type_t = unsafe { std::mem::transmute(button as u32) };
        host.send_mouse_click_event(
            Some(&me),
            MouseButtonType::from(raw_btn),
            if mouse_up { 1 } else { 0 },
            click_count,
        );
    }

    pub(crate) fn send_mouse_move(&self, x: c_int, y: c_int, modifiers: u32, leave: bool) {
        let Some(host) = self.host() else {
            return;
        };
        let me = MouseEvent { x, y, modifiers };
        host.send_mouse_move_event(Some(&me), if leave { 1 } else { 0 });
    }

    pub(crate) fn send_mouse_wheel(
        &self,
        x: c_int,
        y: c_int,
        modifiers: u32,
        dx: c_int,
        dy: c_int,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        let me = MouseEvent { x, y, modifiers };
        host.send_mouse_wheel_event(Some(&me), dx, dy);
    }
}
