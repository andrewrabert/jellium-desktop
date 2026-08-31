//! Wayland input layer.
//!
//! Wraps a foreign-owned wl_display (created by C++ platform_wayland), opens
//! its own EventQueue, binds wl_seat on its own registry view, and runs a
//! dedicated input thread that polls the display fd. Input events come back
//! to C++ as primitives via JfnInputCallbacks so no CEF-typed structs cross
//! the FFI boundary.

use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, LoopSignal, ping::PingSource};
use calloop_wayland_source::WaylandSource;
use parking_lot::Mutex;
use std::ffi::{c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use jfn_linux_util::menu::MenuPoint;
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keymap, Keysym, Modifiers, RawModifiers, RepeatInfo,
};
use smithay_client_toolkit::seat::pointer::{
    PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use wayland_backend::client::Backend;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use xkbcommon::xkb;

use jfn_input::buttons::{
    BTN_BACK, BTN_EXTRA, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE,
};
use jfn_platform_abi::event_flags::{
    EVENTFLAG_ALT_DOWN, EVENTFLAG_CONTROL_DOWN, EVENTFLAG_LEFT_MOUSE_BUTTON,
    EVENTFLAG_MIDDLE_MOUSE_BUTTON, EVENTFLAG_RIGHT_MOUSE_BUTTON, EVENTFLAG_SHIFT_DOWN,
};

use crate::runtime::WlRuntime;
use jfn_platform_abi::LogicalPoint;
use jfn_platform_abi::cursor::CursorShape;

const XK_MENU: u32 = 0xff67;
const XK_F10: u32 = 0xffc7;

fn is_context_menu_key(sym: u32, mods: u32) -> bool {
    sym == XK_MENU || (sym == XK_F10 && mods & EVENTFLAG_SHIFT_DOWN != 0)
}

/// Whether jellyfin-web owns the keyboard. Its page menu is the only menu a
/// key press raises through CEF's asynchronous callback, so it is the only one
/// that needs the grab armed while the press's serial is live; the shell
/// overlay raises its edit menu from the caret with the surface's own serial.
fn web_owns_keys() -> bool {
    jfn_input::shell_state().map(jfn_input::route::route_key) == Some(jfn_input::Target::Web)
}

/// Seat facts the input thread publishes for the root and CEF threads: the
/// serials a grab request must cite, and the focus-loss the menu grab swallowed.
pub struct SeatShared {
    // Interactive move/resize requires the serial of the pointer press whose
    // implicit grab drives the drag — a later key press serial would be rejected.
    last_button_serial: AtomicU32,
    // xdg_popup.grab accepts the serial of any press-type input event; tracking
    // key presses too keeps the serial fresh for keyboard-opened `<select>`s
    // (Enter/Space), which grab without any button press to cite.
    last_input_serial: AtomicU32,
    suppressed_focus_loss: AtomicBool,
    kb_focus_cb: Mutex<Option<KbFocusFn>>,
}

impl SeatShared {
    pub(crate) fn new() -> Self {
        Self {
            last_button_serial: AtomicU32::new(0),
            last_input_serial: AtomicU32::new(0),
            suppressed_focus_loss: AtomicBool::new(false),
            kb_focus_cb: Mutex::new(None),
        }
    }

    pub(crate) fn last_button_serial(&self) -> u32 {
        self.last_button_serial.load(Ordering::Acquire)
    }

    pub(crate) fn last_input_serial(&self) -> u32 {
        self.last_input_serial.load(Ordering::Acquire)
    }

    pub(crate) fn suppress_focus_loss(&self) {
        self.suppressed_focus_loss.store(true, Ordering::Release);
    }

    pub(crate) fn discard_suppressed_focus_loss(&self) {
        self.suppressed_focus_loss.store(false, Ordering::Release);
    }

    pub(crate) fn flush_suppressed_focus_loss(&self) {
        if self.suppressed_focus_loss.swap(false, Ordering::AcqRel)
            && let Some(f) = *self.kb_focus_cb.lock()
        {
            f(0);
        }
    }
}

pub type MouseMoveFn = fn(x: i32, y: i32, mods: u32, leave: c_int);
pub type MouseButtonFn = fn(button: u32, pressed: c_int, x: i32, y: i32, mods: u32);
pub type ScrollFn = fn(x: i32, y: i32, dx: i32, dy: i32, mods: u32);
pub type HistoryNavFn = fn(forward: c_int);
pub type KbFocusFn = fn(gained: c_int);
pub type KeyFn = fn(keysym: u32, native_code: u32, mods: u32, pressed: c_int);
pub type CharFn = fn(codepoint: u32, mods: u32, native_code: u32);

#[derive(Clone, Copy)]
pub struct Callbacks {
    pub mouse_move: Option<MouseMoveFn>,
    pub mouse_button: Option<MouseButtonFn>,
    pub scroll: Option<ScrollFn>,
    pub history_nav: Option<HistoryNavFn>,
    pub kb_focus: Option<KbFocusFn>,
    pub key: Option<KeyFn>,
    pub char_: Option<CharFn>,
}

unsafe impl Send for Callbacks {}
unsafe impl Sync for Callbacks {}

// Safety: State is only ever accessed from the input thread after the
// worker is spawned. xkbcommon's raw pointers are not Send by default; this
// crate restricts them to the worker thread by construction.
unsafe impl Send for State {}

pub(crate) struct State {
    pub(crate) rt: &'static WlRuntime,
    cb: Callbacks,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    shm: Shm,
    pointer: Option<ThemedPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,

    // Pointer state.
    ptr_x: f64,
    ptr_y: f64,
    // Last pointer position on the MAIN surface. ptr_x/ptr_y rebase to
    // menu-local coords while the pointer is over the popup; events forwarded
    // to CEF during that window must use these instead.
    main_ptr_x: f64,
    main_ptr_y: f64,
    pointer_serial: u32,
    mouse_button_modifiers: u32,
    // Releases for button presses consumed by our native popup must also be
    // consumed, even if the popup closes on the press and is inactive by the
    // time Wayland delivers the matching release.
    popup_swallowed_buttons: u32,

    // Scroll accumulation across a single pointer frame.
    scroll_dx: f64,
    scroll_dy: f64,
    scroll_v120_x: i32,
    scroll_v120_y: i32,
    scroll_have_v120: bool,

    xkb_ctx: xkb::Context,
    xkb_kmap: Option<xkb::Keymap>,
    modifiers: u32,

    // Latest desired cursor (re-applied on pointer enter).
    cursor_type: Arc<AtomicU32>,

    menu_focus: bool,

    stop: Arc<AtomicBool>,
    signal: Option<LoopSignal>,
    pub(crate) loop_handle: Option<LoopHandle<'static, State>>,
    /// Bumped by every arm/disarm; a timer whose generation is stale drops
    /// itself instead of firing, so no source is ever removed mid-dispatch.
    repeat_generation: u64,
    repeat_rate: i32,
    repeat_delay: i32,
    repeat_key: Option<KeyEvent>,
    /// The last key press, so a `Repeated` event carrying no UTF-8 can stand
    /// for the text the press carried.
    pressed_key: Option<KeyEvent>,
    pub(crate) selection: crate::selection::SelectionState,
}

impl State {
    fn cef_modifiers(&self) -> u32 {
        self.modifiers | self.mouse_button_modifiers
    }

    fn mouse_button_flag(button: u32) -> Option<u32> {
        match button {
            BTN_LEFT => Some(EVENTFLAG_LEFT_MOUSE_BUTTON),
            BTN_RIGHT => Some(EVENTFLAG_RIGHT_MOUSE_BUTTON),
            BTN_MIDDLE => Some(EVENTFLAG_MIDDLE_MOUSE_BUTTON),
            _ => None,
        }
    }

    fn key_repeats(&self, raw_code: u32) -> bool {
        self.xkb_kmap
            .as_ref()
            .is_some_and(|km| km.key_repeats((raw_code + 8).into()))
    }

    fn apply_cursor(&mut self, conn: &Connection) {
        let cef = CursorShape::from_cef(self.cursor_type.load(Ordering::Relaxed) as i32)
            .unwrap_or(CursorShape::Pointer);
        let Some(pointer) = &self.pointer else { return };
        // set_cursor/hide_cursor reuse the pointer's last enter serial, so they
        // are a protocol error until the pointer has entered one of our surfaces.
        if self.pointer_serial == 0 {
            return;
        }
        let _ = if cef == CursorShape::None {
            pointer.hide_cursor()
        } else {
            pointer.set_cursor(conn, jfn_linux_util::cursor::icon_for(cef))
        };
    }

    fn arm_repeat(&mut self, key: KeyEvent) {
        if self.repeat_rate <= 0 {
            self.disarm_repeat();
            return;
        }
        self.disarm_repeat();
        self.repeat_key = Some(key);
        let generation = self.repeat_generation;
        // A zero delay would fire the first repeat in the same breath as the
        // press, so a reported delay/rate of 0 must not reach 0ms.
        let period = Duration::from_millis(u64::from((1000u32 / self.repeat_rate as u32).max(1)));
        let delay = Duration::from_millis(self.repeat_delay.max(1) as u64);
        let Some(handle) = self.loop_handle.clone() else {
            return;
        };
        let inserted = handle.insert_source(
            Timer::from_duration(delay),
            move |_, (), state: &mut State| {
                if state.repeat_generation != generation {
                    return TimeoutAction::Drop;
                }
                state.fire_key_repeat();
                if state.repeat_generation != generation {
                    return TimeoutAction::Drop;
                }
                TimeoutAction::ToDuration(period)
            },
        );
        if let Err(e) = inserted {
            tracing::error!(target: "Main", "input: repeat timer: {e}");
            self.repeat_key = None;
        }
    }

    fn disarm_repeat(&mut self) {
        self.repeat_key = None;
        self.repeat_generation = self.repeat_generation.wrapping_add(1);
    }

    fn send_key(&self, event: &KeyEvent, pressed: bool) {
        if let Some(f) = self.cb.key {
            f(
                event.keysym.raw(),
                event.raw_code,
                self.modifiers,
                if pressed { 1 } else { 0 },
            );
        }
        if !pressed {
            return;
        }
        if let Some(composed) = jfn_linux_util::input::compose_feed(event.keysym.raw()) {
            jfn_input::jfn_input_dispatch_text(&composed, self.modifiers);
            return;
        }
        if jfn_linux_util::input::compose_pending() {
            return;
        }
        if let Some(f) = self.cb.char_
            && let Some(text) = &event.utf8
        {
            for ch in text.chars() {
                f(ch as u32, self.modifiers, event.raw_code);
            }
        }
    }

    /// The [`KeyEvent`] a `Repeated` key event stands for: a version 10
    /// compositor reports the repeat with no UTF-8, so the pressed key's text
    /// is substituted when the raw codes match.
    fn repeated(&self, event: KeyEvent) -> KeyEvent {
        if event.utf8.is_some() {
            return event;
        }
        let Some(pressed) = self.pressed_key.as_ref() else {
            return event;
        };
        if pressed.raw_code != event.raw_code {
            return event;
        }
        KeyEvent {
            utf8: pressed.utf8.clone(),
            ..event
        }
    }

    fn fire_key_repeat(&mut self) {
        let Some(event) = self.repeat_key.clone() else {
            return;
        };
        // Don't leak a stale repeat into the main surface while a popup
        // has the keyboard.
        if self.rt.menu().is_active() {
            self.disarm_repeat();
            return;
        }
        self.send_key(&event, true);
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![SeatState, OutputState];
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer if self.pointer.is_none() => {
                let cursor_surface = self.compositor.create_surface(qh);
                self.pointer = self
                    .seat_state
                    .get_pointer_with_theme::<_, ()>(
                        qh,
                        &seat,
                        self.shm.wl_shm(),
                        cursor_surface,
                        ThemeSpec::default(),
                    )
                    .inspect_err(|e| tracing::error!(target: "Main", "input: themed pointer: {e}"))
                    .ok();
            }
            Capability::Keyboard if self.keyboard.is_none() => {
                self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer => {
                if let Some(themed) = self.pointer.take()
                    && themed.pointer().version() >= 3
                {
                    themed.pointer().release();
                }
                self.pointer_serial = 0;
            }
            Capability::Keyboard => {
                self.disarm_repeat();
                if let Some(keyboard) = self.keyboard.take()
                    && keyboard.version() >= 3
                {
                    keyboard.release();
                }
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
        self.apply_cursor(conn);
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            self.pointer_event(conn, event);
        }
        self.flush_scroll();
    }
}

impl State {
    fn pointer_event(&mut self, conn: &Connection, event: &PointerEvent) {
        let (surface_x, surface_y) = event.position;
        match event.kind {
            PointerEventKind::Enter { serial } => {
                self.pointer_serial = serial;
                self.menu_focus =
                    crate::popup::surface_matches(self.rt, event.surface.id().protocol_id());
                self.ptr_x = surface_x;
                self.ptr_y = surface_y;
                if self.menu_focus {
                    let position = LogicalPoint::from_view(surface_x, surface_y);
                    self.rt.menu().motion(MenuPoint::Logical {
                        x: position.x,
                        y: position.y,
                    });
                    return;
                }
                self.main_ptr_x = surface_x;
                self.main_ptr_y = surface_y;
                self.apply_cursor(conn);
                if let Some(f) = self.cb.mouse_move {
                    f(
                        self.ptr_x as i32,
                        self.ptr_y as i32,
                        self.cef_modifiers(),
                        0,
                    );
                }
            }
            PointerEventKind::Leave { .. } => {
                if self.menu_focus {
                    self.menu_focus = false;
                    return;
                }
                if let Some(f) = self.cb.mouse_move {
                    f(
                        self.ptr_x as i32,
                        self.ptr_y as i32,
                        self.cef_modifiers(),
                        1,
                    );
                }
            }
            PointerEventKind::Motion { .. } => {
                self.ptr_x = surface_x;
                self.ptr_y = surface_y;
                if !self.menu_focus {
                    self.main_ptr_x = surface_x;
                    self.main_ptr_y = surface_y;
                }
                if self.rt.menu().is_active() {
                    if self.menu_focus {
                        let position = LogicalPoint::from_view(surface_x, surface_y);
                        self.rt.menu().motion(MenuPoint::Logical {
                            x: position.x,
                            y: position.y,
                        });
                    }
                    return;
                }
                if let Some(f) = self.cb.mouse_move {
                    f(
                        self.ptr_x as i32,
                        self.ptr_y as i32,
                        self.cef_modifiers(),
                        0,
                    );
                }
            }
            PointerEventKind::Press { button, serial, .. }
            | PointerEventKind::Release { button, serial, .. } => {
                let pressed = matches!(event.kind, PointerEventKind::Press { .. });
                if pressed {
                    self.rt
                        .seat()
                        .last_button_serial
                        .store(serial, Ordering::Release);
                    self.rt
                        .seat()
                        .last_input_serial
                        .store(serial, Ordering::Release);
                }
                let flag = Self::mouse_button_flag(button);
                if self.rt.menu().is_active() {
                    if pressed {
                        if let Some(flag) = flag {
                            self.popup_swallowed_buttons |= flag;
                        }
                        if self.menu_focus {
                            self.rt.menu().press(MenuPoint::Logical {
                                x: self.ptr_x as c_int,
                                y: self.ptr_y as c_int,
                            });
                        } else {
                            // Click on our own window outside the menu: the popup grab
                            // won't dismiss same-client clicks, so do it ourselves.
                            self.rt.menu().dismiss();
                        }
                    } else if let Some(flag) = flag {
                        if self.mouse_button_modifiers & flag != 0 {
                            // This is the release for the click that opened the
                            // popup. CEF saw that press before the native menu
                            // became active, so it must also see the matching
                            // release; otherwise Blink keeps the button latched
                            // and subsequent <select> activations are ignored.
                            self.mouse_button_modifiers &= !flag;
                            if let Some(f) = self.cb.mouse_button {
                                f(
                                    button,
                                    0,
                                    self.main_ptr_x as i32,
                                    self.main_ptr_y as i32,
                                    self.cef_modifiers(),
                                );
                            }
                        } else {
                            self.popup_swallowed_buttons &= !flag;
                        }
                    }
                    return;
                }
                if let Some(flag) = flag
                    && !pressed
                    && self.popup_swallowed_buttons & flag != 0
                {
                    self.popup_swallowed_buttons &= !flag;
                    return;
                }
                if button == BTN_SIDE
                    || button == BTN_EXTRA
                    || button == BTN_BACK
                    || button == BTN_FORWARD
                {
                    if pressed {
                        let forward = button == BTN_EXTRA || button == BTN_FORWARD;
                        if let Some(f) = self.cb.history_nav {
                            f(if forward { 1 } else { 0 });
                        }
                    }
                    return;
                }
                let Some(flag) = flag else { return };
                // Grab must be requested now, while this press's implicit grab is
                // live; the menu model only arrives later via CEF's async callback.
                // Right-click arms the context menu; left-click arms a possible
                // `<select>` dropdown (CEF tells us asynchronously if one opened).
                if (button == BTN_RIGHT || button == BTN_LEFT) && pressed {
                    self.disarm_repeat();
                    self.rt
                        .menu()
                        .arm(self.ptr_x as i32, self.ptr_y as i32, serial);
                }
                if pressed {
                    self.mouse_button_modifiers |= flag;
                } else {
                    self.mouse_button_modifiers &= !flag;
                }
                if let Some(f) = self.cb.mouse_button {
                    f(
                        button,
                        if pressed { 1 } else { 0 },
                        self.ptr_x as i32,
                        self.ptr_y as i32,
                        self.cef_modifiers(),
                    );
                }
                // Drop the grab armed on the press if this click opened no menu (#494).
                if (button == BTN_RIGHT || button == BTN_LEFT) && !pressed {
                    self.rt.menu().dismiss_if_speculative();
                }
            }
            PointerEventKind::Axis {
                horizontal,
                vertical,
                ..
            } => {
                if vertical.stop {
                    self.scroll_dy = 0.0;
                } else {
                    self.scroll_dy += vertical.absolute;
                }
                if horizontal.stop {
                    self.scroll_dx = 0.0;
                } else {
                    self.scroll_dx += horizontal.absolute;
                }
                if vertical.value120 != 0 || horizontal.value120 != 0 {
                    self.scroll_have_v120 = true;
                    self.scroll_v120_y += vertical.value120;
                    self.scroll_v120_x += horizontal.value120;
                }
            }
        }
    }

    fn flush_scroll(&mut self) {
        let (mut dx, mut dy) = (0i32, 0i32);
        if self.scroll_have_v120 {
            dx = -self.scroll_v120_x;
            dy = -self.scroll_v120_y;
            self.scroll_dx = 0.0;
            self.scroll_dy = 0.0;
        } else if self.scroll_dx != 0.0 || self.scroll_dy != 0.0 {
            let scaled_x = -self.scroll_dx * 12.0;
            let scaled_y = -self.scroll_dy * 12.0;
            dx = scaled_x as i32;
            dy = scaled_y as i32;
            // Carry the sub-step remainder into the next frame; zeroing it
            // rounds slow continuous scrolling away to nothing.
            self.scroll_dx = -(scaled_x - dx as f64) / 12.0;
            self.scroll_dy = -(scaled_y - dy as f64) / 12.0;
        } else {
            self.scroll_dx = 0.0;
            self.scroll_dy = 0.0;
        }
        self.scroll_v120_x = 0;
        self.scroll_v120_y = 0;
        self.scroll_have_v120 = false;
        if dx == 0 && dy == 0 {
            return;
        }
        if self.rt.menu().is_active() {
            // Wheel must not reach CEF while a <select> popup is open —
            // a wheel event outside Blink's popup rect cancels its
            // widget out from under the native menu.
            if self.menu_focus {
                self.rt.menu().scroll(dy);
            }
            return;
        }
        if let Some(f) = self.cb.scroll {
            f(
                self.ptr_x as i32,
                self.ptr_y as i32,
                dx,
                dy,
                self.cef_modifiers(),
            );
        }
    }
}

impl KeyboardHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        // Menu-surface enter/leave is grab plumbing, not CEF focus.
        if crate::popup::is_menu_surface(self.rt, surface.id().protocol_id()) {
            return;
        }
        self.rt.seat().discard_suppressed_focus_loss();
        if let Some(f) = self.cb.kb_focus {
            f(1);
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        // Neither leave may reach CEF as focus-loss — Blink would
        // close the <select> popup the replayed selection keys still
        // need: leave of the menu surface (popup teardown), and leave
        // of the main surface caused by our own grab activating.
        if crate::popup::is_menu_surface(self.rt, surface.id().protocol_id()) {
            return;
        }
        if self.rt.menu().is_engaged() {
            self.rt.seat().suppress_focus_loss();
            return;
        }
        // Stop repeating on real focus loss, or it keeps firing
        // once focus returns to a different surface.
        self.disarm_repeat();
        if let Some(f) = self.cb.kb_focus {
            f(0);
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.rt
            .seat()
            .last_input_serial
            .store(serial, Ordering::Release);
        if self.rt.menu().is_active() {
            self.rt.menu().key(event.keysym.raw());
            return;
        }
        if is_context_menu_key(event.keysym.raw(), self.modifiers) && web_owns_keys() {
            // popup::active() only flips true once the async
            // configure lands, so disarm now rather than rely on it.
            self.disarm_repeat();
            self.rt
                .menu()
                .arm(self.ptr_x as i32, self.ptr_y as i32, serial);
        }
        self.pressed_key = Some(event.clone());
        self.send_key(&event, true);
        // A version-10 compositor repeats keys itself and delivers them through
        // `repeat_key`; arming the timer as well would double every repeat.
        if keyboard.version() < 10 && self.key_repeats(event.raw_code) {
            self.arm_repeat(event);
        }
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let armed = self.repeat_key.as_ref().map(|e| e.raw_code);
        if self.rt.menu().is_active() {
            // Otherwise a repeat released here stays armed and
            // outlives the popup.
            if armed == Some(event.raw_code) {
                self.disarm_repeat();
            }
            return;
        }
        self.send_key(&event, false);
        if armed == Some(event.raw_code) {
            self.disarm_repeat();
        }
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.rt.menu().is_active() {
            self.rt.menu().key(event.keysym.raw());
            return;
        }
        let event = self.repeated(event);
        self.send_key(&event, true);
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        let mut m = 0u32;
        if modifiers.shift {
            m |= EVENTFLAG_SHIFT_DOWN;
        }
        if modifiers.ctrl {
            m |= EVENTFLAG_CONTROL_DOWN;
        }
        if modifiers.alt {
            m |= EVENTFLAG_ALT_DOWN;
        }
        self.modifiers = m;
    }

    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        info: RepeatInfo,
    ) {
        match info {
            RepeatInfo::Repeat { rate, delay } => {
                self.repeat_rate = rate.get() as i32;
                self.repeat_delay = delay as i32;
            }
            RepeatInfo::Disable => {
                self.repeat_rate = 0;
                self.disarm_repeat();
            }
        }
    }

    fn update_keymap(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        keymap: Keymap<'_>,
    ) {
        self.xkb_kmap = xkb::Keymap::new_from_string(
            &self.xkb_ctx,
            keymap.as_string(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        );
    }
}

delegate_dispatch2!(State);
delegate_registry!(State);

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub struct InputThread {
    cursor_type: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    ping: calloop::ping::Ping,
    worker: Mutex<Option<JoinHandle<()>>>,
}

// The display fd is shared with other readers; a blocking dispatch here would
// deadlock them, so the queue is driven through `WaylandSource`.
fn run_input_loop(
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    mut state: State,
    wake: PingSource,
) {
    let mut event_loop = match EventLoop::<State>::try_new() {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(target: "Main", "input: event loop: {e}");
            return;
        }
    };
    let handle = event_loop.handle();
    state.signal = Some(event_loop.get_signal());
    state.loop_handle = Some(handle.clone());

    if let Some(source) = state.rt.selections().take_source() {
        let qh = queue.handle();
        if let Err(e) = handle.insert_source(source, move |(), (), state: &mut State| {
            state.serve_selections(&qh);
        }) {
            tracing::error!(target: "Main", "input: selection source: {e}");
        }
    }

    let wake_conn = conn.clone();
    let stop = state.stop.clone();
    if let Err(e) = handle.insert_source(wake, move |(), (), state: &mut State| {
        state.apply_cursor(&wake_conn);
        let _ = wake_conn.flush();
        if stop.load(Ordering::Relaxed)
            && let Some(signal) = &state.signal
        {
            signal.stop();
        }
    }) {
        tracing::error!(target: "Main", "input: wake source: {e}");
        return;
    }
    if let Err(e) = handle.insert_source(
        WaylandSource::new(conn, queue),
        |_, queue, state: &mut State| queue.dispatch_pending(state),
    ) {
        tracing::error!(target: "Main", "input: wayland source: {e}");
        return;
    }
    if let Err(e) = event_loop.run(None, &mut state, |_| {}) {
        tracing::error!(target: "Main", "input: event loop: {e}");
    }
    state.drain_selection_reads();
}

fn init_impl(rt: &'static WlRuntime, display: *mut c_void, cb: Callbacks) -> Option<InputThread> {
    if display.is_null() {
        return None;
    }
    let (ping, wake) = calloop::ping::make_ping()
        .inspect_err(|e| tracing::error!(target: "Main", "input: ping: {e}"))
        .ok()?;
    let backend = unsafe { Backend::from_foreign_display(display as *mut _) };
    let conn = Connection::from_backend(backend);
    let (globals, queue) = registry_queue_init::<State>(&conn).ok()?;
    let qh = queue.handle();

    let seat_state = SeatState::new(&globals, &qh);
    let seat = seat_state.seats().next()?;
    let output_state = OutputState::new(&globals, &qh);
    let compositor = CompositorState::bind(&globals, &qh)
        .inspect_err(|e| tracing::error!(target: "Main", "input: wl_compositor: {e}"))
        .ok()?;
    let shm = Shm::bind(&globals, &qh)
        .inspect_err(|e| tracing::error!(target: "Main", "input: wl_shm: {e}"))
        .ok()?;

    let cursor_type = Arc::new(AtomicU32::new(CursorShape::Pointer.as_raw() as u32));
    let stop = Arc::new(AtomicBool::new(false));
    *rt.seat().kb_focus_cb.lock() = cb.kb_focus;

    let state = State {
        rt,
        cb,
        registry_state: RegistryState::new(&globals),
        seat_state,
        output_state,
        compositor,
        shm,
        pointer: None,
        keyboard: None,
        ptr_x: 0.0,
        ptr_y: 0.0,
        main_ptr_x: 0.0,
        main_ptr_y: 0.0,
        pointer_serial: 0,
        mouse_button_modifiers: 0,
        popup_swallowed_buttons: 0,
        scroll_dx: 0.0,
        scroll_dy: 0.0,
        scroll_v120_x: 0,
        scroll_v120_y: 0,
        scroll_have_v120: false,
        xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb_kmap: None,
        modifiers: 0,
        cursor_type: cursor_type.clone(),
        menu_focus: false,
        stop: stop.clone(),
        signal: None,
        loop_handle: None,
        repeat_generation: 0,
        repeat_rate: 0,
        repeat_delay: 0,
        repeat_key: None,
        pressed_key: None,
        selection: crate::selection::SelectionState::bind(rt.selections(), &globals, &qh, &seat),
    };

    let worker = thread::spawn(move || run_input_loop(conn, queue, state, wake));
    Some(InputThread {
        cursor_type,
        stop,
        ping,
        worker: Mutex::new(Some(worker)),
    })
}

pub fn init(
    rt: &'static WlRuntime,
    display: *mut c_void,
    callbacks: &Callbacks,
) -> Option<InputThread> {
    init_impl(rt, display, *callbacks)
}

impl InputThread {
    pub(crate) fn set_cursor(&self, cef_cursor_type: u32) {
        self.cursor_type.store(cef_cursor_type, Ordering::Release);
        self.ping.ping();
    }

    /// Stop the worker and join it. Idempotent: a second call finds the join
    /// handle already taken.
    pub(crate) fn shutdown(&self, rt: &'static WlRuntime) {
        *rt.seat().kb_focus_cb.lock() = None;
        self.stop.store(true, Ordering::Relaxed);
        self.ping.ping();
        if let Some(w) = self.worker.lock().take() {
            let _ = w.join();
        }
    }
}
