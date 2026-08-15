//! jellyfin-web's half of the input router.
//!
//! The router hands it points already in the overlay's own space, so it
//! reports no inset of its own.

use std::os::raw::c_int;

use jfn_input::WebInput;

use crate::web_overlay::WebOverlay;

struct WebSink {
    overlay: WebOverlay,
}

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
        self.overlay.client().send_key_event(
            type_,
            modifiers,
            windows_key_code,
            native_key_code,
            is_system_key,
            character,
            unmodified_character,
        );
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
        self.overlay
            .client()
            .send_mouse_click(x, y, modifiers, button, mouse_up, click_count);
    }

    fn send_mouse_move(&self, x: c_int, y: c_int, modifiers: u32, leave: bool) {
        self.overlay
            .client()
            .send_mouse_move(x, y, modifiers, leave);
    }

    fn send_mouse_wheel(&self, x: c_int, y: c_int, modifiers: u32, delta_x: c_int, delta_y: c_int) {
        self.overlay
            .client()
            .send_mouse_wheel(x, y, modifiers, delta_x, delta_y);
    }

    fn set_focus(&self, focus: bool) {
        self.overlay.client().set_focus(focus);
    }

    fn navigate_history(&self, forward: bool) {
        let client = self.overlay.client();
        if forward {
            if client.can_go_forward() {
                client.go_forward();
            }
        } else if client.can_go_back() {
            client.go_back();
        }
    }

    fn undo(&self) {
        self.overlay.client().frame_undo();
    }

    fn redo(&self) {
        self.overlay.client().frame_redo();
    }

    fn cut(&self) {
        self.overlay.client().frame_cut();
    }

    fn copy(&self) {
        self.overlay.client().frame_copy();
    }

    fn paste(&self) {
        self.overlay.client().frame_paste();
    }

    fn select_all(&self) {
        self.overlay.client().frame_select_all();
    }

    fn is_alive(&self) -> bool {
        self.overlay.client().browser_alive()
    }
}

pub(crate) fn install(overlay: WebOverlay) {
    jfn_input::install_web(Box::new(WebSink { overlay }));
}
