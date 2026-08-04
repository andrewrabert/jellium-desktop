mod lifecycle_fsm;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use calloop::channel::{Channel, Sender};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, LoopSignal};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::shm::ConnectionExt as ShmConnectionExt;
use x11rb::protocol::xproto::{
    ConnectionExt as XprotoConnectionExt, CreateGCAux, CreateWindowAux, EventMask, GrabMode,
    GrabStatus, ImageFormat, StackMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use crate::conn_source::X11Source;
use crate::shm::{shm_alloc, shm_free};
use crate::x11_state::ShmBuffer;
use jfn_menu::interaction_fsm::{self, MenuEffect, MenuEvent, MenuState};
use jfn_menu::render::{self, Fonts, Layout};
use lifecycle_fsm::{Life, LifeEffect, LifeEvent};

pub use jfn_menu::MenuItem;

const GRAB_RETRY: Duration = Duration::from_millis(5);
const GRAB_ATTEMPTS: u32 = 40;

pub struct MenuRequest {
    /// CEF view (logical, unscaled) coordinates of the click.
    pub x: i32,
    pub y: i32,
    pub items: Vec<MenuItem>,
    pub on_selected: Option<Box<dyn FnOnce(i32) + Send>>,
}

static SENDER: OnceLock<Option<Sender<MenuRequest>>> = OnceLock::new();

pub fn show(req: MenuRequest) {
    tracing::debug!(
        target: "x11::menu",
        "show: {} items at view=({},{})",
        req.items.len(),
        req.x,
        req.y,
    );
    match SENDER.get_or_init(spawn_worker) {
        Some(tx) => {
            if let Err(e) = tx.send(req) {
                tracing::error!(target: "x11::menu", "show: worker gone; dismissing");
                fire(e.0.on_selected, -1);
            }
        }
        None => {
            tracing::error!(target: "x11::menu", "show: no worker thread; dismissing");
            fire(req.on_selected, -1);
        }
    }
}

fn fire(cb: Option<Box<dyn FnOnce(i32) + Send>>, result: i32) {
    if let Some(cb) = cb {
        cb(result);
    }
}

fn spawn_worker() -> Option<Sender<MenuRequest>> {
    let (tx, rx) = calloop::channel::channel::<MenuRequest>();
    std::thread::Builder::new()
        .name("jfn-x11-menu".into())
        .spawn(move || worker(rx))
        .ok()?;
    Some(tx)
}

fn worker(rx: Channel<MenuRequest>) {
    let Ok((conn, _screen)) = x11rb::connect(None) else {
        tracing::error!(target: "x11::menu", "worker: X11 connect failed; menus disabled");
        drain(rx);
        return;
    };
    let conn = Arc::new(conn);
    let Ok(mut event_loop) = EventLoop::<'static, MenuLoop>::try_new() else {
        tracing::error!(target: "x11::menu", "worker: calloop init failed; menus disabled");
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
        tracing::error!(target: "x11::menu", "worker: event source setup failed; menus disabled");
        return;
    }
    let mut state = MenuLoop {
        keymap: Keymap::query(&conn),
        conn,
        fonts: Fonts::new(),
        phase: Phase::Idle,
        handle,
        signal: event_loop.get_signal(),
    };
    tracing::debug!(target: "x11::menu", "worker: started");
    if let Err(e) = event_loop.run(None, &mut state, |_| {}) {
        tracing::error!(target: "x11::menu", "worker: loop error: {e}");
    }
    state.close(-1);
}

fn drain(rx: Channel<MenuRequest>) {
    while let Ok(req) = rx.recv() {
        fire(req.on_selected, -1);
    }
}

enum Phase {
    Idle,
    Grabbing { open: Open, attempts: u32 },
    Open(Open),
}

struct Open {
    win: u32,
    gc: u32,
    buf: ShmBuffer,
    layout: Layout,
    items: Vec<MenuItem>,
    life: Life,
    state: MenuState,
    on_selected: Option<Box<dyn FnOnce(i32) + Send>>,
}

struct MenuLoop {
    conn: Arc<RustConnection>,
    keymap: Keymap,
    fonts: Fonts,
    phase: Phase,
    handle: LoopHandle<'static, MenuLoop>,
    signal: LoopSignal,
}

impl MenuLoop {
    fn on_channel(&mut self, event: calloop::channel::Event<MenuRequest>) {
        match event {
            calloop::channel::Event::Msg(req) => self.on_request(req),
            calloop::channel::Event::Closed => {
                self.close(-1);
                self.signal.stop();
            }
        }
    }

    fn on_request(&mut self, req: MenuRequest) {
        if !matches!(self.phase, Phase::Idle) {
            tracing::debug!(target: "x11::menu", "on_request: menu already open; dismissing");
            fire(req.on_selected, -1);
            return;
        }
        let MenuRequest {
            x,
            y,
            items,
            on_selected,
        } = req;
        let mut life = Life::default();
        let _ = lifecycle_fsm::step(&mut life, &LifeEvent::Show);

        let Some((win, gc, layout)) = self.build(x, y, &items) else {
            fire(on_selected, end(&mut life, LifeEvent::BuildFail));
            return;
        };
        let mut open = Open {
            win,
            gc,
            buf: ShmBuffer::empty(),
            layout,
            items,
            life,
            state: MenuState::default(),
            on_selected,
        };
        redraw(
            &self.conn,
            open.win,
            open.gc,
            &mut open.buf,
            &mut self.fonts,
            &open.layout,
            &open.items,
            -1,
        );
        let _ = lifecycle_fsm::step(&mut open.life, &LifeEvent::BuildOk);
        self.phase = Phase::Grabbing { open, attempts: 0 };
        if self
            .handle
            .insert_source(Timer::from_duration(GRAB_RETRY), |_, _, st| {
                st.on_grab_retry()
            })
            .is_err()
        {
            tracing::error!(target: "x11::menu", "on_request: grab timer failed; dismissing");
            self.close(-1);
        }
    }

    /// `None` means the menu cannot be shown; nothing is left behind on the
    /// server for the caller to clean up.
    fn build(&mut self, cx: i32, cy: i32, items: &[MenuItem]) -> Option<(u32, u32, Layout)> {
        let snap = snapshot(&self.conn).or_else(|| {
            tracing::warn!(target: "x11::menu", "build: no X11 state snapshot; dismissing");
            None
        })?;
        let layout = render::layout(&mut self.fonts, items, snap.scale);
        if layout.selectable.is_empty() {
            tracing::debug!(target: "x11::menu", "build: no selectable rows; dismissing");
            return None;
        }
        let (wx, wy) = place(&snap, cx, cy, layout.width, layout.height);
        tracing::debug!(
            target: "x11::menu",
            "build: scale={:.2} parent=({},{}) root={}x{} menu={}x{} at=({},{})",
            snap.scale, snap.parent_x, snap.parent_y, snap.root_w, snap.root_h,
            layout.width, layout.height, wx, wy,
        );

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
                layout.width as u16,
                layout.height as u16,
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
        let _ = self.conn.configure_window(
            win,
            &x11rb::protocol::xproto::ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        );
        // Round-trip on the grabbing connection before grabbing — the window
        // must be realized server-side or the grab races into a BadWindow.
        let _ = self
            .conn
            .get_geometry(win)
            .ok()
            .and_then(|c| c.reply().ok());
        tracing::debug!(target: "x11::menu", "build: window 0x{win:x} created+mapped");
        Some((win, gc, layout))
    }

    fn on_grab_retry(&mut self) -> TimeoutAction {
        let Phase::Grabbing { open, attempts } = &mut self.phase else {
            return TimeoutAction::Drop;
        };
        if grab_pointer(&self.conn, open.win) {
            let _ = self.conn.grab_keyboard(
                false,
                open.win,
                x11rb::CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            );
            let _ = lifecycle_fsm::step(&mut open.life, &LifeEvent::GrabOk);
            let Phase::Grabbing { open, .. } = std::mem::replace(&mut self.phase, Phase::Idle)
            else {
                return TimeoutAction::Drop;
            };
            tracing::debug!(target: "x11::menu", "on_grab_retry: grabbed; menu is modal");
            self.phase = Phase::Open(open);
            return TimeoutAction::Drop;
        }
        *attempts += 1;
        if *attempts >= GRAB_ATTEMPTS {
            tracing::error!(target: "x11::menu", "on_grab_retry: pointer grab failed; dismissing");
            self.close(-1);
            return TimeoutAction::Drop;
        }
        TimeoutAction::ToDuration(GRAB_RETRY)
    }

    fn on_event(&mut self, ev: Event) {
        let Phase::Open(open) = &mut self.phase else {
            return;
        };
        let Some(mev) = translate(&self.keymap, &ev) else {
            return;
        };
        let mut result = None;
        for effect in interaction_fsm::step(&mut open.state, &mev, &open.layout, &open.items) {
            match effect {
                MenuEffect::Redraw => redraw(
                    &self.conn,
                    open.win,
                    open.gc,
                    &mut open.buf,
                    &mut self.fonts,
                    &open.layout,
                    &open.items,
                    open.state.active,
                ),
                MenuEffect::Close(id) => {
                    result = Some(id);
                    break;
                }
            }
        }
        if let Some(id) = result {
            self.close(id);
        }
    }

    fn close(&mut self, result: i32) {
        let (mut open, ev) = match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Idle => return,
            Phase::Grabbing { open, .. } => (open, LifeEvent::GrabFail),
            Phase::Open(open) => (open, LifeEvent::Result(result)),
        };
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let id = end(&mut open.life, ev);
        shm_free(&mut open.buf, Some(&*self.conn));
        let _ = self.conn.free_gc(open.gc);
        let _ = self.conn.destroy_window(open.win);
        let _ = self.conn.flush();
        tracing::debug!(target: "x11::menu", "close: id={id}");
        fire(open.on_selected.take(), id);
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

fn place(snap: &Snap, cx: i32, cy: i32, w: i32, h: i32) -> (i32, i32) {
    let mut x = snap.parent_x + (cx as f32 * snap.scale).round() as i32;
    let mut y = snap.parent_y + (cy as f32 * snap.scale).round() as i32;
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

fn end(life: &mut Life, ev: LifeEvent) -> i32 {
    lifecycle_fsm::step(life, &ev)
        .into_iter()
        .find_map(|e| match e {
            LifeEffect::Fire(id) => Some(id),
            _ => None,
        })
        .unwrap_or(-1)
}

fn translate(keymap: &Keymap, ev: &Event) -> Option<MenuEvent> {
    match ev {
        Event::Expose(_) => Some(MenuEvent::Expose),
        Event::MotionNotify(e) => Some(MenuEvent::Motion {
            x: e.event_x as i32,
            y: e.event_y as i32,
        }),
        Event::ButtonPress(e) => Some(MenuEvent::Press {
            x: e.event_x as i32,
            y: e.event_y as i32,
        }),
        Event::KeyPress(e) => Some(MenuEvent::Key(keymap.lookup(e.detail))),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn redraw(
    conn: &RustConnection,
    win: u32,
    gc: u32,
    buf: &mut ShmBuffer,
    fonts: &mut Fonts,
    layout: &Layout,
    items: &[MenuItem],
    active: i32,
) {
    let Some(pm) = render::paint(fonts, layout, items, active) else {
        return;
    };
    let w = layout.width;
    let h = layout.height;
    if !shm_alloc(buf, conn, w, h) {
        return;
    }
    // tiny-skia is premultiplied RGBA; the ARGB32 X visual wants premultiplied
    // BGRA, so swap R and B as we copy into the SHM segment.
    let src = pm.data();
    for (dst, src) in buf
        .pixels_mut()
        .chunks_exact_mut(4)
        .zip(src.chunks_exact(4))
    {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = src[3];
    }
    let _ = conn.shm_put_image(
        win,
        gc,
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
        buf.seg(),
        0,
    );
    let _ = conn.flush();
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
