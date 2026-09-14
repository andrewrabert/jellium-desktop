//! jellyfin-web's half of the input router.
//!
//! Every command upgrades the process registry's weak client reference at the
//! point of use. Commands remain no-ops while no browser client is available.

use std::os::raw::c_int;

use jfn_input::WebInput;

struct WebSink;

impl WebInput for WebSink {
    fn send_key_event(
        &self,
        type_: c_int,
        modifiers: u32,
        windows_key_code: c_int,
        native_key_code: c_int,
        is_system_key: bool,
        character: u16,
        unmodified_character: u16,
    ) {
        crate::web_overlay::with_current_client(|client| {
            client.send_key_event(
                type_,
                modifiers,
                windows_key_code,
                native_key_code,
                is_system_key,
                character,
                unmodified_character,
            );
        });
    }

    fn send_mouse_click(
        &self,
        x: c_int,
        y: c_int,
        modifiers: u32,
        button: c_int,
        mouse_up: bool,
        click_count: c_int,
    ) {
        crate::web_overlay::with_current_client(|client| {
            client.send_mouse_click(x, y, modifiers, button, mouse_up, click_count);
        });
    }

    fn send_mouse_move(&self, x: c_int, y: c_int, modifiers: u32, leave: bool) {
        crate::web_overlay::with_current_client(|client| {
            client.send_mouse_move(x, y, modifiers, leave);
        });
    }

    fn send_mouse_wheel(&self, x: c_int, y: c_int, modifiers: u32, delta_x: c_int, delta_y: c_int) {
        crate::web_overlay::with_current_client(|client| {
            client.send_mouse_wheel(x, y, modifiers, delta_x, delta_y);
        });
    }

    fn set_focus(&self, focus: bool) {
        crate::web_overlay::with_current_client(|client| {
            client.set_focus(focus);
        });
    }

    fn navigate_history(&self, forward: bool) {
        crate::web_overlay::with_current_client(|client| {
            if forward {
                if client.can_go_forward() {
                    client.go_forward();
                }
            } else if client.can_go_back() {
                client.go_back();
            }
        });
    }

    fn undo(&self) {
        crate::web_overlay::with_current_client(|client| {
            client.frame_undo();
        });
    }

    fn redo(&self) {
        crate::web_overlay::with_current_client(|client| {
            client.frame_redo();
        });
    }

    fn cut(&self) {
        crate::web_overlay::with_current_client(|client| {
            client.frame_cut();
        });
    }

    fn copy(&self) {
        crate::web_overlay::with_current_client(|client| {
            client.frame_copy();
        });
    }

    fn paste(&self) {
        crate::web_overlay::with_current_client(|client| {
            client.frame_paste();
        });
    }

    fn select_all(&self) {
        crate::web_overlay::with_current_client(|client| {
            client.frame_select_all();
        });
    }

    fn is_alive(&self) -> bool {
        crate::web_overlay::with_current_client(|client| client.browser_alive()).unwrap_or(false)
    }
}

pub(crate) fn install() {
    jfn_input::install_web(Box::new(WebSink));
}
