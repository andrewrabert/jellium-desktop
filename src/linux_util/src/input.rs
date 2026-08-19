use std::cell::RefCell;
use std::os::raw::c_int;

use xkbcommon::xkb;
use xkbcommon::xkb::compose::{COMPILE_NO_FLAGS, STATE_NO_FLAGS, State, Status, Table};

use crate::keysym;
use jfn_input::key::{KeyReport, PhysicalKey};
use jfn_input::{jfn_input_dispatch_history_nav, jfn_input_dispatch_key};

// The compose state is fed from the key-dispatch thread and read back on the
// same call, so it lives on that thread rather than behind a lock.
thread_local! {
    static COMPOSE: RefCell<Option<Option<State>>> = const { RefCell::new(None) };
}

/// Build the compose table from the current locale. Called once from key
/// dispatch setup; a missing table leaves [`compose_feed`] a permanent
/// `None`.
pub fn compose_init() {
    COMPOSE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return;
        }
        *slot = Some(build_state());
    });
}

fn build_state() -> Option<State> {
    let locale = std::env::var_os("LC_ALL")
        .or_else(|| std::env::var_os("LC_CTYPE"))
        .or_else(|| std::env::var_os("LANG"))
        .unwrap_or_else(|| "C".into());
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let table = Table::new_from_locale(&context, &locale, COMPILE_NO_FLAGS).ok()?;
    Some(State::new(&table, STATE_NO_FLAGS))
}

/// Feed `keysym` to the process's xkb compose state.
///
/// `Some` is the composed text for a completed sequence; `None` means the
/// sequence is still composing, or composing is not active for this keysym
/// and the caller falls back to the xkb-state UTF-8.
pub fn compose_feed(keysym: u32) -> Option<String> {
    compose_init();
    COMPOSE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let state = slot.as_mut()?.as_mut()?;
        state.feed(xkb::Keysym::new(keysym));
        match state.status() {
            Status::Composed => {
                let text = state.utf8();
                state.reset();
                text.filter(|t| !t.is_empty())
            }
            Status::Cancelled => {
                state.reset();
                None
            }
            Status::Composing | Status::Nothing => None,
        }
    })
}

/// Whether the compose state is mid-sequence, so the caller must not fall
/// back to the xkb-state UTF-8 for this key.
pub fn compose_pending() -> bool {
    COMPOSE.with(|slot| {
        let slot = slot.borrow();
        slot.as_ref()
            .and_then(Option::as_ref)
            .is_some_and(|s| s.status() == Status::Composing)
    })
}

pub fn jfn_input_dispatch_key_raw(keysym: u32, native_code: u32, mods: u32, pressed: c_int) {
    // XKB_KEY_XF86Back / XKB_KEY_XF86Forward.
    const XF86_BACK: u32 = 0x1008FF26;
    const XF86_FORWARD: u32 = 0x1008FF27;
    if keysym == XF86_BACK || keysym == XF86_FORWARD {
        if pressed != 0 {
            jfn_input_dispatch_history_nav((keysym == XF86_FORWARD) as c_int);
        }
        return;
    }
    // CEF on Linux expects an X11 keycode (evdev keycode + 8) for
    // native_key_code, which is also the xkb keycode.
    let xkb_code = native_code + 8;
    jfn_input_dispatch_key(KeyReport {
        pressed: pressed != 0,
        modifiers: mods,
        windows_key_code: keysym::keysym_to_vkey(keysym),
        native_key_code: xkb_code as c_int,
        is_system_key: false,
        character: 0,
        unmodified_character: 0,
        logical: keysym_logical_char(keysym),
        physical: PhysicalKey::Xkb(xkb_code as u16),
    });
}

/// The character `keysym` produces with no modifier applied.
pub fn keysym_logical_char(keysym: u32) -> Option<char> {
    jfn_input::key::logical_char(xkb::keysym_to_utf32(xkb::Keysym::new(keysym)))
}
