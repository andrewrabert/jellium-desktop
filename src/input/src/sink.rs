//! The two consumers the router feeds, and the slots they install into.
//!
//! Each slot is filled once at boot — jellyfin-web's CEF layer fills the web
//! slot, the shell overlay fills the shell slot — and an unfilled slot drops
//! everything routed to it.

use jfn_platform_abi::LogicalPoint;
use parking_lot::Mutex;
use std::os::raw::c_int;
use std::sync::{Arc, OnceLock};

pub trait WebInput: Send + Sync {
    #[allow(clippy::too_many_arguments)] // mirrors CEF's KeyEvent layout 1:1
    fn send_key_event(
        &self,
        type_: c_int,
        modifiers: u32,
        windows_key_code: c_int,
        native_key_code: c_int,
        is_system_key: bool,
        character: u16,
        unmodified_character: u16,
    );
    fn send_mouse_click(
        &self,
        x: c_int,
        y: c_int,
        modifiers: u32,
        button: c_int,
        mouse_up: bool,
        click_count: c_int,
    );
    fn send_mouse_move(&self, x: c_int, y: c_int, modifiers: u32, leave: bool);
    fn send_mouse_wheel(&self, x: c_int, y: c_int, modifiers: u32, delta_x: c_int, delta_y: c_int);
    fn set_focus(&self, focus: bool);
    fn navigate_history(&self, forward: bool);
    fn undo(&self);
    fn redo(&self);
    fn cut(&self);
    fn copy(&self);
    fn paste(&self);
    fn select_all(&self);
    fn is_alive(&self) -> bool;
}

pub trait ShellInput: Send + Sync {
    /// A press on [`crate::route::ShellHit::Drag`] or
    /// [`crate::route::ShellHit::Grip`]. The implementation calls
    /// `Platform::window_start_move` / `window_start_resize` on the press
    /// itself, and a second press on the drag region inside 400 ms calls
    /// `window_toggle_maximize` instead. Never reaches the widget tree.
    fn window_gesture(&self, hit: crate::route::ShellHit);
    /// A right-press anywhere the shell owns, modal views included. Raises the
    /// app menu through
    /// `Platform::menu_delivery(MenuKind::ContextMenu)`.
    fn context_menu(&self, p: LogicalPoint);
    /// A key press or release, never typed text: a character the user typed
    /// arrives through [`ShellInput::send_text`], so `character` serves only the
    /// shortcut combinations iced resolves from the key itself.
    fn send_key(&self, pressed: bool, modifiers: u32, windows_key_code: c_int, character: u16);
    fn send_text(&self, text: &str);
    fn send_mouse_move(&self, p: LogicalPoint, modifiers: u32, leave: bool);
    fn send_mouse_click(
        &self,
        p: LogicalPoint,
        modifiers: u32,
        button: c_int,
        mouse_up: bool,
        click_count: c_int,
    );
    fn send_mouse_wheel(&self, p: LogicalPoint, modifiers: u32, delta_x: c_int, delta_y: c_int);
    fn set_focus(&self, focus: bool);
    fn edit(&self, command: EditCommand);
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EditCommand {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
}

static WEB: OnceLock<Box<dyn WebInput>> = OnceLock::new();
static SHELL: OnceLock<Box<dyn ShellInput>> = OnceLock::new();

pub fn install_web(w: Box<dyn WebInput>) {
    let _ = WEB.set(w);
}

pub fn install_shell(s: Box<dyn ShellInput>) {
    let _ = SHELL.set(s);
}

type StateListener = Box<dyn Fn(crate::route::ShellState) + Send + Sync>;

static STATE: Mutex<Option<crate::route::ShellState>> = Mutex::new(None);
static STATE_LISTENERS: Mutex<Vec<Arc<StateListener>>> = Mutex::new(Vec::new());

/// Publish the shell overlay's routing state. The shell overlay is the only
/// publisher; a change of `modal_open` moves keyboard focus off or back onto
/// the web overlay.
pub fn publish_shell_state(state: crate::route::ShellState) {
    let modal_flipped = {
        let mut current = STATE.lock();
        let flipped = current.is_none_or(|prev| prev.modal_open != state.modal_open);
        *current = Some(state);
        flipped
    };
    if modal_flipped {
        with_web(|w| w.set_focus(!state.modal_open));
    }
    let listeners: Vec<Arc<StateListener>> = STATE_LISTENERS.lock().clone();
    for f in listeners {
        f(state);
    }
}

/// The shell overlay's routing state, `None` until it has published one.
pub fn shell_state() -> Option<crate::route::ShellState> {
    *STATE.lock()
}

/// Runs `f` with the published state at registration and on every later
/// publication.
pub fn on_shell_state(f: StateListener) {
    let f = Arc::new(f);
    let seed = {
        let mut listeners = STATE_LISTENERS.lock();
        listeners.push(Arc::clone(&f));
        *STATE.lock()
    };
    if let Some(state) = seed {
        f(state);
    }
}

pub(crate) fn with_web<F: FnOnce(&dyn WebInput)>(f: F) {
    if let Some(w) = WEB.get()
        && w.is_alive()
    {
        f(&**w);
    }
}

pub(crate) fn with_shell<F: FnOnce(&dyn ShellInput)>(f: F) {
    if let Some(s) = SHELL.get() {
        f(&**s);
    }
}
