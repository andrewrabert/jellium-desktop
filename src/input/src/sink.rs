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
    /// [`crate::route::ShellHit::Grip`]. The implementation reaches the
    /// window controls through `Platform::titlebar_controls` and calls
    /// `TitlebarControls::start_move` / `start_resize` on the press itself;
    /// a second press on the drag region inside 400 ms calls
    /// `toggle_maximize` instead. Never reaches the widget tree.
    fn window_gesture(&self, hit: crate::route::ShellHit);
    /// A right-press anywhere the shell owns, modal views included. Raises the
    /// app menu through
    /// `Platform::menu_delivery(MenuKind::ContextMenu)`.
    fn context_menu(&self, p: LogicalPoint);
    /// A key press or release, never typed text: a character the user typed
    /// arrives through [`ShellInput::send_text`], so the key identity serves
    /// only the shortcut combinations iced resolves from the key itself.
    fn send_key(&self, key: crate::key::ShellKey);
    fn send_text(&self, text: &str);
    /// A middle press the shell overlay owns; pastes the primary selection
    /// into the field under `p`.
    fn primary_paste(&self, p: LogicalPoint);
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

/// What the focused shell field can do right now.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct FieldEdit {
    pub undo: bool,
    pub redo: bool,
    pub cut: bool,
    pub copy: bool,
    pub select_all: bool,
}

static FIELD_EDIT: Mutex<Option<FieldEdit>> = Mutex::new(None);

/// Published by the shell overlay after every pass; `None` when no shell field
/// is focused, which is also the state in which jellyfin-web owns input.
pub fn publish_field_edit(state: Option<FieldEdit>) {
    *FIELD_EDIT.lock() = state;
}

pub fn field_edit() -> Option<FieldEdit> {
    *FIELD_EDIT.lock()
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

/// The two reports the web overlay's keyboard focus is computed from, and the
/// belief it was last handed.
struct WebFocus {
    /// The window's keyboard focus as the last backend report named it.
    window: bool,
    /// Whether a modal owns input, as the shell overlay last published it.
    modal_open: bool,
    /// What a live web sink took; `None` until the browser now behind the sink
    /// has taken anything.
    published: Option<bool>,
}

/// Seeded focused with no modal: the window is activated when it is mapped,
/// every backend reports each later change, and the shell overlay publishes a
/// state before it opens a modal.
static WEB_FOCUS: Mutex<WebFocus> = Mutex::new(WebFocus {
    window: true,
    modal_open: false,
    published: None,
});

/// Applies `report` and hands the web overlay the focus the two reports imply,
/// when that is not what a live sink already took.
///
/// The lock is held across the hand-off, so a window-focus report and a modal
/// flip cannot interleave and the report that lands last is the belief the web
/// overlay is left holding. It is the outermost lock on every path that reaches
/// here — the CEF client invokes its created callback holding none of its own —
/// and the web sink's browser lock is the only one taken under it.
fn report_web_focus(report: impl FnOnce(&mut WebFocus)) {
    let mut focus = WEB_FOCUS.lock();
    report(&mut focus);
    let belief = crate::route::web_focus(focus.window, focus.modal_open);
    if focus.published == Some(belief) {
        return;
    }
    let Some(web) = web_sink() else {
        return;
    };
    web.set_focus(belief);
    focus.published = Some(belief);
}

/// The browser behind the web sink became live. A browser holds no focus of
/// the router's until it is told one, so the belief the two reports imply is
/// handed to it here — including one published while no live sink existed,
/// which reached nothing.
pub fn web_became_live() {
    report_web_focus(|focus| focus.published = None);
}

/// The window's keyboard focus, as a backend reports it.
pub(crate) fn set_window_focused(focused: bool) {
    report_web_focus(|focus| focus.window = focused);
}

static STATE_LISTENERS: Mutex<Vec<Arc<StateListener>>> = Mutex::new(Vec::new());

/// Publish the shell overlay's routing state. The shell overlay is the only
/// publisher; a change of `modal_open` moves keyboard focus off or back onto
/// the web overlay.
pub fn publish_shell_state(state: crate::route::ShellState) {
    *STATE.lock() = Some(state);
    report_web_focus(|focus| focus.modal_open = state.modal_open);
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

/// The installed web sink while the browser behind it is live; `None` before
/// one is installed and whenever its browser does not exist.
fn web_sink() -> Option<&'static dyn WebInput> {
    let web = WEB.get()?;
    web.is_alive().then_some(&**web)
}

pub(crate) fn with_web<F: FnOnce(&dyn WebInput)>(f: F) {
    if let Some(w) = web_sink() {
        f(w);
    }
}

pub(crate) fn with_shell<F: FnOnce(&dyn ShellInput)>(f: F) {
    if let Some(s) = SHELL.get() {
        f(&**s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::ShellState;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Whether the double's browser is live; the test owns it.
    static ALIVE: AtomicBool = AtomicBool::new(false);
    /// Every focus a live double took, oldest first.
    static TAKEN: Mutex<Vec<bool>> = Mutex::new(Vec::new());

    /// A web sink whose liveness the test drives. Only `set_focus` and
    /// `is_alive` are reached: the test dispatches no input event.
    struct Double;

    impl WebInput for Double {
        fn send_key_event(
            &self,
            _type_: c_int,
            _modifiers: u32,
            _windows_key_code: c_int,
            _native_key_code: c_int,
            _is_system_key: bool,
            _character: u16,
            _unmodified_character: u16,
        ) {
        }

        fn send_mouse_click(
            &self,
            _x: c_int,
            _y: c_int,
            _modifiers: u32,
            _button: c_int,
            _mouse_up: bool,
            _click_count: c_int,
        ) {
        }

        fn send_mouse_move(&self, _x: c_int, _y: c_int, _modifiers: u32, _leave: bool) {}

        fn send_mouse_wheel(
            &self,
            _x: c_int,
            _y: c_int,
            _modifiers: u32,
            _delta_x: c_int,
            _delta_y: c_int,
        ) {
        }

        fn set_focus(&self, focus: bool) {
            TAKEN.lock().push(focus);
        }

        fn navigate_history(&self, _forward: bool) {}

        fn undo(&self) {}

        fn redo(&self) {}

        fn cut(&self) {}

        fn copy(&self) {}

        fn paste(&self) {}

        fn select_all(&self) {}

        fn is_alive(&self) -> bool {
            ALIVE.load(Ordering::Acquire)
        }
    }

    fn modal(open: bool) -> ShellState {
        ShellState {
            modal_open: open,
            ..ShellState::default()
        }
    }

    /// The web slot, the focus state and the record of what a live double took
    /// are one resource; a test owns all three for its whole body.
    static OWNED: Mutex<()> = Mutex::new(());

    /// Takes the process's web focus for the caller and hands back the state a
    /// fresh process holds.
    fn own_web_focus() -> parking_lot::MutexGuard<'static, ()> {
        let owned = OWNED.lock();
        install_web(Box::new(Double));
        ALIVE.store(false, Ordering::Release);
        TAKEN.lock().clear();
        *WEB_FOCUS.lock() = WebFocus {
            window: true,
            modal_open: false,
            published: None,
        };
        owned
    }

    /// The pair together: a belief published while no live sink exists reaches
    /// the browser that becomes live. Removing either half on its own leaves
    /// this test passing, because the surviving half carries the sequence.
    #[test]
    fn a_focus_no_live_sink_took_is_handed_to_the_next_live_one() {
        let _owned = own_web_focus();

        publish_shell_state(modal(true));
        assert!(TAKEN.lock().is_empty());

        ALIVE.store(true, Ordering::Release);
        web_became_live();
        assert_eq!(*TAKEN.lock(), [false]);

        publish_shell_state(modal(true));
        assert_eq!(*TAKEN.lock(), [false]);

        publish_shell_state(modal(false));
        assert_eq!(*TAKEN.lock(), [false, true]);
    }

    /// The withholding alone: the double's liveness is flipped without the
    /// announcement a created browser makes, so the belief reaches it only
    /// because nothing recorded that belief as taken while no sink was live.
    /// Fails when `published` is written before the liveness check; passes
    /// when `web_became_live` stops clearing it.
    #[test]
    fn a_belief_no_live_sink_took_is_handed_on_at_the_next_report() {
        let _owned = own_web_focus();

        publish_shell_state(modal(true));
        assert!(TAKEN.lock().is_empty());

        ALIVE.store(true, Ordering::Release);
        publish_shell_state(modal(true));
        assert_eq!(*TAKEN.lock(), [false]);
    }

    /// The clear alone: a browser that becomes live is handed the current focus
    /// even when its predecessor took that same belief. Fails when
    /// `web_became_live` stops clearing `published`; passes when `published` is
    /// written before the liveness check.
    #[test]
    fn a_recreated_browser_is_handed_the_focus_its_predecessor_took() {
        let _owned = own_web_focus();

        ALIVE.store(true, Ordering::Release);
        web_became_live();
        assert_eq!(*TAKEN.lock(), [true]);

        publish_shell_state(modal(false));
        assert_eq!(*TAKEN.lock(), [true]);

        web_became_live();
        assert_eq!(*TAKEN.lock(), [true, true]);
    }
}
