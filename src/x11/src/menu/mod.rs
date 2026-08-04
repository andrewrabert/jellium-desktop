use std::ffi::c_int;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use calloop::channel::{Channel, Sender};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, LoopSignal};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::shm::ConnectionExt as ShmConnectionExt;
use x11rb::protocol::xproto::{
    ConfigureWindowAux, ConnectionExt as XprotoConnectionExt, CreateGCAux, CreateWindowAux,
    EventMask, GrabMode, GrabStatus, ImageFormat, StackMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use jfn_linux_util::menu::SoftwareMenu;
use jfn_platform_abi::{
    Generation, MenuClose, MenuHost, MenuMetrics, MenuPaint, MenuPlacement, PopupSurface,
};

use crate::conn_source::X11Source;
use crate::shm::{shm_alloc, shm_free};
use crate::x11_state::ShmBuffer;

const GRAB_RETRY: Duration = Duration::from_millis(5);
const GRAB_ATTEMPTS: u32 = 40;

static MENU: OnceLock<SoftwareMenu> = OnceLock::new();

pub fn warm() {
    host().warm();
}

pub fn host() -> &'static SoftwareMenu {
    MENU.get_or_init(|| SoftwareMenu::spawn(Arc::new(X11PopupSurface { tx: spawn_popup() })))
}

struct X11PopupSurface {
    tx: Option<Sender<Op>>,
}

impl X11PopupSurface {
    fn send(&self, op: Op) {
        let Some(tx) = self.tx.as_ref() else { return };
        if tx.send(op).is_err() {
            tracing::error!(target: "x11::menu", "popup thread gone; menu op dropped");
        }
    }
}

impl PopupSurface for X11PopupSurface {
    fn metrics(&self) -> MenuMetrics {
        let scale = crate::x11_state::parent_snapshot().scale;
        MenuMetrics {
            scale: if scale > 0.0 { scale } else { 1.0 },
            clamp_ph: None,
        }
    }

    fn create(&self, generation: Generation, place: MenuPlacement, _serial: u32) {
        self.send(Op::Create { generation, place });
    }

    fn reposition(&self, generation: Generation, place: MenuPlacement) {
        self.send(Op::Reposition { generation, place });
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
    Reposition {
        generation: Generation,
        place: MenuPlacement,
    },
    Present(MenuPaint),
    Destroy {
        generation: Generation,
    },
}

fn spawn_popup() -> Option<Sender<Op>> {
    let (tx, rx) = calloop::channel::channel::<Op>();
    std::thread::Builder::new()
        .name("jfn-x11-menu".into())
        .spawn(move || popup_thread(rx))
        .ok()?;
    Some(tx)
}

fn popup_thread(rx: Channel<Op>) {
    let Ok((conn, _screen)) = x11rb::connect(None) else {
        tracing::error!(target: "x11::menu", "popup: X11 connect failed; menus disabled");
        drain(rx);
        return;
    };
    let conn = Arc::new(conn);
    let Ok(mut event_loop) = EventLoop::<'static, PopupLoop>::try_new() else {
        tracing::error!(target: "x11::menu", "popup: calloop init failed; menus disabled");
        drain(rx);
        return;
    };
    let handle = event_loop.handle();
    let inserted = handle
        .insert_source(rx, |event, _, st| st.on_channel(event))
        .is_ok()
        && handle
            .insert_source(X11Source::new(conn.clone()), |ev, (), st| st.on_event(ev))
            .is_ok();
    if !inserted {
        tracing::error!(target: "x11::menu", "popup: event source setup failed; menus disabled");
        return;
    }
    let mut state = PopupLoop {
        keymap: Keymap::query(&conn),
        conn,
        phase: Phase::Idle,
        handle,
        signal: event_loop.get_signal(),
    };
    tracing::debug!(target: "x11::menu", "popup: started");
    if let Err(e) = event_loop.run(None, &mut state, |_| {}) {
        tracing::error!(target: "x11::menu", "popup: loop error: {e}");
    }
    state.tear_down();
}

fn drain(rx: Channel<Op>) {
    while rx.recv().is_ok() {}
}

enum Phase {
    Idle,
    Grabbing { window: Window, attempts: u32 },
    Open(Window),
}

impl Phase {
    fn window(&mut self) -> Option<&mut Window> {
        match self {
            Phase::Idle => None,
            Phase::Grabbing { window, .. } | Phase::Open(window) => Some(window),
        }
    }
}

struct Window {
    generation: Generation,
    win: u32,
    gc: u32,
    buf: ShmBuffer,
}

struct PopupLoop {
    conn: Arc<RustConnection>,
    keymap: Keymap,
    phase: Phase,
    handle: LoopHandle<'static, PopupLoop>,
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
            Op::Reposition { generation, place } => self.reposition(generation, place),
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
        let Some(window) = self.build(generation, place) else {
            host().on_done(generation);
            return;
        };
        self.phase = Phase::Grabbing {
            window,
            attempts: 0,
        };
        if self
            .handle
            .insert_source(Timer::from_duration(GRAB_RETRY), |_, _, st| {
                st.on_grab_retry()
            })
            .is_err()
        {
            tracing::error!(target: "x11::menu", "create: grab timer failed; dismissing");
            self.tear_down();
            host().on_done(generation);
        }
    }

    /// On `None`, nothing is left on the server for the caller to clean up.
    fn build(&mut self, generation: Generation, place: MenuPlacement) -> Option<Window> {
        let snap = snapshot(&self.conn).or_else(|| {
            tracing::warn!(target: "x11::menu", "build: no X11 state snapshot; dismissing");
            None
        })?;
        let (wx, wy) = self.place(&snap, place);
        let win = self.conn.generate_id().ok()?;
        let aux = CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE)
            .colormap(snap.colormap);
        if self
            .conn
            .create_window(
                snap.depth,
                win,
                snap.root,
                wx as i16,
                wy as i16,
                place.pw.max(1) as u16,
                place.ph.max(1) as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                snap.visual,
                &aux,
            )
            .is_err()
        {
            tracing::error!(target: "x11::menu", "build: create_window failed");
            return None;
        }
        let Ok(gc) = self.conn.generate_id() else {
            let _ = self.conn.destroy_window(win);
            return None;
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
        Some(Window {
            generation,
            win,
            gc,
            buf: ShmBuffer::empty(),
        })
    }

    fn place(&self, snap: &Snap, place: MenuPlacement) -> (i32, i32) {
        let (w, h) = (place.pw.max(1), place.ph.max(1));
        let mut x = snap.parent_x + (place.x as f32 * snap.scale).round() as i32;
        let mut y = snap.parent_y + (place.y as f32 * snap.scale).round() as i32;
        if x + w > snap.root_w {
            x = (snap.root_w - w).max(0);
        }
        if y + h > snap.root_h {
            let above = y - h;
            y = if above >= 0 {
                above
            } else {
                (snap.root_h - h).max(0)
            };
        }
        (x.max(0), y.max(0))
    }

    fn reposition(&mut self, generation: Generation, place: MenuPlacement) {
        if !self.owns(generation) {
            return;
        }
        let Some(snap) = snapshot(&self.conn) else {
            return;
        };
        let (wx, wy) = self.place(&snap, place);
        let Some(window) = self.phase.window() else {
            return;
        };
        let _ = self.conn.configure_window(
            window.win,
            &ConfigureWindowAux::new()
                .x(wx)
                .y(wy)
                .width(place.pw.max(1) as u32)
                .height(place.ph.max(1) as u32),
        );
        let _ = self.conn.flush();
    }

    fn present(&mut self, paint: MenuPaint) {
        if !self.owns(paint.generation) {
            return;
        }
        let conn = Arc::clone(&self.conn);
        let Some(window) = self.phase.window() else {
            return;
        };
        let (w, h) = (paint.pw.max(1), paint.ph.max(1));
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

    fn on_grab_retry(&mut self) -> TimeoutAction {
        let Phase::Grabbing { window, attempts } = &mut self.phase else {
            return TimeoutAction::Drop;
        };
        let generation = window.generation;
        if grab_pointer(&self.conn, window.win) {
            let _ = self.conn.grab_keyboard(
                false,
                window.win,
                x11rb::CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            );
            let Phase::Grabbing { window, .. } = std::mem::replace(&mut self.phase, Phase::Idle)
            else {
                return TimeoutAction::Drop;
            };
            self.phase = Phase::Open(window);
            tracing::debug!(target: "x11::menu", "grab: menu is modal");
            host().on_ready(generation);
            return TimeoutAction::Drop;
        }
        *attempts += 1;
        if *attempts >= GRAB_ATTEMPTS {
            tracing::error!(target: "x11::menu", "grab: pointer grab failed; dismissing");
            self.tear_down();
            host().on_done(generation);
            return TimeoutAction::Drop;
        }
        TimeoutAction::ToDuration(GRAB_RETRY)
    }

    fn on_event(&mut self, ev: Event) {
        if !matches!(self.phase, Phase::Open(_)) {
            return;
        }
        let scale = crate::x11_state::parent_snapshot().scale;
        let scale = if scale > 0.0 { scale } else { 1.0 };
        // X11 event coordinates are physical; the menu takes logical ones.
        let logical = |v: i16| (v as f32 / scale).round() as c_int;
        match ev {
            Event::Expose(_) => host().expose(),
            Event::MotionNotify(e) => host().motion(logical(e.event_x), logical(e.event_y)),
            Event::ButtonPress(e) => host().press(logical(e.event_x), logical(e.event_y)),
            Event::KeyPress(e) => host().key(self.keymap.lookup(e.detail)),
            _ => {}
        }
    }

    fn tear_down(&mut self) {
        let (Phase::Grabbing { mut window, .. } | Phase::Open(mut window)) =
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

struct Snap {
    visual: u32,
    depth: u8,
    colormap: u32,
    root: u32,
    parent_x: i32,
    parent_y: i32,
    scale: f32,
    root_w: i32,
    root_h: i32,
}

fn snapshot(conn: &RustConnection) -> Option<Snap> {
    let host = crate::x11_state::host()?;
    let paint = crate::x11_state::paint()?;
    let parent = crate::x11_state::parent_snapshot();
    let screen = conn
        .setup()
        .roots
        .iter()
        .find(|s| s.root == host.root)
        .or_else(|| conn.setup().roots.first())?;
    Some(Snap {
        visual: paint.argb_visual,
        depth: paint.argb_depth,
        colormap: paint.colormap,
        root: host.root,
        parent_x: parent.origin_x,
        parent_y: parent.origin_y,
        scale: if parent.scale > 0.0 {
            parent.scale
        } else {
            1.0
        },
        root_w: screen.width_in_pixels as i32,
        root_h: screen.height_in_pixels as i32,
    })
}

fn grab_pointer(conn: &RustConnection, win: u32) -> bool {
    conn.grab_pointer(
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
    .and_then(|c| c.reply().ok())
    .is_some_and(|r| r.status == GrabStatus::SUCCESS)
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
