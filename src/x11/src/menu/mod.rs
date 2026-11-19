use std::ffi::c_int;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use calloop::channel::{Channel, Sender};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, LoopSignal, RegistrationToken};
use parking_lot::Mutex;
use x11rb::connection::Connection;
use x11rb::errors::{ConnectionError, ReplyOrIdError};
use x11rb::protocol::Event;
use x11rb::protocol::shm::ConnectionExt as ShmConnectionExt;
use x11rb::protocol::xinput::{self, ConnectionExt as _, XIEventMask};
use x11rb::protocol::xproto::{
    ConfigureWindowAux, ConnectionExt as XprotoConnectionExt, CreateGCAux, CreateWindowAux,
    EventMask, GrabMode, GrabStatus, ImageFormat, StackMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use jfn_linux_util::menu::{MenuInputEvent, MenuPoint, MenuSpawnError, SoftwareMenu};
use jfn_platform_abi::{
    Generation, MENU_DISMISSED, MenuClose, MenuHandle, MenuHost, MenuMetrics, MenuOpenError,
    MenuPaint, MenuPlacement, MenuRequest, MenuShutdownError, PhysicalSize, PopupSurface,
    Unrepresentable, WindowBasis, WindowPoint,
};

use crate::conn_source::X11Source;
use crate::shm::{shm_alloc, shm_free};
use crate::x11_state::ShmBuffer;

// Preserve the former 40 × 5 ms failure bound, without periodic retries.
const GRAB_WAIT: Duration = Duration::from_millis(200);

static MENU: OnceLock<SoftwareMenu> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
enum BuildError {
    #[error("no host services, paint services, published parent or screen")]
    NoMenuSite,
    #[error("anchor: {0}")]
    Anchor(#[from] Unrepresentable),
    #[error("window id: {0}")]
    WindowId(#[source] ReplyOrIdError),
    #[error("create_window: {0}")]
    CreateWindow(#[source] ConnectionError),
    #[error("graphics context id: {0}")]
    GcId(#[source] ReplyOrIdError),
}

pub fn start() -> Result<(), MenuSpawnError> {
    if MENU.get().is_some() {
        return Ok(());
    }
    let surface = Arc::new(X11PopupSurface {
        tx: Mutex::new(None),
    });
    spawn_popup(&surface);
    let menu = SoftwareMenu::spawn(surface)?;
    MENU.get_or_init(|| menu).warm();
    Ok(())
}

fn dismiss(generation: Generation) {
    if let Some(menu) = MENU.get() {
        menu.on_done(generation);
    }
}

pub(crate) struct X11MenuHost;

impl MenuHost for X11MenuHost {
    fn open(&self, req: MenuRequest) -> Result<MenuHandle, MenuOpenError> {
        let Some(menu) = MENU.get() else {
            req.on_selected.resolve(MENU_DISMISSED);
            return Err(MenuOpenError::EngineUnavailable);
        };
        menu.open(req)
    }

    fn hide(&self, handle: MenuHandle) {
        if let Some(menu) = MENU.get() {
            menu.hide(handle);
        }
    }

    fn shutdown(&self) -> Result<(), MenuShutdownError> {
        MENU.get().map_or(Ok(()), MenuHost::shutdown)
    }
}

struct X11PopupSurface {
    tx: Mutex<Option<Sender<Op>>>,
}

impl X11PopupSurface {
    /// False when the op could not be queued for the popup thread.
    fn send(&self, op: Op) -> bool {
        let slot = self.tx.lock();
        let Some(tx) = slot.as_ref() else {
            return false;
        };
        tx.send(op).is_ok()
    }

    /// Stops accepting ops, then dismisses every menu left in the queue. `rx` is
    /// `None` once the event loop owns the channel and it cannot be reclaimed.
    fn close(&self, rx: Option<Channel<Op>>) {
        let doomed: Vec<Generation> = {
            let mut slot = self.tx.lock();
            *slot = None;
            rx.into_iter()
                .flat_map(|rx| {
                    std::iter::from_fn(move || rx.try_recv().ok()).filter_map(|op| match op {
                        Op::Create { generation, .. } => Some(generation),
                        _ => None,
                    })
                })
                .collect()
        };
        for generation in doomed {
            dismiss(generation);
        }
    }
}

impl PopupSurface for X11PopupSurface {
    fn metrics(&self) -> MenuMetrics {
        MenuMetrics {
            scale: crate::scale::window_scale(),
            clamp_ph: None,
        }
    }

    fn window_basis(&self) -> WindowBasis {
        WindowBasis::FIXED
    }

    fn create(&self, generation: Generation, place: MenuPlacement) {
        if !self.send(Op::Create { generation, place }) {
            tracing::error!(target: "x11::menu", "popup thread gone; dismissing menu");
            dismiss(generation);
        }
    }

    fn present(&self, paint: MenuPaint) {
        self.send(Op::Present(paint));
    }

    fn destroy(&self, generation: Generation, _reason: MenuClose) {
        self.send(Op::Destroy { generation });
    }
}

enum Op {
    Create {
        generation: Generation,
        place: MenuPlacement,
    },
    Present(MenuPaint),
    Destroy {
        generation: Generation,
    },
}

/// Installs the sender and starts the popup thread; on spawn failure the slot
/// is left empty and menus dismiss on arrival.
fn spawn_popup(surface: &Arc<X11PopupSurface>) {
    let (tx, rx) = calloop::channel::channel::<Op>();
    let thread_surface = Arc::clone(surface);
    match std::thread::Builder::new()
        .name("jfn-x11-menu".into())
        .spawn(move || popup_thread(&thread_surface, rx))
    {
        Ok(_) => *surface.tx.lock() = Some(tx),
        Err(e) => {
            tracing::error!(target: "x11::menu", "popup thread spawn failed: {e}; menus disabled");
        }
    }
}

fn popup_thread(surface: &Arc<X11PopupSurface>, rx: Channel<Op>) {
    let Ok((conn, screen)) = x11rb::connect(None) else {
        tracing::error!(target: "x11::menu", "popup: X11 connect failed; menus disabled");
        surface.close(Some(rx));
        return;
    };
    let conn = Arc::new(conn);
    let Ok(mut event_loop) = EventLoop::<'static, PopupLoop>::try_new() else {
        tracing::error!(target: "x11::menu", "popup: calloop init failed; menus disabled");
        surface.close(Some(rx));
        return;
    };
    let handle = event_loop.handle();
    if handle
        .insert_source(X11Source::new(conn.clone()), |ev, (), st| st.on_event(ev))
        .is_err()
    {
        tracing::error!(target: "x11::menu", "popup: event source setup failed; menus disabled");
        surface.close(Some(rx));
        return;
    }
    if let Err(e) = handle.insert_source(rx, |event, _, st| st.on_channel(event)) {
        tracing::error!(target: "x11::menu", "popup: event source setup failed; menus disabled");
        surface.close(Some(e.inserted));
        return;
    }
    // XI 2.1 delivers raw releases even while another client owns the grab.
    // Select before the first attempt, so a release cannot be lost between a
    // failed grab and arming its wakeup. Selection lasts only while waiting.
    let raw_root = conn
        .xinput_xi_query_version(2, 1)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .filter(|version| (version.major_version, version.minor_version) >= (2, 1))
        .map(|_| conn.setup().roots[screen].root);
    let mut state = PopupLoop {
        keymap: Keymap::query(&conn),
        conn,
        phase: Phase::Idle,
        raw_root,
        grab_timeout: None,
        handle,
        signal: event_loop.get_signal(),
    };
    tracing::debug!(target: "x11::menu", "popup: started");
    if let Err(e) = event_loop.run(None, &mut state, |_| {}) {
        tracing::error!(target: "x11::menu", "popup: loop error: {e}");
    }
    state.tear_down();
    surface.close(None);
}

enum Phase {
    Idle,
    Grabbing(Window),
    Open(Window),
}

impl Phase {
    fn window(&mut self) -> Option<&mut Window> {
        match self {
            Phase::Idle => None,
            Phase::Grabbing(window) | Phase::Open(window) => Some(window),
        }
    }
}

struct Window {
    generation: Generation,
    logical: jfn_platform_abi::LogicalSize,
    win: u32,
    gc: u32,
    buf: ShmBuffer,
}

struct PopupLoop {
    conn: Arc<RustConnection>,
    keymap: Keymap,
    phase: Phase,
    handle: LoopHandle<'static, PopupLoop>,
    raw_root: Option<u32>,
    grab_timeout: Option<RegistrationToken>,
    signal: LoopSignal,
}

impl PopupLoop {
    fn on_channel(&mut self, event: calloop::channel::Event<Op>) {
        match event {
            calloop::channel::Event::Msg(op) => self.on_op(op),
            calloop::channel::Event::Closed => {
                self.tear_down();
                self.signal.stop();
            }
        }
    }

    fn on_op(&mut self, op: Op) {
        match op {
            Op::Create { generation, place } => self.create(generation, place),
            Op::Present(paint) => self.present(paint),
            Op::Destroy { generation } => {
                if self.owns(generation) {
                    self.tear_down();
                }
            }
        }
    }

    fn owns(&mut self, generation: Generation) -> bool {
        self.phase
            .window()
            .is_some_and(|w| w.generation == generation)
    }

    fn create(&mut self, generation: Generation, place: MenuPlacement) {
        self.tear_down();
        let window = match self.build(generation, place) {
            Ok(window) => window,
            Err(e) => {
                tracing::error!(target: "x11::menu", "build: {e}; dismissing");
                dismiss(generation);
                return;
            }
        };
        self.phase = Phase::Grabbing(window);
        self.watch_releases(true);
        self.try_grab();
        if !matches!(self.phase, Phase::Grabbing(_)) {
            return;
        }
        match self
            .handle
            .insert_source(Timer::from_duration(GRAB_WAIT), move |_, _, st| {
                if st.owns(generation) && matches!(st.phase, Phase::Grabbing(_)) {
                    st.grab_timeout = None;
                    // One final attempt covers owners releasing an explicit grab
                    // without a button/key event. This is a failure deadline, not
                    // a recurring poll.
                    st.try_grab();
                    if matches!(st.phase, Phase::Grabbing(_)) {
                        tracing::error!(target: "x11::menu", "grab: deadline expired; dismissing");
                        st.tear_down();
                        dismiss(generation);
                    }
                }
                TimeoutAction::Drop
            }) {
            Ok(token) => self.grab_timeout = Some(token),
            Err(_) => {
                tracing::error!(target: "x11::menu", "create: grab deadline setup failed; dismissing");
                self.tear_down();
                dismiss(generation);
            }
        }
    }

    fn watch_releases(&self, enabled: bool) {
        let Some(root) = self.raw_root else {
            return;
        };
        let mask = if enabled {
            vec![XIEventMask::RAW_BUTTON_RELEASE | XIEventMask::RAW_KEY_RELEASE]
        } else {
            Vec::new()
        };
        let masks = [xinput::EventMask {
            deviceid: xinput::Device::ALL_MASTER.into(),
            mask,
        }];
        if let Err(e) = self
            .conn
            .xinput_xi_select_events(root, &masks)
            .map_err(|e| e.to_string())
            .and_then(|cookie| cookie.check().map_err(|e| e.to_string()))
        {
            tracing::debug!(target: "x11::menu", "raw release subscription failed: {e}");
        }
    }

    fn finish_grab_wait(&mut self) {
        if let Some(token) = self.grab_timeout.take() {
            self.handle.remove(token);
        }
        self.watch_releases(false);
    }

    fn build(
        &mut self,
        generation: Generation,
        place: MenuPlacement,
    ) -> Result<Window, BuildError> {
        let (anchor, size) = (place.anchor, place.view.physical());
        let site = menu_site(&self.conn).ok_or(BuildError::NoMenuSite)?;
        let (wx, wy) = self.place(&site, anchor, size)?;
        let win = self.conn.generate_id().map_err(BuildError::WindowId)?;
        let aux = CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE)
            .colormap(site.colormap);
        self.conn
            .create_window(
                site.depth,
                win,
                site.root,
                wx as i16,
                wy as i16,
                size.w as u16,
                size.h as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                site.visual,
                &aux,
            )
            .map_err(BuildError::CreateWindow)?;
        let gc = match self.conn.generate_id() {
            Ok(gc) => gc,
            Err(e) => {
                let _ = self.conn.destroy_window(win);
                return Err(BuildError::GcId(e));
            }
        };
        let _ = self.conn.create_gc(gc, win, &CreateGCAux::new());
        let _ = self.conn.map_window(win);
        let _ = self
            .conn
            .configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        // Round-trip on the grabbing connection before grabbing — the window
        // must be realized server-side or the grab races into a BadWindow.
        let _ = self
            .conn
            .get_geometry(win)
            .ok()
            .and_then(|c| c.reply().ok());
        tracing::debug!(target: "x11::menu", "build: window 0x{win:x} created+mapped");
        Ok(Window {
            generation,
            logical: place.view.logical(),
            win,
            gc,
            buf: ShmBuffer::empty(),
        })
    }

    /// The root-relative top-left of a window of `size` whose anchor is
    /// `anchor`, kept inside the root.
    fn place(
        &self,
        site: &MenuSite,
        anchor: WindowPoint,
        size: PhysicalSize,
    ) -> Result<(i32, i32), Unrepresentable> {
        let (w, h) = (size.w, size.h);
        let (anchor_x, anchor_y) = anchor.floor();
        let mut x = site.parent_x + site.scale.to_physical(anchor_x)?;
        let mut y = site.parent_y + site.scale.to_physical(anchor_y)?;
        if x + w > site.root_w {
            x = (site.root_w - w).max(0);
        }
        if y + h > site.root_h {
            let above = y - h;
            y = if above >= 0 {
                above
            } else {
                (site.root_h - h).max(0)
            };
        }
        Ok((x.max(0), y.max(0)))
    }

    fn present(&mut self, paint: MenuPaint) {
        if !self.owns(paint.generation) {
            return;
        }
        let conn = Arc::clone(&self.conn);
        let Some(window) = self.phase.window() else {
            return;
        };
        let (w, h) = (paint.buffer.w.max(1), paint.buffer.h.max(1));
        if !shm_alloc(&mut window.buf, &conn, w, h) {
            return;
        }
        let pixels = window.buf.pixels_mut();
        let len = pixels.len().min(paint.pixels.len());
        pixels[..len].copy_from_slice(&paint.pixels[..len]);
        let _ = conn.shm_put_image(
            window.win,
            window.gc,
            w as u16,
            h as u16,
            0,
            0,
            w as u16,
            h as u16,
            0,
            0,
            32,
            u8::from(ImageFormat::Z_PIXMAP),
            false,
            window.buf.seg(),
            0,
        );
        let _ = conn.flush();
    }

    fn try_grab(&mut self) {
        let Phase::Grabbing(window) = &self.phase else {
            return;
        };
        let (generation, logical) = (window.generation, window.logical);
        match grab_modal(&self.conn, window.win) {
            GrabAttempt::Ready => {
                self.finish_grab_wait();
                let Phase::Grabbing(window) = std::mem::replace(&mut self.phase, Phase::Idle)
                else {
                    return;
                };
                self.phase = Phase::Open(window);
                tracing::debug!(target: "x11::menu", "grab: menu is modal");
                if let Some(menu) = MENU.get() {
                    menu.on_ready(generation, logical);
                }
            }
            GrabAttempt::Busy => {}
            GrabAttempt::Failed => {
                tracing::error!(target: "x11::menu", "grab: non-retryable failure; dismissing");
                self.tear_down();
                dismiss(generation);
            }
        }
    }

    fn on_event(&mut self, ev: Event) {
        if matches!(self.phase, Phase::Grabbing(_)) && grab_wakeup(&ev) {
            self.try_grab();
            return;
        }
        let Phase::Open(window) = &self.phase else {
            return;
        };
        let pointer = |x: i16, y: i16, press: bool| MenuInputEvent::Pointer {
            at: MenuPoint::Physical {
                x: c_int::from(x),
                y: c_int::from(y),
            },
            press,
        };
        let event = match ev {
            Event::Expose(_) => MenuInputEvent::Expose,
            Event::MotionNotify(e) => pointer(e.event_x, e.event_y, false),
            Event::ButtonPress(e) => pointer(e.event_x, e.event_y, true),
            Event::KeyPress(e) => MenuInputEvent::Key(self.keymap.lookup(e.detail)),
            _ => return,
        };
        if let Some(menu) = MENU.get() {
            menu.input(window.generation, event);
        }
    }

    fn tear_down(&mut self) {
        self.finish_grab_wait();
        let (Phase::Grabbing(mut window) | Phase::Open(mut window)) =
            std::mem::replace(&mut self.phase, Phase::Idle)
        else {
            return;
        };
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        shm_free(&mut window.buf, Some(&*self.conn));
        let _ = self.conn.free_gc(window.gc);
        let _ = self.conn.destroy_window(window.win);
        let _ = self.conn.flush();
        tracing::debug!(target: "x11::menu", "tear_down: window 0x{:x} gone", window.win);
    }
}

struct MenuSite {
    visual: u32,
    depth: u8,
    colormap: u32,
    root: u32,
    parent_x: i32,
    parent_y: i32,
    scale: jfn_platform_abi::Scale,
    root_w: i32,
    root_h: i32,
}

fn menu_site(conn: &RustConnection) -> Option<MenuSite> {
    let host = crate::x11_state::host()?;
    let paint = crate::x11_state::paint()?;
    let parent = crate::x11_state::published_parent()?;
    let screen = conn
        .setup()
        .roots
        .iter()
        .find(|s| s.root == host.root)
        .or_else(|| conn.setup().roots.first())?;
    Some(MenuSite {
        visual: paint.argb_visual,
        depth: paint.argb_depth,
        colormap: paint.colormap,
        root: host.root,
        parent_x: parent.origin_x,
        parent_y: parent.origin_y,
        scale: parent.scale,
        root_w: screen.width_in_pixels as i32,
        root_h: screen.height_in_pixels as i32,
    })
}

fn grab_wakeup(event: &Event) -> bool {
    matches!(
        event,
        Event::XinputRawButtonRelease(_) | Event::XinputRawKeyRelease(_)
    )
}

#[derive(Debug, PartialEq, Eq)]
enum GrabAttempt {
    Ready,
    Busy,
    Failed,
}

fn grab_status(status: GrabStatus) -> GrabAttempt {
    match status {
        GrabStatus::SUCCESS => GrabAttempt::Ready,
        GrabStatus::ALREADY_GRABBED | GrabStatus::FROZEN => GrabAttempt::Busy,
        _ => GrabAttempt::Failed,
    }
}

fn grab_modal(conn: &RustConnection, win: u32) -> GrabAttempt {
    let pointer = conn
        .grab_pointer(
            false,
            win,
            EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            x11rb::CURRENT_TIME,
        )
        .ok()
        .and_then(|cookie| cookie.reply().ok());
    let pointer = pointer.map_or(GrabAttempt::Failed, |reply| grab_status(reply.status));
    if pointer != GrabAttempt::Ready {
        return pointer;
    }
    let keyboard = conn
        .grab_keyboard(
            false,
            win,
            x11rb::CURRENT_TIME,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )
        .ok()
        .and_then(|cookie| cookie.reply().ok());
    let keyboard = keyboard.map_or(GrabAttempt::Failed, |reply| grab_status(reply.status));
    if keyboard != GrabAttempt::Ready {
        // Do not hold half a modal grab while waiting for another owner.
        let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = conn.flush();
    }
    keyboard
}

struct Keymap {
    min_keycode: u8,
    per: u8,
    syms: Vec<u32>,
}

impl Keymap {
    fn query(conn: &RustConnection) -> Self {
        let setup = conn.setup();
        let min = setup.min_keycode;
        let max = setup.max_keycode;
        let count = max - min + 1;
        let syms = conn
            .get_keyboard_mapping(min, count)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| (r.keysyms_per_keycode, r.keysyms))
            .unwrap_or((0, Vec::new()));
        Self {
            min_keycode: min,
            per: syms.0,
            syms: syms.1,
        }
    }

    fn lookup(&self, keycode: u8) -> u32 {
        if self.per == 0 || keycode < self.min_keycode {
            return 0;
        }
        let idx = (keycode - self.min_keycode) as usize * self.per as usize;
        self.syms.get(idx).copied().unwrap_or(0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod grab_tests {
    use super::*;
    use x11rb::protocol::xtest::ConnectionExt as _;

    #[test]
    #[ignore = "requires an isolated X11 server with XInput 2.1 and XTEST"]
    fn contention_release_wakeup_and_partial_grab_cleanup() {
        let (owner, screen) = x11rb::connect(None).unwrap();
        let (popup, _) = x11rb::connect(None).unwrap();
        let root = owner.setup().roots[screen].root;
        let win = popup.generate_id().unwrap();
        popup
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                win,
                root,
                0,
                0,
                20,
                20,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().override_redirect(1),
            )
            .unwrap()
            .check()
            .unwrap();
        popup.map_window(win).unwrap().check().unwrap();
        let version = popup
            .xinput_xi_query_version(2, 1)
            .unwrap()
            .reply()
            .unwrap();
        assert!((version.major_version, version.minor_version) >= (2, 1));
        popup
            .xinput_xi_select_events(
                root,
                &[xinput::EventMask {
                    deviceid: xinput::Device::ALL_MASTER.into(),
                    mask: vec![XIEventMask::RAW_BUTTON_RELEASE],
                }],
            )
            .unwrap()
            .check()
            .unwrap();

        assert_eq!(grab_modal(&owner, root), GrabAttempt::Ready);
        assert_eq!(grab_modal(&popup, win), GrabAttempt::Busy);
        // Raw release must reach the waiting client even while the other
        // connection owns the pointer. A core ButtonRelease cannot do this.
        owner
            .xtest_fake_input(
                x11rb::protocol::xproto::BUTTON_PRESS_EVENT,
                1,
                x11rb::CURRENT_TIME,
                root,
                0,
                0,
                0,
            )
            .unwrap()
            .check()
            .unwrap();
        owner
            .xtest_fake_input(
                x11rb::protocol::xproto::BUTTON_RELEASE_EVENT,
                1,
                x11rb::CURRENT_TIME,
                root,
                0,
                0,
                0,
            )
            .unwrap()
            .check()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            loop {
                let event = popup.wait_for_event().unwrap();
                if grab_wakeup(&event) {
                    break;
                }
            }
            tx.send(popup).unwrap();
        });
        let popup = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        waiter.join().unwrap();

        owner
            .ungrab_pointer(x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        // The keyboard is still held: a failed keyboard grab must release
        // the pointer it just acquired, so another client can take it.
        assert_eq!(grab_modal(&popup, win), GrabAttempt::Busy);
        assert_eq!(grab_modal(&owner, root), GrabAttempt::Ready);
        owner
            .ungrab_pointer(x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        owner
            .ungrab_keyboard(x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(grab_modal(&popup, win), GrabAttempt::Ready);
        popup
            .ungrab_pointer(x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        popup
            .ungrab_keyboard(x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        popup.destroy_window(win).unwrap().check().unwrap();
    }
}
