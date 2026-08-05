use std::collections::VecDeque;
use std::ffi::c_int;
use std::sync::Arc;
use std::thread::JoinHandle;

use jfn_mailbox::Mailbox;
use jfn_platform_abi::{
    Generation, MENU_DISMISSED, MenuClose, MenuHost, MenuItem, MenuMetrics, MenuPaint,
    MenuPlacement, MenuRequest, MenuSelection, PopupSurface, menu_has_selectable, menu_initial_row,
};
use parking_lot::Mutex;

use crate::menu::interaction_fsm::{self, MenuEffect, MenuEvent, MenuState as FsmState};
use crate::menu::render::{self, Fonts, Layout, blit_bgra};

const WHEEL_DETENT: f32 = 120.0;

/// A pointer position relative to the menu's top-left, in the unit the backend
/// delivers it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MenuPoint {
    /// Physical (buffer) pixels.
    Physical { x: f32, y: f32 },
    /// Logical (surface) pixels; converted with the ratio the surface was
    /// presented with.
    Logical { x: f32, y: f32 },
}

pub struct SoftwareMenu {
    emitter: Arc<Emitter>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl SoftwareMenu {
    pub fn spawn(surface: Arc<dyn PopupSurface>) -> SoftwareMenu {
        let emitter = Emitter::new(surface);
        let thread = {
            let emitter = Arc::clone(&emitter);
            match std::thread::Builder::new()
                .name("jfn-menu".into())
                .spawn(move || run(&emitter))
            {
                Ok(handle) => Some(handle),
                Err(e) => {
                    tracing::error!(target: "menu", "render thread spawn failed: {e}; menus disabled");
                    None
                }
            }
        };
        SoftwareMenu {
            emitter,
            thread: Mutex::new(thread),
        }
    }

    fn render_thread_alive(&self) -> bool {
        self.thread.lock().is_some()
    }

    /// `serial` must still be grab-worthy at the call. No-op when the render
    /// thread is absent.
    pub fn arm(&self, x: c_int, y: c_int, serial: u32) {
        if !self.render_thread_alive() {
            return;
        }
        self.emitter.update(|s| {
            let resolve = clear_menu(s).map(Resolve::dismissed);
            let generation = next_generation(s);
            s.generation = Some(generation);
            // The grab can activate at the popup's initial commit, and the
            // grab-induced focus loss must already observe `engaged`.
            s.engaged = true;
            s.phase = Phase::AwaitPlaceholder;
            queue(
                s,
                SurfaceOp::Create {
                    generation,
                    place: MenuPlacement {
                        x,
                        y,
                        lw: 1,
                        lh: 1,
                        pw: 1,
                        ph: 1,
                    },
                    serial,
                },
            );
            resolve
        });
    }

    pub fn dismiss_if_speculative(&self) {
        self.emitter.update(|s| {
            if s.menu.is_some() || s.phase == Phase::Idle {
                return None;
            }
            close_current(s, MenuClose::Speculative)
        });
    }

    pub fn on_ready(&self, generation: Generation) {
        self.emitter.update(|s| {
            if s.generation != Some(generation) {
                return None;
            }
            match s.phase {
                Phase::AwaitPlaceholder => {
                    if s.menu.as_ref().is_some_and(|m| m.layout.is_some()) {
                        begin_menu(s);
                    } else {
                        s.phase = Phase::Placeholder;
                    }
                }
                Phase::AwaitMenu => {
                    // The placement the `Create` (or `begin_menu`) carried still
                    // stands; only the pixels are missing.
                    s.phase = Phase::Shown;
                    request_paint(s);
                }
                Phase::Idle | Phase::Placeholder | Phase::Shown => {}
            }
            None
        });
    }

    pub fn on_done(&self, generation: Generation) {
        self.emitter.update(|s| {
            if s.generation != Some(generation) {
                return None;
            }
            close_current(s, MenuClose::External)
        });
    }

    /// Ignored unless a layout exists to hit-test against.
    pub fn motion(&self, at: MenuPoint) {
        self.pointer(at, false);
    }

    /// Ignored unless a layout exists to hit-test against.
    pub fn press(&self, at: MenuPoint) {
        self.pointer(at, true);
    }

    fn pointer(&self, at: MenuPoint, press: bool) {
        self.emitter.update(|s| {
            let (x, y) = s.menu.as_ref().and_then(|m| buffer_point(m, at))?;
            let ev = if press {
                MenuEvent::Press { x, y }
            } else {
                MenuEvent::Motion { x, y }
            };
            step(s, ev)
        });
    }

    /// Accepted whenever the menu is active, layout or not.
    pub fn key(&self, keysym: u32) {
        self.emitter.update(|s| {
            if !s.active {
                return None;
            }
            step(s, MenuEvent::Key(keysym))
        });
    }

    /// Accepted whenever the menu is active, layout or not.
    pub fn dismiss(&self) {
        self.emitter.update(|s| {
            if !s.active {
                return None;
            }
            step(s, MenuEvent::Dismiss)
        });
    }

    pub fn expose(&self) {
        self.emitter.update(|s| {
            if s.menu.as_ref().is_some_and(|m| m.layout.is_some()) {
                request_paint(s);
            }
            None
        });
    }

    /// ±120 per detent, positive = wheel up.
    pub fn scroll(&self, dy: c_int) {
        self.emitter.update(|s| {
            let menu = s.menu.as_mut().filter(|m| m.layout.is_some())?;
            if menu.view_ph >= menu.ph {
                return None;
            }
            let max = (menu.ph - menu.view_ph).max(0);
            let new = (menu.scroll - scroll_step(dy, row_height(menu))).clamp(0, max);
            if new == menu.scroll {
                return None;
            }
            menu.scroll = new;
            request_paint(s);
            None
        });
    }

    pub fn is_active(&self) -> bool {
        self.emitter.mailbox.peek(|s| s.active)
    }

    pub fn is_engaged(&self) -> bool {
        self.emitter.mailbox.peek(|s| s.engaged)
    }

    pub fn has_menu(&self) -> bool {
        self.emitter.mailbox.peek(|s| s.menu.is_some())
    }
}

impl MenuHost for SoftwareMenu {
    fn warm(&self) {}

    fn open(&self, req: MenuRequest) {
        if !self.render_thread_alive() || !menu_has_selectable(&req.items) {
            self.emitter
                .update(|s| close_current(s, MenuClose::Speculative));
            req.on_selected.resolve(MENU_DISMISSED);
            return;
        }
        self.emitter.update(|s| {
            let resolve = s
                .menu
                .as_mut()
                .and_then(|m| m.on_selected.take())
                .map(Resolve::dismissed);
            s.menu = Some(Menu {
                fsm: FsmState {
                    active: menu_initial_row(&req.items, req.initial),
                },
                items: Arc::new(req.items),
                layout: None,
                pw: 0,
                ph: 0,
                view_ph: 0,
                scroll: 0,
                metrics: MenuMetrics {
                    scale: 1.0,
                    clamp_ph: None,
                },
                width: req.width,
                on_selected: Some(req.on_selected),
                anchor: (req.x, req.y),
            });
            if s.phase == Phase::Idle {
                let generation = next_generation(s);
                s.generation = Some(generation);
                s.engaged = true;
            }
            s.job = Some(RenderJob::Shape);
            resolve
        });
    }

    fn hide(&self) {
        self.emitter.update(|s| {
            // A hide can be the tail of a previous cycle arriving after the
            // next press already armed a fresh popup.
            s.menu.as_ref()?;
            close_current(s, MenuClose::Finished)
        });
    }

    fn shutdown(&self) {
        self.emitter.update(|s| {
            s.shutdown = true;
            close_current(s, MenuClose::Finished)
        });
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
    }
}

/// The one ordered path from menu state to the surface: every op is queued
/// under the state lock and drained in FIFO order by one leader at a time.
struct Emitter {
    surface: Arc<dyn PopupSurface>,
    mailbox: Mailbox<MenuState>,
}

impl Emitter {
    fn new(surface: Arc<dyn PopupSurface>) -> Arc<Emitter> {
        Arc::new(Emitter {
            surface,
            mailbox: Mailbox::new(MenuState::default()),
        })
    }

    /// Runs `f` under the state lock, then flushes what it queued.
    fn update(&self, f: impl FnOnce(&mut MenuState) -> Option<Resolve>) {
        let resolve = self.mailbox.update(f);
        self.flush(resolve);
    }

    /// Drains [`MenuState::pending`] to the surface in issue order, then fires
    /// `resolve` with no lock held. A surface call that re-enters here queues
    /// and returns; the leader emits what it queued.
    fn flush(&self, resolve: Option<Resolve>) {
        let leader = self
            .mailbox
            .update(|s| !std::mem::replace(&mut s.draining, true));
        if leader {
            while let Some(op) = self.mailbox.update(|s| {
                let op = s.pending.pop_front();
                s.draining = op.is_some();
                op
            }) {
                op.emit(&*self.surface);
            }
        }
        if let Some(resolve) = resolve {
            resolve.fire();
        }
    }
}

/// A selection to settle once the state lock is released.
struct Resolve {
    selection: MenuSelection,
    id: c_int,
}

impl Resolve {
    fn dismissed(selection: MenuSelection) -> Resolve {
        Resolve {
            selection,
            id: MENU_DISMISSED,
        }
    }

    fn fire(self) {
        self.selection.resolve(self.id);
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    #[default]
    Idle,
    AwaitPlaceholder,
    Placeholder,
    AwaitMenu,
    Shown,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum RenderJob {
    Paint,
    Shape,
}

struct Menu {
    items: Arc<Vec<MenuItem>>,
    layout: Option<Arc<Layout>>,
    fsm: FsmState,
    pw: i32,
    /// Full content (buffer) height, physical px.
    ph: i32,
    /// Visible (clamped) height, physical px.
    view_ph: i32,
    /// Scroll offset, physical px, `0..=ph - view_ph`.
    scroll: i32,
    metrics: MenuMetrics,
    /// Desired logical width; `<= 0` is content-sized.
    width: i32,
    on_selected: Option<MenuSelection>,
    anchor: (i32, i32),
}

#[derive(Default)]
struct MenuState {
    phase: Phase,
    generation: Option<Generation>,
    next_generation: u64,
    active: bool,
    engaged: bool,
    menu: Option<Menu>,
    job: Option<RenderJob>,
    /// Surface ops in issue order, drained by [`Emitter::flush`]; independent of
    /// the session, so a queued teardown survives `clear_menu`.
    pending: VecDeque<SurfaceOp>,
    /// A flush owns `pending`; cleared only when the queue is observed empty
    /// under the same lock.
    draining: bool,
    shutdown: bool,
}

enum SurfaceOp {
    Create {
        generation: Generation,
        place: MenuPlacement,
        serial: u32,
    },
    Reposition {
        generation: Generation,
        place: MenuPlacement,
    },
    Present(MenuPaint),
    Destroy {
        generation: Generation,
        reason: MenuClose,
    },
}

impl SurfaceOp {
    fn emit(self, surface: &dyn PopupSurface) {
        match self {
            SurfaceOp::Create {
                generation,
                place,
                serial,
            } => surface.create(generation, place, serial),
            SurfaceOp::Reposition { generation, place } => surface.reposition(generation, place),
            SurfaceOp::Present(paint) => surface.present(paint),
            SurfaceOp::Destroy { generation, reason } => surface.destroy(generation, reason),
        }
    }
}

fn queue(state: &mut MenuState, op: SurfaceOp) {
    state.pending.push_back(op);
}

enum Job {
    Shape {
        generation: Generation,
        items: Arc<Vec<MenuItem>>,
        width: i32,
    },
    Paint {
        generation: Generation,
        items: Arc<Vec<MenuItem>>,
        layout: Arc<Layout>,
        active: i32,
    },
}

fn take_job(state: &mut MenuState) -> Option<Job> {
    let job = state.job.take()?;
    let generation = state.generation?;
    let menu = state.menu.as_ref()?;
    match job {
        RenderJob::Shape => Some(Job::Shape {
            generation,
            items: Arc::clone(&menu.items),
            width: menu.width,
        }),
        RenderJob::Paint => Some(Job::Paint {
            generation,
            items: Arc::clone(&menu.items),
            layout: Arc::clone(menu.layout.as_ref()?),
            active: menu.fsm.active,
        }),
    }
}

fn request_paint(state: &mut MenuState) {
    state.job = Some(
        state
            .job
            .map_or(RenderJob::Paint, |j| j.max(RenderJob::Paint)),
    );
}

fn placement(menu: &Menu) -> MenuPlacement {
    MenuPlacement {
        x: menu.anchor.0,
        y: menu.anchor.1,
        lw: logical_dim(menu.pw, menu.metrics.scale),
        lh: logical_dim(menu.view_ph, menu.metrics.scale),
        pw: menu.pw,
        ph: menu.view_ph,
    }
}

/// Buffer coordinates, physical px including the scroll offset. `None` before a
/// layout gives the menu a presented size.
///
/// Logical input divides by the presented `pw/lw` and `view_ph/lh` ratios, so
/// the conversion inverts exactly what the surface was given.
fn buffer_point(menu: &Menu, at: MenuPoint) -> Option<(i32, i32)> {
    menu.layout.as_ref()?;
    let (x, y) = match at {
        MenuPoint::Physical { x, y } => (x, y),
        MenuPoint::Logical { x, y } => (
            x * ratio(menu.pw, logical_dim(menu.pw, menu.metrics.scale)),
            y * ratio(menu.view_ph, logical_dim(menu.view_ph, menu.metrics.scale)),
        ),
    };
    Some((x as i32, y as i32 + menu.scroll))
}

fn ratio(physical: i32, logical: i32) -> f32 {
    if logical > 0 {
        physical as f32 / logical as f32
    } else {
        1.0
    }
}

fn row_height(menu: &Menu) -> i32 {
    menu.layout.as_ref().map_or(1, |l| {
        l.rows
            .iter()
            .find(|r| !r.separator)
            .map_or(1, |r| r.h.max(1))
    })
}

fn scroll_active_into_view(menu: &mut Menu) {
    if menu.view_ph >= menu.ph {
        return;
    }
    let Some(layout) = menu.layout.as_ref() else {
        return;
    };
    let Some(r) = layout
        .rows
        .iter()
        .find(|r| r.item as i32 == menu.fsm.active)
    else {
        return;
    };
    if r.y < menu.scroll {
        menu.scroll = r.y;
    } else if r.y + r.h > menu.scroll + menu.view_ph {
        menu.scroll = r.y + r.h - menu.view_ph;
    }
    menu.scroll = menu.scroll.clamp(0, (menu.ph - menu.view_ph).max(0));
}

fn on_layout(state: &mut MenuState, generation: Generation, layout: Layout, metrics: MenuMetrics) {
    if state.generation != Some(generation) {
        return;
    }
    let Some(menu) = state.menu.as_mut() else {
        return;
    };
    menu.metrics = metrics;
    menu.pw = layout.width;
    menu.ph = layout.height;
    menu.layout = Some(Arc::new(layout));
    let anchor_ph_y = (menu.anchor.1 as f32 * metrics.scale).round() as i32;
    menu.view_ph = view_ph(
        menu.ph,
        row_height(menu),
        menu.width,
        metrics.clamp_ph,
        anchor_ph_y,
    );
    menu.scroll = 0;
    scroll_active_into_view(menu);
    match state.phase {
        Phase::Placeholder => begin_menu(state),
        Phase::Idle => {
            state.active = true;
            state.engaged = true;
            state.phase = Phase::AwaitMenu;
            if let Some(place) = state.menu.as_ref().map(placement) {
                queue(
                    state,
                    SurfaceOp::Create {
                        generation,
                        place,
                        // 0: no triggering press; the surface substitutes
                        // whatever serial it still has.
                        serial: 0,
                    },
                );
            }
        }
        Phase::Shown => {
            if let Some(place) = state.menu.as_ref().map(placement) {
                queue(state, SurfaceOp::Reposition { generation, place });
            }
            request_paint(state);
        }
        Phase::AwaitPlaceholder | Phase::AwaitMenu => {}
    }
}

fn on_pixels(state: &mut MenuState, generation: Generation, pixels: Vec<u8>) {
    if state.generation != Some(generation) {
        return;
    }
    let Some(menu) = state.menu.as_ref() else {
        return;
    };
    let paint = MenuPaint {
        generation,
        pixels,
        pw: menu.pw,
        ph: menu.ph,
        scroll: menu.scroll,
        view_ph: menu.view_ph,
        lw: logical_dim(menu.pw, menu.metrics.scale),
        lh: logical_dim(menu.view_ph, menu.metrics.scale),
    };
    queue(state, SurfaceOp::Present(paint));
}

fn begin_menu(state: &mut MenuState) {
    let Some(generation) = state.generation else {
        return;
    };
    let Some(menu) = state.menu.as_ref() else {
        return;
    };
    let place = placement(menu);
    state.active = true;
    state.engaged = true;
    state.phase = Phase::AwaitMenu;
    // Maps the popup invisibly, activating the grab before the menu has pixels.
    queue(
        state,
        SurfaceOp::Present(MenuPaint {
            generation,
            pixels: vec![0u8; 4],
            pw: 1,
            ph: 1,
            scroll: 0,
            view_ph: 1,
            lw: 1,
            lh: 1,
        }),
    );
    queue(state, SurfaceOp::Reposition { generation, place });
}

fn step(state: &mut MenuState, ev: MenuEvent) -> Option<Resolve> {
    let menu = state.menu.as_mut()?;
    let layout = menu.layout.clone();
    let items = Arc::clone(&menu.items);
    let effects = interaction_fsm::step(&mut menu.fsm, &ev, layout.as_deref(), &items);
    if matches!(ev, MenuEvent::Key(_)) {
        scroll_active_into_view(menu);
    }
    for effect in effects {
        match effect {
            MenuEffect::Redraw => request_paint(state),
            MenuEffect::Close(id) => {
                let generation = state.generation;
                let resolve = clear_menu(state).map(|selection| Resolve { selection, id });
                if let Some(generation) = generation {
                    queue(
                        state,
                        SurfaceOp::Destroy {
                            generation,
                            reason: MenuClose::Finished,
                        },
                    );
                }
                return resolve;
            }
        }
    }
    None
}

/// Clears the session, queues the surface teardown and returns the pending
/// selection, resolved as [`MENU_DISMISSED`].
fn close_current(state: &mut MenuState, reason: MenuClose) -> Option<Resolve> {
    let generation = state.generation;
    let resolve = clear_menu(state).map(Resolve::dismissed);
    if let Some(generation) = generation {
        queue(state, SurfaceOp::Destroy { generation, reason });
    }
    resolve
}

fn clear_menu(state: &mut MenuState) -> Option<MenuSelection> {
    state.active = false;
    state.engaged = false;
    state.phase = Phase::Idle;
    state.generation = None;
    state.job = None;
    state.menu.take().and_then(|mut m| m.on_selected.take())
}

fn next_generation(state: &mut MenuState) -> Generation {
    let v = state.next_generation.wrapping_add(1);
    state.next_generation = v;
    Generation::new(v).unwrap_or(Generation::MIN)
}

fn logical_dim(physical: i32, scale: f32) -> i32 {
    if scale > 0.0 {
        ((physical as f32 / scale).round() as i32).max(1)
    } else {
        physical.max(1)
    }
}

fn view_ph(ph: i32, row_h: i32, width: i32, clamp_ph: Option<i32>, anchor_ph_y: i32) -> i32 {
    let (true, Some(clamp_ph)) = (width > 0, clamp_ph) else {
        return ph;
    };
    ph.min((clamp_ph - anchor_ph_y).max(row_h))
}

fn scroll_step(dy: i32, row_h: i32) -> i32 {
    (dy as f32 / WHEEL_DETENT * row_h as f32).round() as i32
}

fn run(emitter: &Emitter) {
    let mut fonts = Fonts::new();
    loop {
        let (job, shutdown) = emitter.mailbox.wait(
            |s| s.job.is_some() || s.shutdown,
            |s| (take_job(s), s.shutdown),
        );
        if shutdown {
            return;
        }
        let Some(job) = job else { continue };
        match job {
            Job::Shape {
                generation,
                items,
                width,
            } => {
                let metrics = emitter.surface.metrics();
                let mut layout = render::layout(&mut fonts, &items, metrics.scale);
                if width > 0 {
                    layout.width = ((width as f32 * metrics.scale).round() as i32).max(1);
                }
                emitter.update(|s| {
                    on_layout(s, generation, layout, metrics);
                    None
                });
            }
            Job::Paint {
                generation,
                items,
                layout,
                active,
            } => {
                let Some(pm) = render::paint(&mut fonts, &layout, &items, active) else {
                    continue;
                };
                let mut pixels = vec![0u8; (pm.width() as usize) * (pm.height() as usize) * 4];
                blit_bgra(&pm, &mut pixels);
                emitter.update(|s| {
                    on_pixels(s, generation, pixels);
                    None
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;
    use std::sync::mpsc::{Receiver, Sender, channel};

    use super::*;

    struct NoopSurface;

    impl PopupSurface for NoopSurface {
        fn metrics(&self) -> MenuMetrics {
            MenuMetrics {
                scale: 1.0,
                clamp_ph: None,
            }
        }
        fn create(&self, _generation: Generation, _place: MenuPlacement, _serial: u32) {}
        fn reposition(&self, _generation: Generation, _place: MenuPlacement) {}
        fn present(&self, _paint: MenuPaint) {}
        fn destroy(&self, _generation: Generation, _reason: MenuClose) {}
    }

    #[derive(Default)]
    struct RecordingSurface {
        seen: Mutex<Vec<&'static str>>,
    }

    impl PopupSurface for RecordingSurface {
        fn metrics(&self) -> MenuMetrics {
            MenuMetrics {
                scale: 1.0,
                clamp_ph: None,
            }
        }
        fn create(&self, _generation: Generation, _place: MenuPlacement, _serial: u32) {
            self.seen.lock().push("create");
        }
        fn reposition(&self, _generation: Generation, _place: MenuPlacement) {
            self.seen.lock().push("reposition");
        }
        fn present(&self, _paint: MenuPaint) {
            self.seen.lock().push("present");
        }
        fn destroy(&self, _generation: Generation, _reason: MenuClose) {
            self.seen.lock().push("destroy");
        }
    }

    /// Queues a `Destroy` from inside `create`, i.e. while the leader is
    /// draining.
    #[derive(Default)]
    struct ReentrantSurface {
        seen: Mutex<Vec<&'static str>>,
        emitter: OnceLock<Arc<Emitter>>,
    }

    impl PopupSurface for ReentrantSurface {
        fn metrics(&self) -> MenuMetrics {
            MenuMetrics {
                scale: 1.0,
                clamp_ph: None,
            }
        }
        fn create(&self, generation: Generation, _place: MenuPlacement, _serial: u32) {
            self.seen.lock().push("create");
            if let Some(emitter) = self.emitter.get() {
                emitter.update(|s| {
                    queue(
                        s,
                        SurfaceOp::Destroy {
                            generation,
                            reason: MenuClose::Finished,
                        },
                    );
                    None
                });
            }
        }
        fn reposition(&self, _generation: Generation, _place: MenuPlacement) {
            self.seen.lock().push("reposition");
        }
        fn present(&self, _paint: MenuPaint) {
            self.seen.lock().push("present");
        }
        fn destroy(&self, _generation: Generation, _reason: MenuClose) {
            self.seen.lock().push("destroy");
        }
    }

    fn menu_on(surface: Arc<dyn PopupSurface>, alive: bool) -> SoftwareMenu {
        let thread = alive.then(|| std::thread::spawn(|| {}));
        SoftwareMenu {
            emitter: Emitter::new(surface),
            thread: Mutex::new(thread),
        }
    }

    fn menu_with_thread(alive: bool) -> SoftwareMenu {
        menu_on(Arc::new(NoopSurface), alive)
    }

    fn request_row(items: Vec<MenuItem>, initial: c_int) -> (MenuRequest, Receiver<c_int>) {
        let (tx, rx): (Sender<c_int>, Receiver<c_int>) = channel();
        let req = MenuRequest {
            items,
            x: 0,
            y: 0,
            width: 0,
            initial,
            on_selected: MenuSelection::new(move |id| {
                let _ = tx.send(id);
            }),
        };
        (req, rx)
    }

    fn request(items: Vec<MenuItem>) -> (MenuRequest, Receiver<c_int>) {
        request_row(items, MENU_DISMISSED)
    }

    fn selectable_item() -> MenuItem {
        MenuItem {
            id: 1,
            label: "One".into(),
            enabled: true,
            separator: false,
        }
    }

    /// Marks the session live the way a delivered layout would, without a
    /// render thread.
    fn force_active(menu: &SoftwareMenu) {
        menu.emitter.update(|s| {
            s.active = true;
            None
        });
    }

    /// Feeds a layout the way the render thread's `Shape` job would.
    fn deliver_layout(menu: &SoftwareMenu) {
        menu.emitter.update(|s| {
            if let Some(generation) = s.generation {
                on_layout(
                    s,
                    generation,
                    Layout::for_test(100, 40, Vec::new(), Vec::new()),
                    MenuMetrics {
                        scale: 1.0,
                        clamp_ph: None,
                    },
                );
            }
            None
        });
    }

    /// Acknowledges the popup the way the compositor's first configure would.
    fn deliver_ready(menu: &SoftwareMenu) {
        if let Some(generation) = menu.emitter.mailbox.peek(|s| s.generation) {
            menu.on_ready(generation);
        }
    }

    #[test]
    fn a_keyboard_opened_menu_is_created_and_never_repositioned_before_its_pixels() {
        let surface = Arc::new(RecordingSurface::default());
        let menu = menu_on(Arc::clone(&surface) as Arc<dyn PopupSurface>, true);
        let (req, _rx) = request(vec![selectable_item()]);
        menu.open(req);
        deliver_layout(&menu);
        deliver_ready(&menu);
        assert_eq!(*surface.seen.lock(), vec!["create"]);
    }

    #[test]
    fn a_relayout_while_shown_repositions_the_popup() {
        let surface = Arc::new(RecordingSurface::default());
        let menu = menu_on(Arc::clone(&surface) as Arc<dyn PopupSurface>, true);
        let (req, _rx) = request(vec![selectable_item()]);
        menu.open(req);
        deliver_layout(&menu);
        deliver_ready(&menu);
        deliver_layout(&menu);
        assert_eq!(*surface.seen.lock(), vec!["create", "reposition"]);
    }

    #[test]
    fn hide_resolves_the_pending_selection_as_dismissed() {
        let menu = menu_with_thread(true);
        let (req, rx) = request(vec![selectable_item()]);
        menu.open(req);
        assert!(rx.try_recv().is_err());
        menu.hide();
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
    }

    #[test]
    fn a_menu_with_no_selectable_item_is_refused_and_resolved() {
        let menu = menu_with_thread(true);
        let (req, rx) = request(vec![MenuItem {
            id: 0,
            label: String::new(),
            enabled: false,
            separator: true,
        }]);
        menu.open(req);
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
        assert!(!menu.has_menu());
    }

    #[test]
    fn open_without_a_render_thread_resolves_and_leaves_the_state_idle() {
        let menu = menu_with_thread(false);
        let (req, rx) = request(vec![selectable_item()]);
        menu.open(req);
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
        assert!(!menu.has_menu());
        assert!(!menu.is_engaged());
    }

    #[test]
    fn a_queued_create_reaches_the_surface_before_a_later_destroy() {
        let surface = Arc::new(RecordingSurface::default());
        let menu = menu_on(Arc::clone(&surface) as Arc<dyn PopupSurface>, true);
        menu.arm(0, 0, 1);
        menu.dismiss_if_speculative();
        assert_eq!(*surface.seen.lock(), vec!["create", "destroy"]);
    }

    #[test]
    fn a_surface_call_that_re_enters_the_menu_keeps_op_order() {
        let surface = Arc::new(ReentrantSurface::default());
        let menu = menu_on(Arc::clone(&surface) as Arc<dyn PopupSurface>, true);
        let _ = surface.emitter.set(Arc::clone(&menu.emitter));
        menu.arm(0, 0, 1);
        assert_eq!(*surface.seen.lock(), vec!["create", "destroy"]);
    }

    #[test]
    fn an_active_menu_without_pixels_still_dismisses() {
        let menu = menu_with_thread(true);
        let (req, rx) = request(vec![selectable_item()]);
        menu.open(req);
        force_active(&menu);
        menu.dismiss();
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
        assert!(!menu.has_menu());
    }

    #[test]
    fn reopening_a_shown_menu_leaves_it_dismissable() {
        let menu = menu_with_thread(true);
        let (first, first_rx) = request(vec![selectable_item()]);
        menu.open(first);
        force_active(&menu);
        let (second, second_rx) = request(vec![selectable_item()]);
        menu.open(second);
        assert_eq!(first_rx.try_recv(), Ok(MENU_DISMISSED));
        assert!(second_rx.try_recv().is_err());
        menu.dismiss();
        assert_eq!(second_rx.try_recv(), Ok(MENU_DISMISSED));
    }

    #[test]
    fn an_out_of_range_initial_row_highlights_nothing() {
        let menu = menu_with_thread(true);
        let (req, _rx) = request_row(vec![selectable_item()], 5);
        menu.open(req);
        assert_eq!(
            menu.emitter
                .mailbox
                .peek(|s| s.menu.as_ref().map(|m| m.fsm.active)),
            Some(MENU_DISMISSED)
        );
    }

    #[test]
    fn logical_and_physical_points_land_on_the_same_buffer_pixel() {
        let mut menu = Menu {
            items: Arc::new(vec![selectable_item()]),
            layout: Some(Arc::new(Layout::for_test(150, 90, Vec::new(), Vec::new()))),
            fsm: FsmState::default(),
            pw: 150,
            ph: 90,
            view_ph: 90,
            scroll: 0,
            metrics: MenuMetrics {
                scale: 1.5,
                clamp_ph: None,
            },
            width: 0,
            on_selected: None,
            anchor: (0, 0),
        };
        assert_eq!(logical_dim(menu.pw, menu.metrics.scale), 100);
        assert_eq!(
            buffer_point(&menu, MenuPoint::Logical { x: 50.0, y: 30.0 }),
            buffer_point(&menu, MenuPoint::Physical { x: 75.0, y: 45.0 })
        );
        menu.layout = None;
        assert_eq!(
            buffer_point(&menu, MenuPoint::Physical { x: 1.0, y: 1.0 }),
            None
        );
    }

    #[test]
    fn shape_supersedes_a_queued_paint() {
        let mut s = MenuState::default();
        request_paint(&mut s);
        s.job = Some(s.job.map_or(RenderJob::Shape, |j| j.max(RenderJob::Shape)));
        assert_eq!(s.job, Some(RenderJob::Shape));
        request_paint(&mut s);
        assert_eq!(s.job, Some(RenderJob::Shape));
    }

    #[test]
    fn content_sized_menus_are_never_clamped() {
        assert_eq!(view_ph(500, 20, 0, Some(100), 0), 500);
        assert_eq!(view_ph(500, 20, 120, None, 0), 500);
    }

    #[test]
    fn width_constrained_menu_clamps_to_the_window_bottom() {
        assert_eq!(view_ph(500, 20, 120, Some(400), 100), 300);
        assert_eq!(view_ph(200, 20, 120, Some(400), 100), 200);
    }

    #[test]
    fn a_bottom_anchor_keeps_one_row() {
        assert_eq!(view_ph(500, 20, 120, Some(400), 400), 20);
        assert_eq!(view_ph(500, 20, 120, Some(400), 900), 20);
    }

    #[test]
    fn generations_start_at_one_and_never_hit_zero() {
        let mut s = MenuState::default();
        assert_eq!(next_generation(&mut s).get(), 1);
        assert_eq!(next_generation(&mut s).get(), 2);
        s.next_generation = u64::MAX;
        assert_eq!(next_generation(&mut s).get(), Generation::MIN.get());
    }

    #[test]
    fn logical_dim_never_collapses_to_zero() {
        assert_eq!(logical_dim(100, 2.0), 50);
        assert_eq!(logical_dim(1, 4.0), 1);
        assert_eq!(logical_dim(7, 0.0), 7);
    }

    #[test]
    fn one_detent_scrolls_one_row() {
        assert_eq!(scroll_step(120, 28), 28);
        assert_eq!(scroll_step(-120, 28), -28);
        assert_eq!(scroll_step(0, 28), 0);
    }
}
