use std::collections::VecDeque;
use std::ffi::c_int;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::thread::JoinHandle;

use jfn_mailbox::Mailbox;
use jfn_platform_abi::{
    ExtentError, Generation, LogicalSize, MENU_DISMISSED, MenuClose, MenuHandle, MenuHost,
    MenuItem, MenuMetrics, MenuOpenError, MenuPaint, MenuPlacement, MenuRequest, MenuSelection,
    MenuShutdownError, PhysicalSize, PopupSurface, Unrepresentable, WindowBasis, WindowExtent,
    WindowPoint, menu_has_selectable, menu_initial_row,
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
    Physical { x: c_int, y: c_int },
    /// Logical (surface) pixels.
    Logical { x: c_int, y: c_int },
}

/// Content input addressed to a particular menu lifetime.
pub enum MenuInputEvent {
    Pointer { at: MenuPoint, press: bool },
    Key(u32),
    Scroll(c_int),
    Expose,
}

#[derive(Debug, thiserror::Error)]
pub enum MenuSpawnError {
    #[error("{thread} thread spawn: {source}")]
    Thread {
        thread: &'static str,
        #[source]
        source: std::io::Error,
    },
}

pub struct SoftwareMenu {
    emitter: Arc<Emitter>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl SoftwareMenu {
    pub fn spawn(surface: Arc<dyn PopupSurface>) -> Result<SoftwareMenu, MenuSpawnError> {
        let emitter = Emitter::new(surface);
        let thread = {
            let emitter = Arc::clone(&emitter);
            std::thread::Builder::new()
                .name("jfn-menu".into())
                .spawn(move || run(&emitter))
                .map_err(|source| MenuSpawnError::Thread {
                    thread: "menu render",
                    source,
                })?
        };
        Ok(SoftwareMenu {
            emitter,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn open_triggered(
        &self,
        req: MenuRequest,
        serial: u32,
    ) -> Result<MenuHandle, MenuOpenError> {
        self.open_impl(req, serial)
    }

    fn open_impl(&self, req: MenuRequest, serial: u32) -> Result<MenuHandle, MenuOpenError> {
        let refusal = if !self.render_thread_alive() {
            Some(MenuOpenError::EngineUnavailable)
        } else if !menu_has_selectable(&req.items) {
            Some(MenuOpenError::NothingSelectable)
        } else {
            None
        };
        if let Some(e) = refusal {
            req.on_selected.resolve(MENU_DISMISSED);
            return Err(e);
        }
        let surface = &self.emitter.surface;
        let mut refused = None;
        let mut handle = Err(MenuOpenError::StaleBasis);
        self.emitter.update(|s| {
            if surface.window_basis() != req.basis {
                refused = Some(req.on_selected);
                return None;
            }
            let resolve = close_current(s, MenuClose::Finished);
            let id = next_id(&mut s.next_session);
            handle = Ok(MenuHandle::new(id));
            s.session = Session::Laying(Menu {
                id,
                fsm: FsmState {
                    active: menu_initial_row(&req.items, req.initial),
                },
                items: Arc::new(req.items),
                width: req.width,
                on_selected: Some(req.on_selected),
                anchor: req.anchor,
                basis: req.basis,
                serial,
            });
            s.job = Some(RenderJob::Shape);
            resolve
        });
        if let Some(selection) = refused {
            selection.resolve(MENU_DISMISSED);
        }
        handle
    }

    fn render_thread_alive(&self) -> bool {
        self.thread.lock().is_some()
    }

    pub fn on_ready(&self, generation: Generation, configured: LogicalSize) {
        let surface = &self.emitter.surface;
        self.emitter.update(|s| {
            let Session::Created(live) = &mut s.session else {
                return None;
            };
            if live.generation != generation {
                return None;
            }
            if surface.window_basis() != live.menu.basis {
                return close_current(s, MenuClose::Finished);
            }
            let asked = match view(&live.laid) {
                Ok(v) => v.logical(),
                Err(e) => {
                    tracing::error!(target: "menu", "menu extent as asked: {e}; closing");
                    return close_current(s, MenuClose::Finished);
                }
            };
            if asked != configured {
                let scale = live.laid.metrics.scale;
                match scale.to_physical(configured.h) {
                    Ok(h) => live.laid.view_ph = live.laid.view_ph.min(h),
                    Err(e) => {
                        tracing::error!(target: "menu", "configured menu height: {e}; closing");
                        return close_current(s, MenuClose::Finished);
                    }
                }
                let active = live.menu.fsm.active;
                scroll_active_into_view(&mut live.laid, active);
            }
            let Session::Created(live) = std::mem::take(&mut s.session) else {
                return None;
            };
            s.session = Session::Shown(live);
            request_paint(s);
            None
        });
    }

    pub fn on_done(&self, generation: Generation) {
        self.emitter.update(|s| {
            if s.session.generation() != Some(generation) {
                return None;
            }
            close_current(s, MenuClose::External)
        });
    }

    pub fn window_changed(&self) {
        let surface = &self.emitter.surface;
        self.emitter.update(|s| {
            let basis = s.session.menu()?.basis;
            if surface.window_basis() == basis {
                return None;
            }
            close_current(s, MenuClose::Finished)
        });
    }

    /// Stale events from a retired native object cannot affect its successor.
    pub fn input(&self, generation: Generation, event: MenuInputEvent) {
        self.emitter.update(|s| {
            let Session::Shown(live) = &mut s.session else {
                return None;
            };
            if live.generation != generation {
                return None;
            }
            match event {
                MenuInputEvent::Pointer { at, press } => {
                    let (x, y) = match buffer_point(&live.laid, at) {
                        Ok(point) => point,
                        Err(e) => {
                            tracing::error!(target: "menu", "menu pointer position: {e}; closing");
                            return close_current(s, MenuClose::Finished);
                        }
                    };
                    step(
                        s,
                        if press {
                            MenuEvent::Press { x, y }
                        } else {
                            MenuEvent::Motion { x, y }
                        },
                    )
                }
                MenuInputEvent::Key(keysym) => step(s, MenuEvent::Key(keysym)),
                MenuInputEvent::Scroll(dy) => {
                    let laid = &mut live.laid;
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
                MenuInputEvent::Expose => {
                    request_paint(s);
                    None
                }
            }
        });
    }

    #[cfg(test)]
    fn is_engaged(&self) -> bool {
        self.emitter
            .mailbox
            .peek(|s| !matches!(s.session, Session::Idle))
    }
}

impl MenuHost for SoftwareMenu {
    fn warm(&self) {}

    fn open(&self, req: MenuRequest) -> Result<MenuHandle, MenuOpenError> {
        self.open_impl(req, 0)
    }

    fn hide(&self, handle: MenuHandle) -> Result<(), jfn_platform_abi::MenuHideError> {
        self.emitter.update(|s| {
            if s.session.menu().map(|m| MenuHandle::new(m.id)) != Some(handle) {
                return None;
            }
            close_current(s, MenuClose::Finished)
        });
        Ok(())
    }

    fn shutdown(&self) -> Result<(), MenuShutdownError> {
        self.emitter.update(|s| {
            s.shutdown = true;
            close_current(s, MenuClose::Finished)
        });
        let thread = self.thread.lock().take();
        match thread.map(JoinHandle::join) {
            Some(Err(_)) => Err(MenuShutdownError::ThreadPanicked),
            Some(Ok(())) | None => Ok(()),
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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum RenderJob {
    Paint,
    Shape,
}

struct Laid {
    layout: Arc<Layout>,
    metrics: MenuMetrics,
    /// Full content size, physical px.
    content: PhysicalSize,
    /// Visible height, physical px; never above `content.h`.
    view_ph: c_int,
    /// Scroll offset into the content, physical px, `0..=content.h - view_ph`.
    scroll: c_int,
}

struct Menu {
    id: NonZeroU64,
    items: Arc<Vec<MenuItem>>,
    fsm: FsmState,
    /// Desired logical width; `<= 0` is content-sized.
    width: c_int,
    on_selected: Option<MenuSelection>,
    anchor: WindowPoint,
    basis: WindowBasis,
    serial: u32,
}

struct Live {
    menu: Menu,
    laid: Laid,
    generation: Generation,
}

#[derive(Default)]
enum Session {
    #[default]
    Idle,
    Laying(Menu),
    Created(Live),
    Shown(Live),
}

impl Session {
    fn menu(&self) -> Option<&Menu> {
        match self {
            Session::Idle => None,
            Session::Laying(menu) => Some(menu),
            Session::Created(live) | Session::Shown(live) => Some(&live.menu),
        }
    }

    fn generation(&self) -> Option<Generation> {
        match self {
            Session::Idle | Session::Laying(_) => None,
            Session::Created(live) | Session::Shown(live) => Some(live.generation),
        }
    }
}

#[derive(Default)]
struct MenuState {
    session: Session,
    next_session: u64,
    next_generation: u64,
    job: Option<RenderJob>,
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
            SurfaceOp::Create { generation, place } => surface.create(generation, place),
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
        id: NonZeroU64,
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
    match (&state.session, job) {
        (Session::Laying(menu), RenderJob::Shape) => Some(Job::Shape {
            id: menu.id,
            items: Arc::clone(&menu.items),
            width: menu.width,
        }),
        (Session::Shown(live), RenderJob::Paint) => Some(Job::Paint {
            generation: live.generation,
            items: Arc::clone(&live.menu.items),
            layout: Arc::clone(&live.laid.layout),
            active: live.menu.fsm.active,
        }),
        _ => None,
    }
}

fn request_paint(state: &mut MenuState) {
    state.job = Some(
        state
            .job
            .map_or(RenderJob::Paint, |j| j.max(RenderJob::Paint)),
    );
}

/// The extent the menu's presented size names: the full content width and the
/// visible height, in both spaces.
fn view(laid: &Laid) -> Result<WindowExtent, ExtentError> {
    let scale = laid.metrics.scale;
    let physical = PhysicalSize {
        w: laid.content.w,
        h: laid.view_ph,
    };
    WindowExtent::new(physical, scale, physical.to_logical(scale)?)
}

fn buffer_point(laid: &Laid, at: MenuPoint) -> Result<(c_int, c_int), Unrepresentable> {
    let scale = laid.metrics.scale;
    let (x, y) = match at {
        MenuPoint::Physical { x, y } => (x, y),
        MenuPoint::Logical { x, y } => (scale.to_physical(x)?, scale.to_physical(y)?),
    };
    Ok((x, y + laid.scroll))
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
    surface: &dyn PopupSurface,
    id: NonZeroU64,
    layout: Result<Layout, Unrepresentable>,
    metrics: MenuMetrics,
) -> Option<Resolve> {
    let Session::Laying(menu) = &state.session else {
        return None;
    };
    if menu.id != id {
        return None;
    }
    let layout = match layout {
        Ok(layout) => layout,
        Err(e) => {
            tracing::error!(target: "menu", "menu layout: {e}; closing");
            return close_current(state, MenuClose::Finished);
        }
    };
    if surface.window_basis() != menu.basis {
        return close_current(state, MenuClose::Finished);
    }
    let content = PhysicalSize {
        w: layout.width,
        h: layout.height,
    };
    let mut laid = Laid {
        layout: Arc::new(layout),
        metrics,
        content,
        view_ph: content.h,
        scroll: 0,
    };
    let (_, anchor_y) = menu.anchor.floor();
    let placed = metrics
        .scale
        .to_physical(anchor_y)
        .map_err(ExtentError::from)
        .and_then(|anchor_ph_y| {
            laid.view_ph = view_ph(
                content.h,
                row_height(&laid),
                menu.width,
                metrics.clamp_ph,
                anchor_ph_y,
            );
            view(&laid)
        });
    let view = match placed {
        Ok(view) => view,
        Err(e) => {
            tracing::error!(target: "menu", "menu extent: {e}; closing");
            return close_current(state, MenuClose::Finished);
        }
    };
    scroll_active_into_view(&mut laid, menu.fsm.active);
    let Session::Laying(menu) = std::mem::take(&mut state.session) else {
        return None;
    };
    let generation = next_id(&mut state.next_generation);
    let place = MenuPlacement {
        anchor: menu.anchor,
        view,
        serial: menu.serial,
        basis: menu.basis,
    };
    state.session = Session::Created(Live {
        menu,
        laid,
        generation,
    });
    queue(state, SurfaceOp::Create { generation, place });
    None
}

fn on_pixels(
    state: &mut MenuState,
    surface: &dyn PopupSurface,
    generation: Generation,
    pixels: Vec<u8>,
) -> Option<Resolve> {
    let Session::Shown(live) = &state.session else {
        return None;
    };
    if live.generation != generation {
        return None;
    }
    let basis = live.menu.basis;
    if surface.window_basis() != basis {
        return close_current(state, MenuClose::Finished);
    }
    let buffer = live.laid.content;
    let scroll = live.laid.scroll;
    let view = match view(&live.laid) {
        Ok(view) => view,
        Err(e) => {
            tracing::error!(target: "menu", "menu presented extent: {e}; closing");
            return close_current(state, MenuClose::Finished);
        }
    };
    queue(
        state,
        SurfaceOp::Present(MenuPaint {
            generation,
            pixels,
            buffer,
            scroll,
            view,
            basis,
        }),
    );
    None
}

fn step(state: &mut MenuState, ev: MenuEvent) -> Option<Resolve> {
    let Session::Shown(live) = &mut state.session else {
        return None;
    };
    let layout = Arc::clone(&live.laid.layout);
    let items = Arc::clone(&live.menu.items);
    let effects = interaction_fsm::step(&mut live.menu.fsm, &ev, Some(&*layout), &items);
    let active = live.menu.fsm.active;
    if matches!(ev, MenuEvent::Key(_)) {
        scroll_active_into_view(&mut live.laid, active);
    }
    for effect in effects {
        match effect {
            MenuEffect::Redraw => request_paint(state),
            MenuEffect::Close(id) => {
                return close(state, MenuClose::Finished)
                    .map(|selection| Resolve { selection, id });
            }
        }
    }
    None
}

fn close_current(state: &mut MenuState, reason: MenuClose) -> Option<Resolve> {
    close(state, reason).map(Resolve::dismissed)
}

fn close(state: &mut MenuState, reason: MenuClose) -> Option<MenuSelection> {
    state.job = None;
    let (mut menu, generation) = match std::mem::take(&mut state.session) {
        Session::Idle => return None,
        Session::Laying(menu) => (menu, None),
        Session::Created(live) | Session::Shown(live) => (live.menu, Some(live.generation)),
    };
    if let Some(generation) = generation {
        queue(state, SurfaceOp::Destroy { generation, reason });
    }
    menu.on_selected.take()
}

fn next_id(counter: &mut u64) -> NonZeroU64 {
    *counter = counter.wrapping_add(1);
    NonZeroU64::new(*counter).unwrap_or(NonZeroU64::MIN)
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
            Job::Shape { id, items, width } => {
                let metrics = emitter.surface.metrics();
                let layout =
                    render::layout(&mut fonts, &items, metrics.scale).and_then(|mut layout| {
                        if width > 0 {
                            layout.width = metrics.scale.to_physical(width)?;
                        }
                        Ok(layout)
                    });
                emitter.update(|s| on_layout(s, &*emitter.surface, id, layout, metrics));
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
                emitter.update(|s| on_pixels(s, &*emitter.surface, generation, pixels));
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

    #[derive(Default)]
    struct RecordingSurface {
        seen: Mutex<Vec<&'static str>>,
        basis: Mutex<WindowBasis>,
    }

    impl PopupSurface for RecordingSurface {
        fn metrics(&self) -> MenuMetrics {
            MenuMetrics {
                scale: Scale::ONE,
                clamp_ph: None,
            }
        }
        fn window_basis(&self) -> WindowBasis {
            *self.basis.lock()
        }
        fn create(&self, _generation: Generation, _place: MenuPlacement) {
            self.seen.lock().push("create");
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
        fn window_basis(&self) -> WindowBasis {
            WindowBasis::FIXED
        }
        fn create(&self, generation: Generation, _place: MenuPlacement) {
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

    fn recorded(alive: bool) -> (Arc<RecordingSurface>, SoftwareMenu) {
        let surface = Arc::new(RecordingSurface::default());
        let menu = menu_on(Arc::clone(&surface) as Arc<dyn PopupSurface>, alive);
        (surface, menu)
    }

    fn request_row(items: Vec<MenuItem>, initial: c_int) -> (MenuRequest, Receiver<c_int>) {
        let (tx, rx): (Sender<c_int>, Receiver<c_int>) = channel();
        let req = MenuRequest {
            items,
            anchor: WindowPoint::from_window(0.0, 0.0),
            basis: WindowBasis::FIXED,
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

    fn laid_at(scale: Scale) -> Laid {
        Laid {
            layout: Arc::new(Layout::for_test(150, 90, Vec::new(), Vec::new())),
            metrics: MenuMetrics {
                scale,
                clamp_ph: None,
            },
            content: PhysicalSize { w: 150, h: 90 },
            view_ph: 90,
            scroll: 0,
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

    fn deliver_layout_result(menu: &SoftwareMenu, layout: Result<Layout, Unrepresentable>) {
        let surface = Arc::clone(&menu.emitter.surface);
        menu.emitter.update(|s| {
            let id = s.session.menu()?.id;
            on_layout(
                s,
                &*surface,
                id,
                layout,
                MenuMetrics {
                    scale: Scale::ONE,
                    clamp_ph: None,
                },
            )
        });
    }

    fn deliver_layout_sized(menu: &SoftwareMenu, w: i32, h: i32) {
        deliver_layout_result(menu, Ok(Layout::for_test(w, h, Vec::new(), Vec::new())));
    }

    fn deliver_layout(menu: &SoftwareMenu) {
        deliver_layout_sized(menu, 100, 40);
    }

    fn deliver_ready(menu: &SoftwareMenu, configured: LogicalSize) {
        if let Some(generation) = menu.emitter.mailbox.peek(|s| s.session.generation()) {
            menu.on_ready(generation, configured);
        }
    }

    fn view_ph_of(menu: &SoftwareMenu) -> Option<c_int> {
        menu.emitter.mailbox.peek(|s| match &s.session {
            Session::Created(live) | Session::Shown(live) => Some(live.laid.view_ph),
            Session::Idle | Session::Laying(_) => None,
        })
    }

    #[test]
    fn a_menu_creates_its_surface_once_and_only_after_its_layout() {
        let (surface, menu) = recorded(true);
        let (req, _rx) = request(vec![selectable_item()]);
        assert!(menu.open(req).is_ok());
        assert!(surface.seen.lock().is_empty());
        deliver_layout(&menu);
        assert_eq!(*surface.seen.lock(), vec!["create"]);
        deliver_ready(&menu, LogicalSize { w: 100, h: 40 });
        deliver_ready(&menu, LogicalSize { w: 100, h: 40 });
        assert_eq!(*surface.seen.lock(), vec!["create"]);
    }

    #[test]
    fn a_menu_whose_size_names_no_extent_closes_without_a_surface() {
        let (surface, menu) = recorded(true);
        let (req, rx) = request(vec![selectable_item()]);
        assert!(menu.open(req).is_ok());
        deliver_layout_sized(&menu, 1, 1);
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
        assert!(!menu.is_engaged());
        assert!(surface.seen.lock().is_empty());
    }

    #[test]
    fn a_configured_size_equal_to_the_request_leaves_the_height_untouched() {
        let (_surface, menu) = recorded(true);
        let (req, _rx) = request(vec![selectable_item()]);
        assert!(menu.open(req).is_ok());
        deliver_layout(&menu);
        deliver_ready(&menu, LogicalSize { w: 100, h: 40 });
        assert_eq!(view_ph_of(&menu), Some(40));
    }

    #[test]
    fn a_configured_size_never_raises_the_visible_height() {
        let (_surface, menu) = recorded(true);
        let (req, _rx) = request(vec![selectable_item()]);
        assert!(menu.open(req).is_ok());
        deliver_layout(&menu);
        deliver_ready(&menu, LogicalSize { w: 100, h: 90 });
        assert_eq!(view_ph_of(&menu), Some(40));
    }

    #[test]
    fn a_smaller_configured_size_lowers_the_visible_height() {
        let (_surface, menu) = recorded(true);
        let (req, _rx) = request(vec![selectable_item()]);
        assert!(menu.open(req).is_ok());
        deliver_layout(&menu);
        deliver_ready(&menu, LogicalSize { w: 100, h: 30 });
        assert_eq!(view_ph_of(&menu), Some(30));
    }

    #[test]
    fn hide_resolves_the_pending_selection_as_dismissed() {
        let (_surface, menu) = recorded(true);
        let (req, rx) = request(vec![selectable_item()]);
        let handle = menu.open(req);
        assert!(rx.try_recv().is_err());
        if let Ok(handle) = handle {
            menu.hide(handle).unwrap();
        }
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
    }

    #[test]
    fn a_hide_for_a_replaced_menu_leaves_its_successor_open() {
        let (_surface, menu) = recorded(true);
        let (first, first_rx) = request(vec![selectable_item()]);
        let stale = menu.open(first);
        let (second, second_rx) = request(vec![selectable_item()]);
        assert!(menu.open(second).is_ok());
        assert_eq!(first_rx.try_recv(), Ok(MENU_DISMISSED));
        if let Ok(stale) = stale {
            menu.hide(stale).unwrap();
        }
        assert!(second_rx.try_recv().is_err());
        assert!(menu.is_engaged());
    }

    #[test]
    fn a_refused_request_resolves_itself_and_leaves_the_live_menu_open() {
        let (_surface, menu) = recorded(true);
        let (live, live_rx) = request(vec![selectable_item()]);
        assert!(menu.open(live).is_ok());
        let (req, rx) = request(vec![MenuItem {
            id: 0,
            label: String::new(),
            enabled: false,
            separator: true,
        }]);
        assert_eq!(menu.open(req), Err(MenuOpenError::NothingSelectable));
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
        assert!(live_rx.try_recv().is_err());
        assert!(menu.is_engaged());
    }

    #[test]
    fn open_without_a_render_thread_resolves_and_leaves_the_state_idle() {
        let (_surface, menu) = recorded(false);
        let (req, rx) = request(vec![selectable_item()]);
        assert_eq!(menu.open(req), Err(MenuOpenError::EngineUnavailable));
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
        assert!(!menu.is_engaged());
    }

    #[test]
    fn a_request_under_a_replaced_window_state_is_refused() {
        let (surface, menu) = recorded(true);
        *surface.basis.lock() = WindowBasis::FIXED.next();
        let (req, rx) = request(vec![selectable_item()]);
        assert_eq!(menu.open(req), Err(MenuOpenError::StaleBasis));
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
        assert!(!menu.is_engaged());
    }

    #[test]
    fn a_window_change_before_the_layout_arrives_closes_the_menu() {
        let (surface, menu) = recorded(true);
        let (req, rx) = request(vec![selectable_item()]);
        assert!(menu.open(req).is_ok());
        *surface.basis.lock() = WindowBasis::FIXED.next();
        deliver_layout(&menu);
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
        assert!(surface.seen.lock().is_empty());
    }

    #[test]
    fn a_layout_error_for_the_current_session_dismisses_the_request() {
        let (surface, menu) = recorded(true);
        let (req, rx) = request(vec![selectable_item()]);
        assert!(menu.open(req).is_ok());
        deliver_layout_result(
            &menu,
            Err(Unrepresentable {
                value: c_int::MAX,
                scale: Scale::ONE,
            }),
        );
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
        assert!(!menu.is_engaged());
        assert!(surface.seen.lock().is_empty());
    }

    #[test]
    fn a_window_change_closes_a_menu_that_is_sitting_open() {
        let (surface, menu) = recorded(true);
        let (req, rx) = request(vec![selectable_item()]);
        assert!(menu.open(req).is_ok());
        deliver_layout(&menu);
        deliver_ready(&menu, LogicalSize { w: 100, h: 40 });
        menu.window_changed();
        assert!(rx.try_recv().is_err());
        *surface.basis.lock() = WindowBasis::FIXED.next();
        menu.window_changed();
        assert_eq!(rx.try_recv(), Ok(MENU_DISMISSED));
        assert_eq!(*surface.seen.lock(), vec!["create", "destroy"]);
    }

    #[test]
    fn a_surface_call_that_re_enters_the_menu_keeps_op_order() {
        let surface = Arc::new(ReentrantSurface::default());
        let menu = menu_on(Arc::clone(&surface) as Arc<dyn PopupSurface>, true);
        let _ = surface.emitter.set(Arc::clone(&menu.emitter));
        let (req, _rx) = request(vec![selectable_item()]);
        assert!(menu.open(req).is_ok());
        deliver_layout(&menu);
        assert_eq!(*surface.seen.lock(), vec!["create", "destroy"]);
    }

    #[test]
    fn reopening_a_shown_menu_replaces_its_surface() {
        let (surface, menu) = recorded(true);
        let (first, first_rx) = request(vec![selectable_item()]);
        assert!(menu.open(first).is_ok());
        deliver_layout(&menu);
        deliver_ready(&menu, LogicalSize { w: 100, h: 40 });
        let (second, second_rx) = request(vec![selectable_item()]);
        assert!(menu.open(second).is_ok());
        assert_eq!(first_rx.try_recv(), Ok(MENU_DISMISSED));
        assert!(second_rx.try_recv().is_err());
        deliver_layout(&menu);
        assert_eq!(*surface.seen.lock(), vec!["create", "destroy", "create"]);
    }

    #[test]
    fn input_for_a_retired_surface_does_not_reach_its_successor() {
        let (_surface, menu) = recorded(true);
        let (first, _first_rx) = request(vec![selectable_item()]);
        assert!(menu.open(first).is_ok());
        deliver_layout(&menu);
        let retired = menu.emitter.mailbox.peek(|s| s.session.generation());
        let (second, second_rx) = request(vec![selectable_item()]);
        assert!(menu.open(second).is_ok());
        deliver_layout(&menu);
        deliver_ready(&menu, LogicalSize { w: 100, h: 40 });
        if let Some(retired) = retired {
            menu.on_done(retired);
        }
        assert!(second_rx.try_recv().is_err());
        assert!(menu.is_engaged());
    }

    #[test]
    fn an_out_of_range_initial_row_highlights_nothing() {
        let (_surface, menu) = recorded(true);
        let (req, _rx) = request_row(vec![selectable_item()], 5);
        assert!(menu.open(req).is_ok());
        assert_eq!(
            menu.emitter
                .mailbox
                .peek(|s| s.session.menu().map(|m| m.fsm.active)),
            Some(MENU_DISMISSED)
        );
    }

    #[test]
    fn a_logical_pointer_maps_through_the_reported_scale_at_every_covered_scale() {
        const AT: (c_int, c_int) = (50, 30);
        let agrees: Vec<Result<bool, Unrepresentable>> = jfn_platform_abi::COVERED_SCALES
            .into_iter()
            .map(|scale| {
                let laid = laid_at(scale);
                let logical = buffer_point(&laid, MenuPoint::Logical { x: AT.0, y: AT.1 });
                let physical = buffer_point(
                    &laid,
                    MenuPoint::Physical {
                        x: scale.to_physical(AT.0)?,
                        y: scale.to_physical(AT.1)?,
                    },
                );
                Ok(logical == physical && logical.is_ok())
            })
            .collect();
        assert_eq!(
            agrees,
            vec![Ok(true); jfn_platform_abi::COVERED_SCALES.len()]
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
    fn identities_start_at_one_and_never_hit_zero() {
        let mut counter = 0;
        assert_eq!(next_id(&mut counter).get(), 1);
        assert_eq!(next_id(&mut counter).get(), 2);
        counter = u64::MAX;
        assert_eq!(next_id(&mut counter).get(), NonZeroU64::MIN.get());
    }

    #[test]
    fn one_detent_scrolls_one_row() {
        assert_eq!(scroll_step(120, 28), 28);
        assert_eq!(scroll_step(-120, 28), -28);
        assert_eq!(scroll_step(0, 28), 0);
    }
}
