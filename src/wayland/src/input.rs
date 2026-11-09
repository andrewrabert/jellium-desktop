//! Ordered Wayland seat and popup dispatch on the application's display.
//!
//! Protocol destinations own input delivery. The content adapter translates
//! resolved events into the application's input callbacks; popup contents
//! register their own adapter on the same queue.

use crate::popup_protocol::{PopupCommand, Popups};
use crate::protocol::{InputTarget, SeatInput};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, LoopSignal, ping::PingSource};
use calloop_wayland_source::WaylandSource;
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use std::ffi::{c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

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

use jfn_input::buttons::{BTN_BACK, BTN_EXTRA, BTN_FORWARD, BTN_SIDE};
use jfn_platform_abi::event_flags::{
    EVENTFLAG_ALT_DOWN, EVENTFLAG_CONTROL_DOWN, EVENTFLAG_SHIFT_DOWN,
};

use crate::runtime::WlRuntime;
use jfn_platform_abi::cursor::CursorShape;

/// Input serials published for requests from application callbacks.
pub struct SeatShared {
    // Interactive move/resize requires the serial of the pointer press whose
    // implicit grab drives the drag — a later key press serial would be rejected.
    last_button_serial: AtomicU32,
    // xdg_popup.grab accepts the serial of any press-type input event; tracking
    // key presses too keeps the serial fresh for keyboard-opened `<select>`s
    // (Enter/Space), which grab without any button press to cite.
    last_input_serial: AtomicU32,
}

impl SeatShared {
    pub(crate) fn new() -> Self {
        Self {
            last_button_serial: AtomicU32::new(0),
            last_input_serial: AtomicU32::new(0),
        }
    }

    pub(crate) fn last_button_serial(&self) -> u32 {
        self.last_button_serial.load(Ordering::Acquire)
    }

    pub(crate) fn last_input_serial(&self) -> u32 {
        self.last_input_serial.load(Ordering::Acquire)
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
    pub(crate) compositor: CompositorState,
    shm: Shm,
    pointer: Option<ThemedPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,

    pub(crate) qh: QueueHandle<Self>,
    pub(crate) seat: wl_seat::WlSeat,
    pub(crate) popups: Popups,
    pub(crate) protocol: SeatInput,
    popup_commands: Receiver<PopupCommand>,
    pointer_serial: u32,

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

    fn send_key(&mut self, event: &KeyEvent, pressed: bool) {
        self.protocol.key(event, pressed, self.modifiers);
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
        if seat != self.seat {
            return;
        }
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
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if seat != self.seat {
            return;
        }
        match capability {
            Capability::Pointer => {
                self.protocol.cancel_pointer(self.modifiers);
                if let Some(themed) = self.pointer.take()
                    && themed.pointer().version() >= 3
                {
                    themed.pointer().release();
                }
                self.pointer_serial = 0;
            }
            Capability::Keyboard => {
                self.protocol.cancel_keyboard(self.modifiers);
                self.reconcile_keyboard_focus();
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

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if seat == self.seat {
            self.disarm_repeat();
            self.protocol.cancel_pointer(self.modifiers);
            self.protocol.cancel_keyboard(self.modifiers);
            self.reconcile_keyboard_focus();
        }
    }
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
            // Axis groups belong to the focus under which they arrived.
            if matches!(
                event.kind,
                PointerEventKind::Enter { .. } | PointerEventKind::Leave { .. }
            ) {
                self.flush_scroll();
                self.scroll_dx = 0.0;
                self.scroll_dy = 0.0;
            }
            self.pointer_event(conn, event);
            self.drain_popup_commands();
        }
        self.flush_scroll();
    }
}

impl State {
    fn pointer_event(&mut self, conn: &Connection, event: &PointerEvent) {
        let modifiers = self.modifiers;
        match event.kind {
            PointerEventKind::Enter { serial } => {
                self.pointer_serial = serial;
                self.protocol
                    .enter(&event.surface, event.position, modifiers);
                self.apply_cursor(conn);
            }
            PointerEventKind::Leave { .. } => {
                self.protocol.leave(&event.surface, modifiers);
            }
            PointerEventKind::Motion { .. } => {
                self.protocol
                    .motion(&event.surface, event.position, modifiers);
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
                let dismiss = if pressed {
                    self.protocol.outside_press(&event.surface)
                } else {
                    Vec::new()
                };
                let consumed = !dismiss.is_empty();
                for generation in dismiss {
                    self.dismiss_popup(generation);
                }
                self.protocol
                    .button(&event.surface, button, pressed, consumed, self.modifiers);
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
        self.protocol.scroll(dx, dy, self.modifiers);
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
        self.protocol.keyboard_enter(surface);
        self.reconcile_keyboard_focus();
    }

    fn leave(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.disarm_repeat();
        self.protocol.keyboard_leave(surface, self.modifiers);
        // Enter may be in a later event group. A sync on this queue observes
        // the focus transition without inferring focus from popup close reasons.
        conn.display()
            .sync(qh, FocusBarrier(self.protocol.focus_epoch));
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
    popup_commands: Sender<PopupCommand>,
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
        state.drain_popup_commands();
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
        |_, queue, state: &mut State| {
            let result = queue.dispatch_pending(state);
            state.drain_popup_commands();
            result
        },
    ) {
        tracing::error!(target: "Main", "input: wayland source: {e}");
        return;
    }
    if let Err(e) = event_loop.run(None, &mut state, |_| {}) {
        tracing::error!(target: "Main", "input: event loop: {e}");
    }
    state.protocol.cancel_pointer(state.modifiers);
    state.protocol.cancel_keyboard(state.modifiers);
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
    let (popup_tx, popup_commands) = unbounded();
    let xdg_shell = XdgShell::bind(&globals, &qh).ok()?;
    let viewporter: WpViewporter = globals.bind(&qh, 1..=1, ()).ok()?;
    let pool = smithay_client_toolkit::shm::slot::SlotPool::new(4 * 1024 * 1024, &shm).ok();
    let mut protocol = SeatInput::default();
    let window = rt.root().window()?;
    protocol.register(window.wl_surface().clone(), Arc::new(ContentInput { cb }));

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
        qh: qh.clone(),
        seat: seat.clone(),
        popups: Popups::new(xdg_shell, viewporter, pool),
        protocol,
        popup_commands,
        pointer_serial: 0,
        scroll_dx: 0.0,
        scroll_dy: 0.0,
        scroll_v120_x: 0,
        scroll_v120_y: 0,
        scroll_have_v120: false,
        xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb_kmap: None,
        modifiers: 0,
        cursor_type: cursor_type.clone(),
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
        popup_commands: popup_tx,
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
    pub(crate) fn popup(&self, command: PopupCommand) {
        if self.popup_commands.send(command).is_ok() {
            self.ping.ping();
        }
    }

    pub(crate) fn set_cursor(&self, cef_cursor_type: u32) {
        self.cursor_type.store(cef_cursor_type, Ordering::Release);
        self.ping.ping();
    }

    /// Stop the worker and join it. Idempotent: a second call finds the join
    /// handle already taken.
    pub(crate) fn shutdown(&self, _rt: &'static WlRuntime) {
        self.stop.store(true, Ordering::Relaxed);
        self.ping.ping();
        if let Some(w) = self.worker.lock().take() {
            let _ = w.join();
        }
    }
}

/// Application encoding lives at the registered content endpoint.
struct ContentInput {
    cb: Callbacks,
}
impl InputTarget for ContentInput {
    fn motion(&self, position: (f64, f64), modifiers: u32, leave: bool) {
        if let Some(f) = self.cb.mouse_move {
            f(
                position.0 as i32,
                position.1 as i32,
                modifiers,
                i32::from(leave),
            );
        }
    }
    fn button(&self, button: u32, pressed: bool, position: (f64, f64), modifiers: u32) {
        if matches!(button, BTN_SIDE | BTN_EXTRA | BTN_BACK | BTN_FORWARD) {
            if pressed && let Some(f) = self.cb.history_nav {
                f(i32::from(matches!(button, BTN_EXTRA | BTN_FORWARD)));
            }
            return;
        }
        if let Some(f) = self.cb.mouse_button {
            f(
                button,
                i32::from(pressed),
                position.0 as i32,
                position.1 as i32,
                modifiers,
            );
        }
    }
    fn scroll(&self, position: (f64, f64), dx: i32, dy: i32, modifiers: u32) {
        if let Some(f) = self.cb.scroll {
            f(position.0 as i32, position.1 as i32, dx, dy, modifiers);
        }
    }
    fn key(&self, event: &KeyEvent, pressed: bool, modifiers: u32) {
        if let Some(f) = self.cb.key {
            f(
                event.keysym.raw(),
                event.raw_code,
                modifiers,
                if pressed { 1 } else { 0 },
            );
        }
        if !pressed {
            return;
        }
        if let Some(composed) = jfn_linux_util::input::compose_feed(event.keysym.raw()) {
            jfn_input::jfn_input_dispatch_text(&composed, modifiers);
            return;
        }
        if jfn_linux_util::input::compose_pending() {
            return;
        }
        if let Some(f) = self.cb.char_
            && let Some(text) = &event.utf8
        {
            for ch in text.chars() {
                f(ch as u32, modifiers, event.raw_code);
            }
        }
    }
}

pub(crate) struct FocusBarrier(u64);
impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, FocusBarrier> for State {
    fn event(
        state: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        barrier: &FocusBarrier,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if barrier.0 == state.protocol.focus_epoch {
            state.reconcile_keyboard_focus();
        }
    }
}

impl State {
    fn reconcile_keyboard_focus(&mut self) {
        let focused = self.protocol.has_keyboard_focus();
        if self.protocol.focused != focused {
            self.protocol.focused = focused;
            if let Some(f) = self.cb.kb_focus {
                f(i32::from(focused));
            }
        }
    }

    fn drain_popup_commands(&mut self) {
        while let Ok(command) = self.popup_commands.try_recv() {
            match command {
                PopupCommand::Create {
                    generation,
                    anchor,
                    serial,
                    input,
                    parent,
                } => self.create_popup(generation, anchor, serial, input, parent),
                PopupCommand::Map { generation } => self.map_popup(generation),
                PopupCommand::Reposition { generation, place } => {
                    self.reposition_popup(generation, place)
                }
                PopupCommand::Paint(paint) => self.paint_popup(paint),
                PopupCommand::Destroy { generation } => self.destroy_popup(generation),
            }
        }
    }
}
