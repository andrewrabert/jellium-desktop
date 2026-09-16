use jfn_platform_abi::LogicalPoint;
use parking_lot::Mutex;
use std::os::raw::c_int;
use std::sync::{Arc, OnceLock};

pub trait WebInput: Send + Sync {
    #[allow(clippy::too_many_arguments)]
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
    fn window_gesture(&self, hit: crate::route::ShellHit);
    fn context_menu(&self, p: LogicalPoint);
    fn send_key(&self, key: crate::key::ShellKey);
    fn send_text(&self, text: &str);
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

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct FieldEdit {
    pub undo: bool,
    pub redo: bool,
    pub cut: bool,
    pub copy: bool,
    pub select_all: bool,
}

static FIELD_EDIT: Mutex<Option<FieldEdit>> = Mutex::new(None);

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

struct WebFocus {
    window: bool,
    modal_open: bool,
    published: Option<bool>,
}

static WEB_FOCUS: Mutex<WebFocus> = Mutex::new(WebFocus {
    window: true,
    modal_open: false,
    published: None,
});

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

pub fn web_became_live() {
    report_web_focus(|focus| focus.published = None);
}

pub(crate) fn set_window_focused(focused: bool) {
    report_web_focus(|focus| focus.window = focused);
}

static STATE_LISTENERS: Mutex<Vec<Arc<StateListener>>> = Mutex::new(Vec::new());

pub fn publish_shell_state(state: crate::route::ShellState) {
    *STATE.lock() = Some(state);
    report_web_focus(|focus| focus.modal_open = state.modal_open);
    let listeners: Vec<Arc<StateListener>> = STATE_LISTENERS.lock().clone();
    for f in listeners {
        f(state);
    }
}

pub fn shell_state() -> Option<crate::route::ShellState> {
    *STATE.lock()
}

pub struct ShellStateSubscription(Arc<StateListener>);
impl Drop for ShellStateSubscription {
    fn drop(&mut self) {
        STATE_LISTENERS.lock().retain(|f| !Arc::ptr_eq(f, &self.0));
    }
}
pub fn on_shell_state_scoped(f: StateListener) -> ShellStateSubscription {
    register_shell_state(f)
}
fn register_shell_state(f: StateListener) -> ShellStateSubscription {
    let f = Arc::new(f);
    let seed = {
        let mut listeners = STATE_LISTENERS.lock();
        listeners.push(Arc::clone(&f));
        *STATE.lock()
    };
    let subscription = ShellStateSubscription(f);
    if let Some(state) = seed {
        (subscription.0)(state);
    }
    subscription
}

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

    #[test]
    fn dropping_subscription_releases_its_capture() {
        let _serial = OWNED.lock();
        let capture = Arc::new(());
        let observer = Arc::downgrade(&capture);
        let subscription = on_shell_state_scoped(Box::new(move |_| {
            let _ = &capture;
        }));
        assert!(observer.upgrade().is_some());
        drop(subscription);
        assert!(observer.upgrade().is_none());
    }

    #[test]
    fn a_panicking_seed_unregisters_and_releases_its_capture() {
        let _serial = OWNED.lock();
        let previous = STATE.lock().replace(ShellState::default());
        let capture = Arc::new(());
        let observer = Arc::downgrade(&capture);
        let before = STATE_LISTENERS.lock().len();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _subscription = on_shell_state_scoped(Box::new(move |_| {
                let _ = &capture;
                panic!("injected seed failure");
            }));
        }));
        *STATE.lock() = previous;
        assert!(result.is_err());
        assert_eq!(STATE_LISTENERS.lock().len(), before);
        assert!(observer.upgrade().is_none());
    }

    static ALIVE: AtomicBool = AtomicBool::new(false);
    static TAKEN: Mutex<Vec<bool>> = Mutex::new(Vec::new());

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

    static OWNED: Mutex<()> = Mutex::new(());

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

    #[test]
    fn a_belief_no_live_sink_took_is_handed_on_at_the_next_report() {
        let _owned = own_web_focus();

        publish_shell_state(modal(true));
        assert!(TAKEN.lock().is_empty());

        ALIVE.store(true, Ordering::Release);
        publish_shell_state(modal(true));
        assert_eq!(*TAKEN.lock(), [false]);
    }

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
