use std::ffi::c_int;
use std::sync::Arc;
use std::thread::JoinHandle;

use jfn_mailbox::Mailbox;
use jfn_platform_abi::{
    Generation, MENU_DISMISSED, MenuClose, MenuHost, MenuItem, MenuMetrics, MenuPaint,
    MenuPlacement, MenuRequest, MenuSelectionFn, PopupSurface,
};
use parking_lot::Mutex;

use crate::menu::interaction_fsm::{self, MenuEffect, MenuEvent, MenuState as FsmState};
use crate::menu::render::{self, Fonts, Layout, blit_bgra};

const WHEEL_DETENT: f32 = 120.0;

pub struct SoftwareMenu {
    mailbox: Mailbox<MenuState>,
    surface: Arc<dyn PopupSurface>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl SoftwareMenu {
    pub fn spawn(surface: Arc<dyn PopupSurface>) -> SoftwareMenu {
        let mailbox = Mailbox::new(MenuState::default());
        let thread = {
            let mailbox = mailbox.clone();
            let surface = Arc::clone(&surface);
            std::thread::Builder::new()
                .name("jfn-menu".into())
                .spawn(move || run(surface, mailbox))
                .ok()
        };
        SoftwareMenu {
            mailbox,
            surface,
            thread: Mutex::new(thread),
        }
    }

    fn apply(&self, out: Outbox) {
        out.emit(&*self.surface);
    }

    /// `serial` must still be grab-worthy at the call.
    pub fn arm(&self, x: c_int, y: c_int, serial: u32) {
        let (generation, fire) = self.mailbox.update(|s| {
            let fire = clear_menu(s);
            let generation = next_generation(s);
            s.generation = Some(generation);
            // The grab can activate at the popup's initial commit, and the
            // grab-induced focus loss must already observe `engaged`.
            s.engaged = true;
            s.phase = Phase::AwaitPlaceholder;
            (generation, fire)
        });
        self.apply(Outbox {
            ops: vec![SurfaceOp::Create {
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
            }],
            fire: fire.map(|f| (f, MENU_DISMISSED)),
        });
    }

    pub fn dismiss_if_speculative(&self) {
        let out = self.mailbox.update(|s| {
            if s.menu.is_some() || s.phase == Phase::Idle {
                return Outbox::default();
            }
            let generation = s.generation;
            let fire = clear_menu(s);
            Outbox {
                ops: generation
                    .map(|generation| SurfaceOp::Destroy {
                        generation,
                        reason: MenuClose::Speculative,
                    })
                    .into_iter()
                    .collect(),
                fire: fire.map(|f| (f, MENU_DISMISSED)),
            }
        });
        self.apply(out);
    }

    pub fn on_ready(&self, generation: Generation) {
        let out = self.mailbox.update(|s| {
            if s.generation != Some(generation) {
                return Outbox::default();
            }
            match s.phase {
                Phase::AwaitPlaceholder => {
                    if s.menu.as_ref().is_some_and(|m| m.layout.is_some()) {
                        begin_menu(s)
                    } else {
                        s.phase = Phase::Placeholder;
                        Outbox::default()
                    }
                }
                Phase::AwaitMenu => {
                    s.phase = Phase::Shown;
                    refresh_shown(s)
                }
                Phase::Idle | Phase::Placeholder | Phase::Shown => Outbox::default(),
            }
        });
        self.apply(out);
    }

    pub fn on_done(&self, generation: Generation) {
        let out = self.mailbox.update(|s| {
            if s.generation != Some(generation) {
                return Outbox::default();
            }
            let fire = clear_menu(s);
            Outbox {
                ops: vec![SurfaceOp::Destroy {
                    generation,
                    reason: MenuClose::External,
                }],
                fire: fire.map(|f| (f, MENU_DISMISSED)),
            }
        });
        self.apply(out);
    }

    /// `local_x`/`local_y` are logical (unscaled) popup-local coordinates.
    pub fn motion(&self, local_x: c_int, local_y: c_int) {
        self.pointer(local_x, local_y, false);
    }

    pub fn press(&self, local_x: c_int, local_y: c_int) {
        self.pointer(local_x, local_y, true);
    }

    fn pointer(&self, local_x: c_int, local_y: c_int, press: bool) {
        let out = self.mailbox.update(|s| {
            let Some(menu) = s.menu.as_ref().filter(|m| m.mapped) else {
                return Outbox::default();
            };
            let x = (local_x as f32 * menu.metrics.scale) as i32;
            let y = (local_y as f32 * menu.metrics.scale) as i32 + menu.scroll;
            let ev = if press {
                MenuEvent::Press { x, y }
            } else {
                MenuEvent::Motion { x, y }
            };
            step(s, ev)
        });
        self.apply(out);
    }

    pub fn key(&self, keysym: u32) {
        let out = self.mailbox.update(|s| {
            if s.menu.as_ref().filter(|m| m.mapped).is_none() {
                return Outbox::default();
            }
            step(s, MenuEvent::Key(keysym))
        });
        self.apply(out);
    }

    pub fn dismiss(&self) {
        let out = self.mailbox.update(|s| {
            if s.menu.as_ref().filter(|m| m.mapped).is_none() {
                return Outbox::default();
            }
            step(s, MenuEvent::Dismiss)
        });
        self.apply(out);
    }

    pub fn expose(&self) {
        self.mailbox.update(|s| {
            if s.menu.as_ref().filter(|m| m.mapped).is_some() {
                request_paint(s);
            }
        });
    }

    /// ±120 per detent, positive = wheel up.
    pub fn scroll(&self, dy: c_int) {
        self.mailbox.update(|s| {
            let Some(menu) = s.menu.as_mut().filter(|m| m.mapped) else {
                return;
            };
            if menu.view_ph >= menu.ph {
                return;
            }
            let max = (menu.ph - menu.view_ph).max(0);
            let new = (menu.scroll - scroll_step(dy, row_height(menu))).clamp(0, max);
            if new == menu.scroll {
                return;
            }
            menu.scroll = new;
            request_paint(s);
        });
    }

    pub fn is_active(&self) -> bool {
        self.mailbox.peek(|s| s.active)
    }

    pub fn is_engaged(&self) -> bool {
        self.mailbox.peek(|s| s.engaged)
    }

    pub fn has_menu(&self) -> bool {
        self.mailbox.peek(|s| s.menu.is_some())
    }
}

impl MenuHost for SoftwareMenu {
    fn warm(&self) {}

    fn open(&self, req: MenuRequest) {
        let out = self.mailbox.update(|s| {
            let fire = s.menu.as_mut().and_then(|m| m.on_selected.take());
            s.menu = Some(Menu {
                items: Arc::new(req.items),
                layout: None,
                fsm: FsmState {
                    active: req.initial,
                },
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
                mapped: false,
                anchor: (req.x, req.y),
            });
            if s.phase == Phase::Idle {
                let generation = next_generation(s);
                s.generation = Some(generation);
                s.engaged = true;
            }
            s.job = Some(RenderJob::Shape);
            Outbox {
                ops: Vec::new(),
                fire: fire.map(|f| (f, MENU_DISMISSED)),
            }
        });
        self.apply(out);
    }

    fn hide(&self) {
        let out = self.mailbox.update(|s| {
            // A hide can be the tail of a previous cycle arriving after the
            // next press already armed a fresh popup.
            if s.menu.is_none() {
                return Outbox::default();
            }
            let generation = s.generation;
            let _ = clear_menu(s);
            Outbox {
                ops: generation
                    .map(|generation| SurfaceOp::Destroy {
                        generation,
                        reason: MenuClose::Finished,
                    })
                    .into_iter()
                    .collect(),
                fire: None,
            }
        });
        self.apply(out);
    }

    fn shutdown(&self) {
        let out = self.mailbox.update(|s| {
            s.shutdown = true;
            let generation = s.generation;
            let fire = clear_menu(s);
            Outbox {
                ops: generation
                    .map(|generation| SurfaceOp::Destroy {
                        generation,
                        reason: MenuClose::Finished,
                    })
                    .into_iter()
                    .collect(),
                fire: fire.map(|f| (f, MENU_DISMISSED)),
            }
        });
        self.apply(out);
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
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
    on_selected: Option<MenuSelectionFn>,
    mapped: bool,
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
    shutdown: bool,
}

#[derive(Default)]
struct Outbox {
    ops: Vec<SurfaceOp>,
    fire: Option<(MenuSelectionFn, c_int)>,
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

impl Outbox {
    fn emit(self, surface: &dyn PopupSurface) {
        for op in self.ops {
            match op {
                SurfaceOp::Create {
                    generation,
                    place,
                    serial,
                } => surface.create(generation, place, serial),
                SurfaceOp::Reposition { generation, place } => {
                    surface.reposition(generation, place);
                }
                SurfaceOp::Present(paint) => surface.present(paint),
                SurfaceOp::Destroy { generation, reason } => surface.destroy(generation, reason),
            }
        }
        if let Some((cb, id)) = self.fire {
            cb(id);
        }
    }
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

fn on_layout(
    state: &mut MenuState,
    generation: Generation,
    layout: Layout,
    metrics: MenuMetrics,
) -> Outbox {
    if state.generation != Some(generation) {
        return Outbox::default();
    }
    let Some(menu) = state.menu.as_mut() else {
        return Outbox::default();
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
            let place = state.menu.as_ref().map(placement);
            Outbox {
                ops: place
                    .map(|place| SurfaceOp::Create {
                        generation,
                        place,
                        // 0: no triggering press; the surface substitutes
                        // whatever serial it still has.
                        serial: 0,
                    })
                    .into_iter()
                    .collect(),
                fire: None,
            }
        }
        Phase::Shown => refresh_shown(state),
        Phase::AwaitPlaceholder | Phase::AwaitMenu => Outbox::default(),
    }
}

fn on_pixels(state: &mut MenuState, generation: Generation, pixels: Vec<u8>) -> Outbox {
    if state.generation != Some(generation) {
        return Outbox::default();
    }
    let Some(menu) = state.menu.as_mut() else {
        return Outbox::default();
    };
    menu.mapped = true;
    Outbox {
        ops: vec![SurfaceOp::Present(MenuPaint {
            generation,
            pixels,
            pw: menu.pw,
            ph: menu.ph,
            scroll: menu.scroll,
            view_ph: menu.view_ph,
            lw: logical_dim(menu.pw, menu.metrics.scale),
            lh: logical_dim(menu.view_ph, menu.metrics.scale),
        })],
        fire: None,
    }
}

fn begin_menu(state: &mut MenuState) -> Outbox {
    let Some(generation) = state.generation else {
        return Outbox::default();
    };
    let Some(menu) = state.menu.as_ref() else {
        return Outbox::default();
    };
    let place = placement(menu);
    state.active = true;
    state.engaged = true;
    state.phase = Phase::AwaitMenu;
    Outbox {
        ops: vec![
            // Maps the popup invisibly, activating the grab before the menu
            // has pixels.
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
            SurfaceOp::Reposition { generation, place },
        ],
        fire: None,
    }
}

fn refresh_shown(state: &mut MenuState) -> Outbox {
    let Some(generation) = state.generation else {
        return Outbox::default();
    };
    let place = state
        .menu
        .as_ref()
        .filter(|m| m.layout.is_some())
        .map(placement);
    request_paint(state);
    Outbox {
        ops: place
            .map(|place| SurfaceOp::Reposition { generation, place })
            .into_iter()
            .collect(),
        fire: None,
    }
}

fn step(state: &mut MenuState, ev: MenuEvent) -> Outbox {
    let Some(menu) = state.menu.as_mut() else {
        return Outbox::default();
    };
    let Some(layout) = menu.layout.clone() else {
        return Outbox::default();
    };
    let items = Arc::clone(&menu.items);
    let effects = interaction_fsm::step(&mut menu.fsm, &ev, &layout, &items);
    if matches!(ev, MenuEvent::Key(_)) {
        scroll_active_into_view(menu);
    }
    let mut out = Outbox::default();
    for effect in effects {
        match effect {
            MenuEffect::Redraw => request_paint(state),
            MenuEffect::Close(id) => {
                let generation = state.generation;
                out.fire = clear_menu(state).map(|f| (f, id));
                out.ops
                    .extend(generation.map(|generation| SurfaceOp::Destroy {
                        generation,
                        reason: MenuClose::Finished,
                    }));
                return out;
            }
        }
    }
    out
}

fn clear_menu(state: &mut MenuState) -> Option<MenuSelectionFn> {
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

fn run(surface: Arc<dyn PopupSurface>, mailbox: Mailbox<MenuState>) {
    let mut fonts = Fonts::new();
    loop {
        let (job, shutdown) = mailbox.wait(
            |s| s.job.is_some() || s.shutdown,
            |s| (take_job(s), s.shutdown),
        );
        if shutdown {
            return;
        }
        let Some(job) = job else { continue };
        let out = match job {
            Job::Shape {
                generation,
                items,
                width,
            } => {
                let metrics = surface.metrics();
                let mut layout = render::layout(&mut fonts, &items, metrics.scale);
                if width > 0 {
                    layout.width = ((width as f32 * metrics.scale).round() as i32).max(1);
                }
                mailbox.update(|s| on_layout(s, generation, layout, metrics))
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
                mailbox.update(|s| on_pixels(s, generation, pixels))
            }
        };
        out.emit(&*surface);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
