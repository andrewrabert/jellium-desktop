//! Wayland input layer.
//!
//! Wraps a foreign-owned wl_display (created by C++ platform_wayland), opens
//! its own EventQueue, binds wl_seat on its own registry view, and runs a
//! dedicated input thread that polls the display fd. Input events come back
//! to C++ as primitives via JfnInputCallbacks so no CEF-typed structs cross
//! the FFI boundary.

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::time::TimeSpec;
use nix::sys::timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags};
use parking_lot::Mutex;
use std::ffi::{c_int, c_void};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keymap, Keysym, Modifiers, RawModifiers, RepeatInfo,
};
use smithay_client_toolkit::seat::pointer::{
    CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
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
use jfn_platform_abi::cursor::CursorShape;

const XK_MENU: u32 = 0xff67;
const XK_F10: u32 = 0xffc7;

fn is_context_menu_key(sym: u32, mods: u32) -> bool {
    sym == XK_MENU || (sym == XK_F10 && mods & EVENTFLAG_SHIFT_DOWN != 0)
}

fn cef_to_cursor_icon(shape: CursorShape) -> CursorIcon {
    use CursorShape::*;
    match shape {
        Cross => CursorIcon::Crosshair,
        Hand => CursorIcon::Pointer,
        IBeam => CursorIcon::Text,
        Wait => CursorIcon::Wait,
        Help => CursorIcon::Help,
        EastResize => CursorIcon::EResize,
        NorthResize => CursorIcon::NResize,
        NorthEastResize => CursorIcon::NeResize,
        NorthWestResize => CursorIcon::NwResize,
        SouthResize => CursorIcon::SResize,
        SouthEastResize => CursorIcon::SeResize,
        SouthWestResize => CursorIcon::SwResize,
        WestResize => CursorIcon::WResize,
        NorthSouthResize => CursorIcon::NsResize,
        EastWestResize => CursorIcon::EwResize,
        NorthEastSouthWestResize => CursorIcon::NeswResize,
        NorthWestSouthEastResize => CursorIcon::NwseResize,
        ColumnResize => CursorIcon::ColResize,
        RowResize => CursorIcon::RowResize,
        Move => CursorIcon::Move,
        VerticalText => CursorIcon::VerticalText,
        Cell => CursorIcon::Cell,
        ContextMenu => CursorIcon::ContextMenu,
        Alias => CursorIcon::Alias,
        Progress => CursorIcon::Progress,
        NoDrop => CursorIcon::NoDrop,
        Copy => CursorIcon::Copy,
        NotAllowed => CursorIcon::NotAllowed,
        ZoomIn => CursorIcon::ZoomIn,
        ZoomOut => CursorIcon::ZoomOut,
        Grab => CursorIcon::Grab,
        Grabbing => CursorIcon::Grabbing,
        MiddlePanning | MiddlePanningVertical | MiddlePanningHorizontal => CursorIcon::AllScroll,
        _ => CursorIcon::Default,
    }
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

    fn suppress_focus_loss(&self) {
        self.suppressed_focus_loss.store(true, Ordering::Release);
    }

    fn discard_suppressed_focus_loss(&self) {
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

struct State {
    rt: &'static WlRuntime,
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

    repeat_timer: TimerFd,
    repeat_rate: i32,
    repeat_delay: i32,
    repeat_key: Option<KeyEvent>,
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
            pointer.set_cursor(conn, cef_to_cursor_icon(cef))
        };
    }

    fn arm_repeat(&mut self, key: KeyEvent) {
        if self.repeat_rate <= 0 {
            self.disarm_repeat();
            return;
        }
        self.repeat_key = Some(key);
        // A zero start disarms the timer outright regardless of the
        // interval, so a reported delay/rate of 0 must not reach 0ms.
        let period_ms = (1000u32 / self.repeat_rate as u32).max(1);
        let expiration = Expiration::IntervalDelayed(
            ms_to_timespec(self.repeat_delay.max(1) as u32),
            ms_to_timespec(period_ms),
        );
        let _ = self
            .repeat_timer
            .set(expiration, TimerSetTimeFlags::empty());
    }

    fn disarm_repeat(&mut self) {
        self.repeat_key = None;
        let _ = self.repeat_timer.unset();
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
        if pressed
            && let Some(f) = self.cb.char_
            && let Some(text) = &event.utf8
        {
            for ch in text.chars() {
                f(ch as u32, self.modifiers, event.raw_code);
            }
        }
    }

    fn fire_key_repeat(&mut self) {
        let Some(event) = self.repeat_key.clone() else {
            return;
        };
        // Don't leak a stale repeat into the main surface while a popup
        // has the keyboard.
        if crate::popup::active(self.rt) {
            self.disarm_repeat();
            return;
        }
        self.send_key(&event, true);
    }
}

fn ms_to_timespec(ms: u32) -> TimeSpec {
    TimeSpec::from_duration(Duration::from_millis(u64::from(ms)))
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
                    crate::popup::handle_motion(self.rt, surface_x as i32, surface_y as i32);
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
                if crate::popup::active(self.rt) {
                    if self.menu_focus {
                        crate::popup::handle_motion(self.rt, surface_x as i32, surface_y as i32);
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
                if crate::popup::active(self.rt) {
                    if pressed {
                        if let Some(flag) = flag {
                            self.popup_swallowed_buttons |= flag;
                        }
                        if self.menu_focus {
                            crate::popup::handle_button(
                                self.rt,
                                self.ptr_x as i32,
                                self.ptr_y as i32,
                                pressed,
                            );
                        } else {
                            // Click on our own window outside the menu: the popup grab
                            // won't dismiss same-client clicks, so do it ourselves.
                            crate::popup::handle_outside_press(self.rt);
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
                    crate::popup::arm(self.rt, self.ptr_x as i32, self.ptr_y as i32);
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
                if (button == BTN_RIGHT || button == BTN_LEFT)
                    && !pressed
                    && crate::popup::dismiss_if_speculative(self.rt)
                {
                    // The window still holds compositor focus here — teardown
                    // returns the keyboard to the main surface, so a leave
                    // swallowed at arm time was our own grab, not a real loss.
                    self.rt.seat().discard_suppressed_focus_loss();
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
        if crate::popup::active(self.rt) {
            // Wheel must not reach CEF while a <select> popup is open —
            // a wheel event outside Blink's popup rect cancels its
            // widget out from under the native menu.
            if self.menu_focus {
                crate::popup::handle_scroll(self.rt, dy);
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
        if crate::popup::is_engaged(self.rt) {
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
        if crate::popup::active(self.rt) {
            crate::popup::handle_key(self.rt, event.keysym.raw(), true);
            return;
        }
        if is_context_menu_key(event.keysym.raw(), self.modifiers) {
            // popup::active() only flips true once the async
            // configure lands, so disarm now rather than rely on it.
            self.disarm_repeat();
            crate::popup::arm(self.rt, self.ptr_x as i32, self.ptr_y as i32);
        }
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
        if crate::popup::active(self.rt) {
            // Otherwise a repeat released here stays armed and
            // outlives the popup.
            if armed == Some(event.raw_code) {
                self.disarm_repeat();
            }
            crate::popup::handle_key(self.rt, event.keysym.raw(), false);
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
        if crate::popup::active(self.rt) {
            crate::popup::handle_key(self.rt, event.keysym.raw(), true);
            return;
        }
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
    set_cursor_inbox: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    wake: Arc<jfn_wake_event::WakeEvent>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

fn worker_loop(
    conn: Connection,
    mut queue: wayland_client::EventQueue<State>,
    mut state: State,
    wake: Arc<jfn_wake_event::WakeEvent>,
    stop: Arc<AtomicBool>,
    cursor_type: Arc<AtomicU32>,
    set_cursor_inbox: Arc<AtomicBool>,
) {
    let display_fd = conn.as_fd().as_raw_fd();
    let wake_fd = wake.fd();
    let repeat_fd = state.repeat_timer.as_fd().as_raw_fd();
    loop {
        // Apply any pending cursor change before we block.
        if set_cursor_inbox.swap(false, Ordering::Acquire) {
            // cursor_type already reflects the desired value (writer updates
            // it before signalling); this just ensures we re-issue the
            // Wayland request on the current pointer/serial.
            state.apply_cursor(&conn);
            let _ = conn.flush();
        }

        let _ = queue.dispatch_pending(&mut state);
        let _ = conn.flush();

        let read_guard = match queue.prepare_read() {
            Some(g) => g,
            None => continue,
        };

        let mut pfds = [
            PollFd::new(
                unsafe { BorrowedFd::borrow_raw(display_fd) },
                PollFlags::POLLIN,
            ),
            PollFd::new(
                unsafe { BorrowedFd::borrow_raw(wake_fd) },
                PollFlags::POLLIN,
            ),
            PollFd::new(
                unsafe { BorrowedFd::borrow_raw(repeat_fd) },
                PollFlags::POLLIN,
            ),
        ];
        match poll(&mut pfds, PollTimeout::NONE) {
            Err(Errno::EINTR) => {
                drop(read_guard);
                continue;
            }
            Err(_) => {
                drop(read_guard);
                break;
            }
            Ok(_) => {}
        }
        let revents = |i: usize| pfds[i].revents().unwrap_or(PollFlags::empty());

        if revents(0).contains(PollFlags::POLLIN) {
            if read_guard.read().is_err() {
                break;
            }
        } else {
            drop(read_guard);
        }
        if revents(0).intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
            break;
        }
        if revents(1).contains(PollFlags::POLLIN) {
            wake.drain();
            // Wake reasons: cursor change request, or cleanup.
            if stop.load(Ordering::Relaxed) {
                let _ = queue.dispatch_pending(&mut state);
                break;
            }
            // Cursor change is handled at the top of the next iteration.
        }
        // Dispatch before the repeat fd: an unread release event would
        // otherwise leave state.repeat_key stale for this check.
        let _ = queue.dispatch_pending(&mut state);

        if revents(2).contains(PollFlags::POLLIN) {
            // Drain the expiration count so a level-triggered re-fire
            // doesn't spin the loop, then resend the held key.
            let mut buf = [0u8; 8];
            let _ = nix::unistd::read(unsafe { BorrowedFd::borrow_raw(repeat_fd) }, &mut buf);
            state.fire_key_repeat();
        }
    }

    let _ = cursor_type;
}

fn init_impl(rt: &'static WlRuntime, display: *mut c_void, cb: Callbacks) -> Option<InputThread> {
    if display.is_null() {
        return None;
    }
    let wake = Arc::new(jfn_wake_event::WakeEvent::new()?);
    let repeat_timer = TimerFd::new(
        ClockId::CLOCK_MONOTONIC,
        TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC,
    )
    .ok()?;
    let backend = unsafe { Backend::from_foreign_display(display as *mut _) };
    let conn = Connection::from_backend(backend);
    let (globals, queue) = registry_queue_init::<State>(&conn).ok()?;
    let qh = queue.handle();

    let seat_state = SeatState::new(&globals, &qh);
    seat_state.seats().next()?;
    let output_state = OutputState::new(&globals, &qh);
    let compositor = CompositorState::bind(&globals, &qh)
        .inspect_err(|e| tracing::error!(target: "Main", "input: wl_compositor: {e}"))
        .ok()?;
    let shm = Shm::bind(&globals, &qh)
        .inspect_err(|e| tracing::error!(target: "Main", "input: wl_shm: {e}"))
        .ok()?;

    let cursor_type = Arc::new(AtomicU32::new(CursorShape::Pointer.as_raw() as u32));
    let set_cursor_inbox = Arc::new(AtomicBool::new(false));
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
        repeat_timer,
        repeat_rate: 0,
        repeat_delay: 0,
        repeat_key: None,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let cursor_type_thread = cursor_type.clone();
    let inbox_thread = set_cursor_inbox.clone();
    let stop_thread = stop.clone();
    let wake_thread = wake.clone();
    let worker = thread::spawn(move || {
        worker_loop(
            conn,
            queue,
            state,
            wake_thread,
            stop_thread,
            cursor_type_thread,
            inbox_thread,
        )
    });
    Some(InputThread {
        cursor_type,
        set_cursor_inbox,
        stop,
        wake,
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
        self.cursor_type.store(cef_cursor_type, Ordering::Relaxed);
        self.set_cursor_inbox.store(true, Ordering::Release);
        // Wake the input thread so it picks up the cursor change.
        self.wake.signal();
    }

    /// Stop the worker and join it. Idempotent: a second call finds the join
    /// handle already taken.
    pub(crate) fn shutdown(&self, rt: &'static WlRuntime) {
        *rt.seat().kb_focus_cb.lock() = None;
        self.stop.store(true, Ordering::Relaxed);
        self.wake.signal();
        if let Some(w) = self.worker.lock().take() {
            let _ = w.join();
        }
        // The WakeEvent closes its fd when the last Arc (worker's + this one) drops.
    }
}
