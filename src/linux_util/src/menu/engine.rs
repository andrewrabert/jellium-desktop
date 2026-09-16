use std::collections::VecDeque;
use std::ffi::c_int;
use std::sync::Arc;
use std::thread::JoinHandle;

use jfn_mailbox::Mailbox;
use jfn_platform_abi::{
    Generation, LogicalPoint, MENU_DISMISSED, MenuClose, MenuHost, MenuItem, MenuMetrics,
    MenuPaint, MenuPlacement, MenuRequest, MenuSelection, PhysicalSize, PopupSurface, WindowExtent,
    menu_has_selectable, menu_initial_row,
};
use parking_lot::Mutex;

use crate::menu::interaction_fsm::{self, MenuEffect, MenuEvent, MenuState as FsmState};
use crate::menu::render::{self, Fonts, Layout, blit_bgra};

const WHEEL_DETENT: f32 = 120.0;

mod configured {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(super) struct Configured(());

    pub(super) fn configured() -> Configured {
        Configured(())
    }
}
use configured::Configured;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MenuPoint {
    Physical { x: c_int, y: c_int },
    Logical { x: c_int, y: c_int },
}

pub enum MenuInputEvent {
    Pointer { at: MenuPoint, press: bool },
    Key(u32),
    Scroll(c_int),
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

    pub fn open_triggered(&self, req: MenuRequest, serial: u32) {
        self.open_impl(req, Some(serial));
    }

    fn open_impl(&self, req: MenuRequest, trigger: Option<u32>) {
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
            if let Some(serial) = trigger {
                s.active = false;
                if let Some(generation) = s.generation {
                    queue(
                        s,
                        SurfaceOp::Destroy {
                            generation,
                            reason: MenuClose::Finished,
                        },
                    );
                }
                let generation = next_generation(s);
                s.generation = Some(generation);
                s.phase = Phase::AwaitArmed;
                queue(
                    s,
                    SurfaceOp::Arm {
                        generation,
                        anchor: LogicalPoint { x: req.x, y: req.y },
                        serial,
                    },
                );
            }
            s.menu = Some(Menu {
                fsm: FsmState {
                    active: menu_initial_row(&req.items, req.initial),
                },
                items: Arc::new(req.items),
                laid: None,
                width: req.width,
                on_selected: Some(req.on_selected),
                anchor: LogicalPoint { x: req.x, y: req.y },
            });
            if s.phase == Phase::Idle {
                let generation = next_generation(s);
                s.generation = Some(generation);
            }
            s.job = Some(RenderJob::Shape);
            resolve
        });
    }

    fn render_thread_alive(&self) -> bool {
        self.thread.lock().is_some()
    }

    pub fn arm(&self, x: c_int, y: c_int, serial: u32) {
        if !self.render_thread_alive() {
            return;
        }
        self.emitter.update(|s| {
            let resolve = clear_menu(s).map(Resolve::dismissed);
            let generation = next_generation(s);
            s.generation = Some(generation);
            s.phase = Phase::AwaitArmed;
            queue(
                s,
                SurfaceOp::Arm {
                    generation,
                    anchor: LogicalPoint { x, y },
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
                Phase::AwaitArmed => {
                    let configured = configured::configured();
                    if s.menu.as_ref().is_some_and(|m| m.laid.is_some()) {
                        return begin_menu(s, configured);
                    }
                    s.phase = Phase::Armed(configured);
                }
                Phase::AwaitMenu => {
                    s.phase = Phase::Shown;
                    request_paint(s);
                }
                Phase::Idle | Phase::Armed(_) | Phase::Shown => {}
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

    pub fn motion(&self, at: MenuPoint) {
        self.pointer(at, false);
    }

    pub fn press(&self, at: MenuPoint) {
        self.pointer(at, true);
    }

    fn pointer(&self, at: MenuPoint, press: bool) {
        self.input_impl(None, MenuInputEvent::Pointer { at, press });
    }

    pub fn key(&self, keysym: u32) {
        self.input_impl(None, MenuInputEvent::Key(keysym));
    }

    pub fn input(&self, generation: Generation, event: MenuInputEvent) {
        self.input_impl(Some(generation), event);
    }

    fn input_impl(&self, generation: Option<Generation>, event: MenuInputEvent) {
        self.emitter.update(|s| {
            if generation.is_some_and(|generation| s.generation != Some(generation)) {
                return None;
            }
            match event {
                MenuInputEvent::Pointer { at, press } => {
                    let (x, y) = s.menu.as_ref().and_then(|m| buffer_point(m, at))?;
                    step(
                        s,
                        if press {
                            MenuEvent::Press { x, y }
                        } else {
                            MenuEvent::Motion { x, y }
                        },
                    )
                }
                MenuInputEvent::Key(keysym) => {
                    if !s.active {
                        return None;
                    }
                    step(s, MenuEvent::Key(keysym))
                }
                MenuInputEvent::Scroll(dy) => {
                    let laid = s.menu.as_mut()?.laid.as_mut()?;
                    if laid.view_ph >= laid.content.h {
                        return None;
                    }
                    let max = (laid.content.h - laid.view_ph).max(0);
                    let new = (laid.scroll - scroll_step(dy, row_height(laid))).clamp(0, max);
                    if new == laid.scroll {
                        return None;
                    }
                    laid.scroll = new;
                    request_paint(s);
                    None
                }
            }
        });
    }

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
            if s.menu.as_ref().is_some_and(|m| m.laid.is_some()) {
                request_paint(s);
            }
            None
        });
    }

    pub fn scroll(&self, dy: c_int) {
        self.input_impl(None, MenuInputEvent::Scroll(dy));
    }

    pub fn is_active(&self) -> bool {
        self.emitter.mailbox.peek(|s| s.active)
    }

    pub fn is_engaged(&self) -> bool {
        self.emitter.mailbox.peek(|s| s.phase != Phase::Idle)
    }

    pub fn has_menu(&self) -> bool {
        self.emitter.mailbox.peek(|s| s.menu.is_some())
    }
}

impl MenuHost for SoftwareMenu {
    fn warm(&self) {}

    fn open(&self, req: MenuRequest) {
        self.open_impl(req, None);
    }

    fn hide(&self) {
        self.emitter.update(|s| {
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

    fn update(&self, f: impl FnOnce(&mut MenuState) -> Option<Resolve>) {
        let resolve = self.mailbox.update(f);
        self.flush(resolve);
    }

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
    AwaitArmed,
    Armed(Configured),
    AwaitMenu,
    Shown,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum RenderJob {
    Paint,
    Shape,
}

struct Laid {
    layout: Arc<Layout>,
    metrics: MenuMetrics,
    content: PhysicalSize,
    view_ph: c_int,
    scroll: c_int,
}

struct Menu {
    items: Arc<Vec<MenuItem>>,
    fsm: FsmState,
    laid: Option<Laid>,
    width: c_int,
    on_selected: Option<MenuSelection>,
    anchor: LogicalPoint,
}

#[derive(Default)]
struct MenuState {
    phase: Phase,
    generation: Option<Generation>,
    next_generation: u64,
    active: bool,
    menu: Option<Menu>,
    job: Option<RenderJob>,
    pending: VecDeque<SurfaceOp>,
    draining: bool,
    shutdown: bool,
}

enum SurfaceOp {
    Arm {
        generation: Generation,
        anchor: LogicalPoint,
        serial: u32,
    },
    MapArmed {
        generation: Generation,
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
            SurfaceOp::Arm {
                generation,
                anchor,
                serial,
            } => surface.arm(generation, anchor, serial),
            SurfaceOp::MapArmed { generation } => surface.map_armed(generation),
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
            layout: Arc::clone(&menu.laid.as_ref()?.layout),
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

fn view(laid: &Laid) -> Option<WindowExtent> {
    let scale = laid.metrics.scale;
    let physical = PhysicalSize {
        w: laid.content.w,
        h: laid.view_ph,
    };
    WindowExtent::new(physical, scale, physical.to_logical(scale)?)
}

fn placement(menu: &Menu) -> Option<MenuPlacement> {
    Some(MenuPlacement {
        anchor: menu.anchor,
        view: view(menu.laid.as_ref()?)?,
    })
}

fn close_unplaceable(state: &mut MenuState, what: &'static str) -> Option<Resolve> {
    if let Some(laid) = state.menu.as_ref().and_then(|m| m.laid.as_ref()) {
        tracing::error!(
            target: "menu",
            "menu {what} {}x{} is unrepresentable at scale {}; closing",
            laid.content.w,
            laid.view_ph,
            laid.metrics.scale
        );
    }
    close_current(state, MenuClose::Finished)
}

fn buffer_point(menu: &Menu, at: MenuPoint) -> Option<(c_int, c_int)> {
    let laid = menu.laid.as_ref()?;
    let scale = laid.metrics.scale;
    let (x, y) = match at {
        MenuPoint::Physical { x, y } => (x, y),
        MenuPoint::Logical { x, y } => (scale.to_physical(x)?, scale.to_physical(y)?),
    };
    Some((x, y + laid.scroll))
}

fn row_height(laid: &Laid) -> c_int {
    laid.layout
        .rows
        .iter()
        .find(|r| !r.separator)
        .map_or(1, |r| r.h.max(1))
}

fn scroll_active_into_view(laid: &mut Laid, active: c_int) {
    if laid.view_ph >= laid.content.h {
        return;
    }
    let Some(r) = laid.layout.rows.iter().find(|r| r.item as i32 == active) else {
        return;
    };
    if r.y < laid.scroll {
        laid.scroll = r.y;
    } else if r.y + r.h > laid.scroll + laid.view_ph {
        laid.scroll = r.y + r.h - laid.view_ph;
    }
    laid.scroll = laid.scroll.clamp(0, (laid.content.h - laid.view_ph).max(0));
}

fn on_layout(
    state: &mut MenuState,
    generation: Generation,
    layout: Layout,
    metrics: MenuMetrics,
) -> Option<Resolve> {
    if state.generation != Some(generation) {
        return None;
    }
    let menu = state.menu.as_mut()?;
    let content = PhysicalSize {
        w: layout.width,
        h: layout.height,
    };
    let (anchor, width, active) = (menu.anchor, menu.width, menu.fsm.active);
    let laid = menu.laid.insert(Laid {
        layout: Arc::new(layout),
        metrics,
        content,
        view_ph: content.h,
        scroll: 0,
    });
    let Some(anchor_ph_y) = metrics.scale.to_physical(anchor.y) else {
        return close_unplaceable(state, "anchor");
    };
    laid.view_ph = view_ph(
        content.h,
        row_height(laid),
        width,
        metrics.clamp_ph,
        anchor_ph_y,
    );
    scroll_active_into_view(laid, active);
    match state.phase {
        Phase::Armed(configured) => begin_menu(state, configured),
        Phase::Idle => {
            state.phase = Phase::AwaitArmed;
            queue(
                state,
                SurfaceOp::Arm {
                    generation,
                    anchor,
                    serial: 0,
                },
            );
            None
        }
        Phase::Shown => {
            let Some(place) = state.menu.as_ref().and_then(placement) else {
                return close_unplaceable(state, "presented size");
            };
            queue(state, SurfaceOp::Reposition { generation, place });
            request_paint(state);
            None
        }
        Phase::AwaitArmed | Phase::AwaitMenu => None,
    }
}

fn on_pixels(state: &mut MenuState, generation: Generation, pixels: Vec<u8>) -> Option<Resolve> {
    if state.generation != Some(generation) {
        return None;
    }
    let laid = state.menu.as_ref()?.laid.as_ref()?;
    let buffer = laid.content;
    let scroll = laid.scroll;
    let Some(view) = view(laid) else {
        return close_unplaceable(state, "presented size");
    };
    queue(
        state,
        SurfaceOp::Present(MenuPaint {
            generation,
            pixels,
            buffer,
            scroll,
            view,
        }),
    );
    None
}

fn begin_menu(state: &mut MenuState, configured: Configured) -> Option<Resolve> {
    let Configured { .. } = configured;
    let generation = state.generation?;
    let menu = state.menu.as_ref()?;
    let Some(place) = placement(menu) else {
        return close_unplaceable(state, "presented size");
    };
    state.active = true;
    state.phase = Phase::AwaitMenu;
    queue(state, SurfaceOp::MapArmed { generation });
    queue(state, SurfaceOp::Reposition { generation, place });
    None
}

fn step(state: &mut MenuState, ev: MenuEvent) -> Option<Resolve> {
    let menu = state.menu.as_mut()?;
    let layout = menu.laid.as_ref().map(|l| Arc::clone(&l.layout));
    let items = Arc::clone(&menu.items);
    let effects = interaction_fsm::step(&mut menu.fsm, &ev, layout.as_deref(), &items);
    let active = menu.fsm.active;
    if matches!(ev, MenuEvent::Key(_))
        && let Some(laid) = menu.laid.as_mut()
    {
        scroll_active_into_view(laid, active);
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
                let Some(mut layout) = render::layout(&mut fonts, &items, metrics.scale) else {
                    tracing::error!(target: "menu", "menu metrics are unrepresentable at scale {}", metrics.scale);
                    continue;
                };
                if width > 0 {
                    let Some(width) = metrics.scale.to_physical(width) else {
                        tracing::error!(target: "menu", "requested width {width} is unrepresentable at scale {}", metrics.scale);
                        continue;
                    };
                    layout.width = width;
                }
                emitter.update(|s| on_layout(s, generation, layout, metrics));
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
                emitter.update(|s| on_pixels(s, generation, pixels));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;
    use std::sync::mpsc::{Receiver, Sender, channel};

    use jfn_platform_abi::Scale;

    use super::*;

    struct NoopSurface;

    impl PopupSurface for NoopSurface {
        fn metrics(&self) -> MenuMetrics {
            MenuMetrics {
                scale: Scale::ONE,
                clamp_ph: None,
            }
        }
        fn arm(&self, _generation: Generation, _anchor: LogicalPoint, _serial: u32) {}
        fn map_armed(&self, _generation: Generation) {}
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
                scale: Scale::ONE,
                clamp_ph: None,
            }
        }
        fn arm(&self, _generation: Generation, _anchor: LogicalPoint, _serial: u32) {
            self.seen.lock().push("arm");
        }
        fn map_armed(&self, _generation: Generation) {
            self.seen.lock().push("map_armed");
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

    #[derive(Default)]
    struct ReentrantSurface {
        seen: Mutex<Vec<&'static str>>,
        emitter: OnceLock<Arc<Emitter>>,
    }

    impl PopupSurface for ReentrantSurface {
        fn metrics(&self) -> MenuMetrics {
            MenuMetrics {
                scale: Scale::ONE,
                clamp_ph: None,
            }
        }
        fn arm(&self, generation: Generation, _anchor: LogicalPoint, _serial: u32) {
            self.seen.lock().push("arm");
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
        fn map_armed(&self, _generation: Generation) {
            self.seen.lock().push("map_armed");
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

    fn menu_at(scale: Scale, layout: Option<Layout>) -> Menu {
        Menu {
            items: Arc::new(vec![selectable_item()]),
            fsm: FsmState::default(),
            laid: layout.map(|layout| Laid {
                layout: Arc::new(layout),
                metrics: MenuMetrics {
                    scale,
                    clamp_ph: None,
                },
                content: PhysicalSize { w: 150, h: 90 },
                view_ph: 90,
                scroll: 0,
            }),
            width: 0,
            on_selected: None,
            anchor: LogicalPoint { x: 0, y: 0 },
        }
    }

    fn selectable_item() -> MenuItem {
        MenuItem {
            id: 1,
            label: "One".into(),
            enabled: true,
            separator: false,
        }
    }

    fn force_active(menu: &SoftwareMenu) {
        menu.emitter.update(|s| {
            s.active = true;
            None
        });
    }

    fn deliver_layout_sized(menu: &SoftwareMenu, w: i32, h: i32) {
        menu.emitter.update(|s| {
            let generation = s.generation?;
            on_layout(
                s,
                generation,
                Layout::for_test(w, h, Vec::new(), Vec::new()),
                MenuMetrics {
                    scale: Scale::ONE,
                    clamp_ph: None,
                },
            )
        });
    }

    fn deliver_layout(menu: &SoftwareMenu) {
        deliver_layout_sized(menu, 100, 40);
    }

    fn deliver_ready(menu: &SoftwareMenu) {
        if let Some(generation) = menu.emitter.mailbox.peek(|s| s.generation) {
            menu.on_ready(generation);
        }
    }

    #[test]
    fn a_keyboard_opened_menu_commits_no_buffer_before_its_surface_is_configured() {
        let surface = Arc::new(RecordingSurface::default());
        let menu = menu_on(Arc::clone(&surface) as Arc<dyn PopupSurface>, true);
        let (req, _rx) = request(vec![selectable_item()]);
        menu.open(req);
        deliver_layout(&menu);
        assert_eq!(*surface.seen.lock(), vec!["arm"]);
        deliver_ready(&menu);
        assert_eq!(*surface.seen.lock(), vec!["arm", "map_armed", "reposition"]);
    }

    #[test]
    fn a_pointer_armed_menu_commits_no_buffer_before_its_surface_is_configured() {
        let surface = Arc::new(RecordingSurface::default());
        let menu = menu_on(Arc::clone(&surface) as Arc<dyn PopupSurface>, true);
        menu.arm(0, 0, 1);
        assert_eq!(*surface.seen.lock(), vec!["arm"]);
        deliver_ready(&menu);
        assert_eq!(*surface.seen.lock(), vec!["arm"]);
        let (req, _rx) = request(vec![selectable_item()]);
        menu.open(req);
        deliver_layout(&menu);
        assert_eq!(*surface.seen.lock(), vec!["arm", "map_armed", "reposition"]);
    }

    #[test]
    fn a_menu_whose_size_names_no_extent_closes_instead_of_holding_the_grab() {
        let surface = Arc::new(RecordingSurface::default());
        let menu = menu_on(Arc::clone(&surface) as Arc<dyn PopupSurface>, true);
        menu.arm(0, 0, 1);
        deliver_ready(&menu);
        let (req, rx) = request(vec![selectable_item()]);
        menu.open(req);
        deliver_layout_sized(&menu, 1, 1);
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
        assert!(!menu.has_menu());
        assert!(!menu.is_engaged());
        assert_eq!(*surface.seen.lock(), vec!["arm", "destroy"]);
    }

    #[test]
    fn a_relayout_while_shown_repositions_the_popup() {
        let surface = Arc::new(RecordingSurface::default());
        let menu = menu_on(Arc::clone(&surface) as Arc<dyn PopupSurface>, true);
        let (req, _rx) = request(vec![selectable_item()]);
        menu.open(req);
        deliver_layout(&menu);
        deliver_ready(&menu);
        deliver_ready(&menu);
        deliver_layout(&menu);
        assert_eq!(
            *surface.seen.lock(),
            vec!["arm", "map_armed", "reposition", "reposition"]
        );
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
        assert_eq!(*surface.seen.lock(), vec!["arm", "destroy"]);
    }

    #[test]
    fn a_surface_call_that_re_enters_the_menu_keeps_op_order() {
        let surface = Arc::new(ReentrantSurface::default());
        let menu = menu_on(Arc::clone(&surface) as Arc<dyn PopupSurface>, true);
        let _ = surface.emitter.set(Arc::clone(&menu.emitter));
        menu.arm(0, 0, 1);
        assert_eq!(*surface.seen.lock(), vec!["arm", "destroy"]);
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
    fn a_logical_pointer_maps_through_the_reported_scale_at_every_covered_scale() {
        const AT: (c_int, c_int) = (50, 30);
        let agrees: Vec<Option<bool>> = jfn_platform_abi::COVERED_SCALES
            .into_iter()
            .map(|scale| {
                let menu = menu_at(
                    scale,
                    Some(Layout::for_test(150, 90, Vec::new(), Vec::new())),
                );
                let logical = buffer_point(&menu, MenuPoint::Logical { x: AT.0, y: AT.1 });
                let physical = buffer_point(
                    &menu,
                    MenuPoint::Physical {
                        x: scale.to_physical(AT.0)?,
                        y: scale.to_physical(AT.1)?,
                    },
                );
                Some(logical == physical && logical.is_some())
            })
            .collect();
        assert_eq!(
            agrees,
            vec![Some(true); jfn_platform_abi::COVERED_SCALES.len()]
        );
        assert_eq!(
            buffer_point(
                &menu_at(Scale::ONE, None),
                MenuPoint::Physical { x: 1, y: 1 }
            ),
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
    fn one_detent_scrolls_one_row() {
        assert_eq!(scroll_step(120, 28), 28);
        assert_eq!(scroll_step(-120, 28), -28);
        assert_eq!(scroll_step(0, 28), 0);
    }
}
