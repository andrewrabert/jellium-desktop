use std::ffi::c_int;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use calloop::channel::{Channel, Sender};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, LoopSignal, RegistrationToken};
use parking_lot::Mutex;
use x11rb::connection::Connection;
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
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
    #[error(transparent)]
    Anchor(#[from] Unrepresentable),
    #[error(transparent)]
    Id(#[from] ReplyOrIdError),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Reply(#[from] ReplyError),
    #[error("{cause}; releasing the window failed: {cleanup}")]
    Unwind {
        #[source]
        cause: Box<BuildError>,
        cleanup: ConnectionError,
    },
}

#[derive(Debug, thiserror::Error)]
enum PresentError {
    #[error("no shared memory buffer")]
    Shm,
    #[error(transparent)]
    Connection(#[from] ConnectionError),
}

#[derive(Debug, thiserror::Error)]
enum QueueError {
    #[error("popup thread not running")]
    NotRunning,
    #[error("popup thread gone")]
    Gone,
}

pub fn start() -> Result<(), MenuSpawnError> {
    if MENU.get().is_some() {
        return Ok(());
    }
    let surface = Arc::new(X11PopupSurface {
        tx: Mutex::new(None),
    });
    spawn_popup(&surface).map_err(|source| MenuSpawnError::Thread {
        thread: "menu popup",
        source,
    })?;
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

    fn hide(&self, handle: MenuHandle) -> Result<(), jfn_platform_abi::MenuHideError> {
        MENU.get().map_or(Ok(()), |menu| menu.hide(handle))
    }

    fn shutdown(&self) -> Result<(), MenuShutdownError> {
        MENU.get().map_or(Ok(()), MenuHost::shutdown)
    }
}

struct X11PopupSurface {
    tx: Mutex<Option<Sender<Op>>>,
}

impl X11PopupSurface {
    fn send(&self, op: Op) -> Result<(), QueueError> {
        let slot = self.tx.lock();
        let tx = slot.as_ref().ok_or(QueueError::NotRunning)?;
        tx.send(op).map_err(|_| QueueError::Gone)
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
        if let Err(e) = self.send(Op::Create { generation, place }) {
            tracing::error!(target: "x11::menu", "create: {e}; dismissing menu");
            dismiss(generation);
        }
    }

    fn present(&self, paint: MenuPaint) {
        if let Err(e) = self.send(Op::Present(paint)) {
            tracing::error!(target: "x11::menu", "present: {e}");
        }
    }

    fn destroy(&self, generation: Generation, _reason: MenuClose) {
        if let Err(e) = self.send(Op::Destroy { generation }) {
            tracing::error!(target: "x11::menu", "destroy: {e}");
        }
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

fn spawn_popup(surface: &Arc<X11PopupSurface>) -> std::io::Result<()> {
    let (tx, rx) = calloop::channel::channel::<Op>();
    let thread_surface = Arc::clone(surface);
    std::thread::Builder::new()
        .name("jfn-x11-menu".into())
        .spawn(move || popup_thread(&thread_surface, rx))?;
    *surface.tx.lock() = Some(tx);
    Ok(())
}

fn popup_thread(surface: &Arc<X11PopupSurface>, rx: Channel<Op>) {
    let (conn, screen) = match x11rb::connect(None) {
        Ok(connected) => connected,
        Err(e) => {
            tracing::error!(target: "x11::menu", "popup: X11 connect: {e}; menus disabled");
            surface.close(Some(rx));
            return;
        }
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
    let raw_root = match conn
        .xinput_xi_query_version(2, 1)
        .map_err(ReplyError::from)
        .and_then(|cookie| cookie.reply())
    {
        Ok(version) => ((version.major_version, version.minor_version) >= (2, 1))
            .then(|| conn.setup().roots[screen].root),
        Err(e) => {
            tracing::warn!(target: "x11::menu", "popup: XInput query: {e}; grabs wait out contention");
            None
        }
    };
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
    state.tear_down_reporting("popup exit");
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
                self.tear_down_reporting("channel closed");
                self.signal.stop();
            }
        }
    }

    fn on_op(&mut self, op: Op) {
        match op {
            Op::Create { generation, place } => self.create(generation, place),
            Op::Present(paint) => {
                let generation = paint.generation;
                if let Err(e) = self.present(paint) {
                    tracing::error!(target: "x11::menu", "present {generation}: {e}");
                }
            }
            Op::Destroy { generation } => {
                if self.owns(generation) {
                    self.tear_down_reporting("destroy");
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
        self.tear_down_reporting("create");
        let window = match self.build(generation, place) {
            Ok(window) => window,
            Err(e) => {
                tracing::error!(target: "x11::menu", "build: {e}; dismissing");
                dismiss(generation);
                return;
            }
        };
        self.phase = Phase::Grabbing(window);
        if let Err(e) = self.watch_releases(true) {
            tracing::warn!(target: "x11::menu", "raw release subscription: {e}; the grab deadline still applies");
        }
        self.report_grab();
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
                    st.report_grab();
                    if matches!(st.phase, Phase::Grabbing(_)) {
                        tracing::error!(target: "x11::menu", "grab: deadline expired; dismissing");
                        st.tear_down_reporting("grab deadline");
                        dismiss(generation);
                    }
                }
                TimeoutAction::Drop
            }) {
            Ok(token) => self.grab_timeout = Some(token),
            Err(_) => {
                tracing::error!(target: "x11::menu", "create: grab deadline setup failed; dismissing");
                self.tear_down_reporting("grab deadline setup");
                dismiss(generation);
            }
        }
    }

    fn watch_releases(&self, enabled: bool) -> Result<(), ReplyError> {
        let Some(root) = self.raw_root else {
            return Ok(());
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
        self.conn.xinput_xi_select_events(root, &masks)?.check()
    }

    fn finish_grab_wait(&mut self) -> Result<(), ReplyError> {
        if let Some(token) = self.grab_timeout.take() {
            self.handle.remove(token);
        }
        self.watch_releases(false)
    }

    fn build(
        &mut self,
        generation: Generation,
        place: MenuPlacement,
    ) -> Result<Window, BuildError> {
        let (anchor, size) = (place.anchor, place.view.physical());
        let site = menu_site(&self.conn).ok_or(BuildError::NoMenuSite)?;
        let (wx, wy) = self.place(&site, anchor, size)?;
        let win = self.conn.generate_id()?;
        let aux = CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE)
            .colormap(site.colormap);
        self.conn.create_window(
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
        )?;
        let gc = match self.conn.generate_id() {
            Ok(gc) => gc,
            Err(e) => return Err(self.unwind(e.into(), win, None)),
        };
        if let Err(e) = self.realize(win, gc) {
            return Err(self.unwind(e, win, Some(gc)));
        }
        tracing::debug!(target: "x11::menu", "build: window 0x{win:x} created+mapped");
        Ok(Window {
            generation,
            logical: place.view.logical(),
            win,
            gc,
            buf: ShmBuffer::empty(),
        })
    }

    fn unwind(&self, cause: BuildError, win: u32, gc: Option<u32>) -> BuildError {
        let released = gc
            .map_or(Ok(()), |gc| self.conn.free_gc(gc).map(drop))
            .and_then(|()| self.conn.destroy_window(win).map(drop));
        match released {
            Ok(()) => cause,
            Err(cleanup) => BuildError::Unwind {
                cause: Box::new(cause),
                cleanup,
            },
        }
    }

    fn realize(&self, win: u32, gc: u32) -> Result<(), BuildError> {
        let conn = &*self.conn;
        let create_gc = conn.create_gc(gc, win, &CreateGCAux::new())?;
        let map = conn.map_window(win)?;
        let raise =
            conn.configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        conn.get_geometry(win)
            .map_err(ReplyError::from)
            .and_then(|cookie| cookie.reply())?;
        create_gc.check()?;
        map.check()?;
        raise.check()?;
        Ok(())
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

    fn present(&mut self, paint: MenuPaint) -> Result<(), PresentError> {
        if !self.owns(paint.generation) {
            return Ok(());
        }
        let conn = Arc::clone(&self.conn);
        let Some(window) = self.phase.window() else {
            return Ok(());
        };
        let (w, h) = (paint.buffer.w.max(1), paint.buffer.h.max(1));
        if !shm_alloc(&mut window.buf, &conn, w, h) {
            return Err(PresentError::Shm);
        }
        let pixels = window.buf.pixels_mut();
        let len = pixels.len().min(paint.pixels.len());
        pixels[..len].copy_from_slice(&paint.pixels[..len]);
        conn.shm_put_image(
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
        )?;
        Ok(conn.flush()?)
    }

    fn try_grab(&mut self) -> Result<(), GrabError> {
        let Phase::Grabbing(window) = &self.phase else {
            return Ok(());
        };
        let (generation, logical) = (window.generation, window.logical);
        match grab_modal(&self.conn, window.win) {
            GrabAttempt::Ready => {
                let unwatched = self.finish_grab_wait();
                let Phase::Grabbing(window) = std::mem::replace(&mut self.phase, Phase::Idle)
                else {
                    return unwatched.map_err(GrabError::Unwatch);
                };
                self.phase = Phase::Open(window);
                tracing::debug!(target: "x11::menu", "grab: menu is modal");
                if let Some(menu) = MENU.get() {
                    menu.on_ready(generation, logical);
                }
                unwatched.map_err(GrabError::Unwatch)
            }
            GrabAttempt::Busy => Ok(()),
            GrabAttempt::Failed => {
                let torn_down = self.tear_down();
                dismiss(generation);
                match torn_down {
                    Ok(()) => Err(GrabError::Refused),
                    Err(e) => Err(GrabError::RefusedTearDown(e)),
                }
            }
        }
    }

    fn report_grab(&mut self) {
        if let Err(e) = self.try_grab() {
            tracing::error!(target: "x11::menu", "grab: {e}");
        }
    }

    fn on_event(&mut self, ev: Event) {
        if matches!(self.phase, Phase::Grabbing(_)) && grab_wakeup(&ev) {
            self.report_grab();
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

    fn tear_down_reporting(&mut self, context: &str) {
        if let Err(e) = self.tear_down() {
            tracing::error!(target: "x11::menu", "tear_down ({context}): {e}");
        }
    }

    fn tear_down(&mut self) -> Result<(), ReplyError> {
        let unwatched = self.finish_grab_wait();
        let (Phase::Grabbing(mut window) | Phase::Open(mut window)) =
            std::mem::replace(&mut self.phase, Phase::Idle)
        else {
            return unwatched;
        };
        let pointer = self.conn.ungrab_pointer(x11rb::CURRENT_TIME).map(drop);
        let keyboard = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME).map(drop);
        shm_free(&mut window.buf, Some(&*self.conn));
        let gc = self.conn.free_gc(window.gc).map(drop);
        let destroyed = self.conn.destroy_window(window.win).map(drop);
        let flushed = self.conn.flush();
        tracing::debug!(target: "x11::menu", "tear_down: window 0x{:x} gone", window.win);
        unwatched?;
        pointer.and(keyboard).and(gc).and(destroyed).and(flushed)?;
        Ok(())
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

#[derive(Debug, thiserror::Error)]
enum GrabError {
    #[error("non-retryable failure; dismissed")]
    Refused,
    #[error("non-retryable failure; dismissed, teardown: {0}")]
    RefusedTearDown(#[source] ReplyError),
    #[error("modal, but raw release unsubscription: {0}")]
    Unwatch(#[source] ReplyError),
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
