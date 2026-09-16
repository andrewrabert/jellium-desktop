use std::ffi::c_void;
use std::num::NonZeroI32;

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use calloop::{EventLoop, LoopSignal, ping::PingSource};
use calloop_wayland_source::WaylandSource;
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::csd_frame::WindowState;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::window::{
    self as sctk_window, Window, WindowConfigure, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::{XdgShell, XdgSurface as _};
use smithay_client_toolkit::shm::slot::{Buffer as SlotBuffer, SlotPool};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_output::{Transform, WlOutput},
    wl_seat::WlSeat,
    wl_shm::WlShm,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::xdg_toplevel;
#[cfg(feature = "kde-palette")]
use wayland_protocols_plasma::server_decoration_palette::client::{
    org_kde_kwin_server_decoration_palette::OrgKdeKwinServerDecorationPalette,
    org_kde_kwin_server_decoration_palette_manager::OrgKdeKwinServerDecorationPaletteManager,
};

use jfn_platform_abi::{EffectiveDecorations, WindowDecorations};

use crate::input::SeatShared;
use crate::runtime::WlRuntime;
use crate::wl_state::{InitError, ShmGlobal, bind_error, new_slot_pool};

const APP_ID: &str = "net.nullsum.JelliumDesktop";
const TITLE: &str = "Jellium Desktop";

const BG: [u8; 3] = [0x10, 0x10, 0x10];

const DEFAULT_W: i32 = 1280;
const DEFAULT_H: i32 = 720;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
enum DecorationRequest {
    Auto = 0,
    ClientSide = 1,
    ServerSide = 2,
}

impl DecorationRequest {
    fn to_sctk(self) -> sctk_window::WindowDecorations {
        match self {
            Self::Auto => sctk_window::WindowDecorations::ServerDefault,
            Self::ClientSide => sctk_window::WindowDecorations::RequestClient,
            Self::ServerSide => sctk_window::WindowDecorations::RequestServer,
        }
    }
}

pub(crate) struct RootShared {
    decoration_request: Mutex<DecorationRequest>,
    effective: EffectiveState,
    boot: Mutex<BootGeometry>,
    started: AtomicBool,
    commands_tx: Sender<WindowCommand>,
    commands_rx: Receiver<WindowCommand>,
    pending_present: AtomicBool,
    root_surface: OnceLock<RootSurfaceHandle>,
    window: OnceLock<Window>,
    thread: OnceLock<RootThread>,
}

#[derive(Copy, Clone)]
struct BootGeometry {
    w: i32,
    h: i32,
    maximized: bool,
}

impl RootShared {
    pub(crate) fn new() -> Self {
        let (commands_tx, commands_rx) = unbounded();
        Self {
            decoration_request: Mutex::new(DecorationRequest::Auto),
            effective: EffectiveState(Mutex::new(EffectiveDecorations::ClientSide)),
            boot: Mutex::new(BootGeometry {
                w: DEFAULT_W,
                h: DEFAULT_H,
                maximized: false,
            }),
            started: AtomicBool::new(false),
            commands_tx,
            commands_rx,
            pending_present: AtomicBool::new(false),
            root_surface: OnceLock::new(),
            window: OnceLock::new(),
            thread: OnceLock::new(),
        }
    }

    fn decoration_request(&self) -> DecorationRequest {
        *self.decoration_request.lock()
    }

    pub(crate) fn set_decorations(&self, configured: Option<WindowDecorations>) {
        let request = match configured {
            None => DecorationRequest::Auto,
            Some(WindowDecorations::Csd) => DecorationRequest::ClientSide,
            Some(_) => DecorationRequest::ServerSide,
        };
        *self.decoration_request.lock() = request;
    }

    pub(crate) fn effective_decorations(&self) -> EffectiveDecorations {
        self.effective.load()
    }

    pub(crate) fn set_boot_geometry(&self, w: i32, h: i32, maximized: bool) {
        let mut boot = self.boot.lock();
        if let Some(size) = crate::window_state::WindowSize::new(w, h) {
            boot.w = size.w();
            boot.h = size.h();
        }
        boot.maximized = maximized;
    }

    fn boot_geometry(&self) -> BootGeometry {
        *self.boot.lock()
    }

    pub(crate) fn window(&self) -> Option<&Window> {
        self.window.get()
    }

    pub(crate) fn root_surface_handle(&self) -> Option<RootSurfaceHandle> {
        self.root_surface.get().copied()
    }

    fn wake(&self) {
        if let Some(t) = self.thread.get() {
            t.ping.ping();
        }
    }

    fn send(&self, cmd: WindowCommand) {
        let _ = self.commands_tx.send(cmd);
        self.wake();
    }

    pub(crate) fn start_move(&self, seat: &SeatShared) {
        self.send(WindowCommand::Move {
            serial: seat.last_button_serial(),
        });
    }

    pub(crate) fn start_resize(&self, seat: &SeatShared, edge: u32) {
        self.send(WindowCommand::Resize {
            serial: seat.last_button_serial(),
            edge,
        });
    }

    pub(crate) fn set_fullscreen(&self, on: bool) {
        self.send(WindowCommand::Fullscreen(ModeRequest::Set(on)));
    }

    pub(crate) fn toggle_fullscreen(&self) {
        self.send(WindowCommand::Fullscreen(ModeRequest::Toggle));
    }

    pub(crate) fn toggle_maximize(&self) {
        self.send(WindowCommand::Maximized(ModeRequest::Toggle));
    }

    pub(crate) fn set_minimized(&self) {
        self.send(WindowCommand::Minimize);
    }

    pub(crate) fn set_background_color(&self, r: u8, g: u8, b: u8) {
        self.send(WindowCommand::SetBackground([r, g, b]));
    }

    pub(crate) fn request_present(&self) {
        self.pending_present.store(true, Ordering::Release);
        self.wake();
    }

    #[cfg(feature = "kde-palette")]
    pub(crate) fn set_titlebar_palette(&self, path: &std::path::Path) {
        if let Some(s) = path.to_str() {
            self.send(WindowCommand::SetTitlebarPalette(s.to_owned()));
        }
    }
}

struct EffectiveState(Mutex<EffectiveDecorations>);

impl EffectiveState {
    fn load(&self) -> EffectiveDecorations {
        *self.0.lock()
    }

    fn store(&self, mode: EffectiveDecorations) -> bool {
        std::mem::replace(&mut *self.0.lock(), mode) != mode
    }
}

struct RootState {
    rt: &'static WlRuntime,
    registry_state: RegistryState,
    output_state: OutputState,
    conn: Connection,
    window: Window,
    decorations_negotiated: bool,
    seat: Option<WlSeat>,
    #[cfg(feature = "kde-palette")]
    palette: Option<OrgKdeKwinServerDecorationPalette>,
    shm_pool: Option<SlotPool>,
    viewport: WpViewport,
    bg_buffer: Option<SlotBuffer>,
    bg: [u8; 3],
    #[allow(dead_code)]
    frac_mgr: Option<WpFractionalScaleManagerV1>,
    #[allow(dead_code)]
    frac_scale: Option<WpFractionalScaleV1>,

    current_size: Option<crate::window_state::WindowSize>,
    pending_w: Option<NonZeroI32>,
    pending_h: Option<NonZeroI32>,
    mode: crate::window_state::WindowMode,
    suspended: bool,
    floating: FloatingRestore,
    pending_configure: Option<Presented>,
    present: Option<Presented>,
    pre_fs_maximized: bool,
    stop: Arc<AtomicBool>,
}

impl RootState {
    fn surface(&self) -> &WlSurface {
        self.window.wl_surface()
    }
}

const SCALE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

mod floating_restore {
    use crate::window_state::{WindowMode, WindowSize};

    #[derive(Clone, Copy)]
    pub(super) struct FloatingRestore(Option<WindowSize>);

    impl FloatingRestore {
        pub(super) const EMPTY: Self = Self(None);

        pub(super) fn size(self) -> Option<WindowSize> {
            self.0
        }

        pub(super) fn record(&mut self, mode: WindowMode, w: i32, h: i32) {
            if mode.uses_floating_restore() {
                self.0 = WindowSize::new(w, h);
            }
        }
    }
}
use floating_restore::FloatingRestore;

mod present_cap {
    use super::WindowConfigure;

    #[derive(Clone, Copy)]
    pub(super) struct Presented(());

    pub(super) fn acked(_: &WindowConfigure) -> Presented {
        Presented(())
    }
}
use present_cap::Presented;

mod presentation {
    use std::num::NonZeroI32;

    use crate::window_state::{WindowMode, WindowSize};

    #[derive(Clone, Copy)]
    pub(super) struct Inputs {
        pub(super) mapped: bool,
        pub(super) pending_configure: bool,
        pub(super) size: Option<WindowSize>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Step {
        Wait,
        Present,
    }

    pub(super) fn plan(i: Inputs) -> Step {
        if !i.pending_configure && !i.mapped {
            return Step::Wait;
        }
        if i.size.is_none() {
            return Step::Wait;
        }
        Step::Present
    }

    pub(super) fn resolve_logical_size(
        pending: (Option<NonZeroI32>, Option<NonZeroI32>),
        cur: Option<WindowSize>,
        floating: Option<WindowSize>,
        mode: WindowMode,
    ) -> Option<WindowSize> {
        let pick =
            |pending: Option<NonZeroI32>, cur: Option<i32>, floating: Option<i32>| -> Option<i32> {
                if let Some(p) = pending {
                    Some(p.get())
                } else if mode.uses_floating_restore() {
                    floating
                } else {
                    cur
                }
            };
        let w = pick(pending.0, cur.map(|s| s.w()), floating.map(|s| s.w()))?;
        let h = pick(pending.1, cur.map(|s| s.h()), floating.map(|s| s.h()))?;
        WindowSize::new(w, h)
    }
}
use presentation::resolve_logical_size;

impl RootState {
    fn resolve_logical(&self) -> Option<crate::window_state::WindowSize> {
        resolve_logical_size(
            (self.pending_w, self.pending_h),
            self.current_size,
            self.floating.size(),
            self.mode,
        )
    }

    fn try_present(&mut self) {
        let step = presentation::plan(presentation::Inputs {
            mapped: self.present.is_some(),
            pending_configure: self.pending_configure.is_some(),
            size: self.resolve_logical(),
        });
        match step {
            presentation::Step::Wait => {}
            presentation::Step::Present => self.execute_present(),
        }
    }

    fn execute_present(&mut self) {
        let Some(size) = self.resolve_logical() else {
            return;
        };
        let (w, h) = (size.w(), size.h());

        let first = self.present.is_none();
        let present = if let Some(p) = self.pending_configure.take() {
            self.present = Some(p);
            p
        } else if let Some(p) = self.present {
            p
        } else {
            return;
        };
        self.window.xdg_surface().set_window_geometry(0, 0, w, h);
        self.fill_background(w, h, present);
        self.current_size = Some(size);
        self.floating.record(self.mode, w, h);
        if first {
            tracing::info!(target: "Main", "root window: first configure {w}x{h} (app toplevel is live)");
        }

        self.rt.proxy().set_window_size(size);
        self.rt.window().publish(self.rt, size, self.mode);

        self.rt
            .root()
            .pending_present
            .store(true, Ordering::Release);
    }

    fn present_transaction(&mut self, _present: Presented) {
        self.surface().commit();
    }

    fn fill_background(&mut self, w: i32, h: i32, _present: Presented) {
        self.viewport.set_destination(w, h);
        if self.bg_buffer.is_none() {
            self.bg_buffer = self.create_solid_buffer();
            self.attach_background();
        }
        crate::wl_state::damage_all(self.surface());
    }

    fn rebuild_background(&mut self, w: i32, h: i32, _present: Presented) {
        let Some(new) = self.create_solid_buffer() else {
            return;
        };
        drop(self.bg_buffer.replace(new));
        self.attach_background();
        self.viewport.set_destination(w, h);
        crate::wl_state::damage_all(self.surface());
    }

    fn attach_background(&self) {
        let Some(buf) = self.bg_buffer.as_ref() else {
            return;
        };
        if let Err(e) = buf.attach_to(self.surface()) {
            tracing::error!(target: "Main", "root window: attach background: {e}");
        }
    }

    fn create_solid_buffer(&mut self) -> Option<SlotBuffer> {
        let bg = self.bg;
        crate::wl_state::draw_argb8888(self.shm_pool.as_mut()?, 1, 1, move |dst| {
            dst.copy_from_slice(&[bg[2], bg[1], bg[0], 0xFF]);
            true
        })
    }
}

#[derive(Copy, Clone)]
pub(crate) struct RootSurfaceHandle(std::ptr::NonNull<c_void>);

unsafe impl Send for RootSurfaceHandle {}
unsafe impl Sync for RootSurfaceHandle {}

impl RootSurfaceHandle {
    pub(crate) fn as_ptr(self) -> *mut c_void {
        self.0.as_ptr()
    }
}

enum WindowCommand {
    Move {
        serial: u32,
    },
    Resize {
        serial: u32,
        edge: u32,
    },
    Fullscreen(ModeRequest),
    Maximized(ModeRequest),
    Minimize,
    SetBackground([u8; 3]),
    #[cfg(feature = "kde-palette")]
    SetTitlebarPalette(String),
}

#[derive(Copy, Clone)]
enum ModeRequest {
    Set(bool),
    Toggle,
}

impl ModeRequest {
    fn resolve(self, current: bool) -> bool {
        match self {
            ModeRequest::Set(on) => on,
            ModeRequest::Toggle => !current,
        }
    }
}

fn apply_command(state: &mut RootState, cmd: WindowCommand) {
    use crate::window_state::WindowMode;
    match cmd {
        WindowCommand::Move { serial } => {
            if let Some(seat) = &state.seat {
                state.window.move_(seat, serial);
            } else {
                tracing::warn!(target: "Main", "interactive move dropped: no seat");
            }
        }
        WindowCommand::Resize { serial, edge } => {
            if let Some(seat) = &state.seat {
                match xdg_toplevel::ResizeEdge::try_from(edge) {
                    Ok(e) => state.window.resize(seat, serial, e),
                    Err(_) => {
                        tracing::warn!(target: "Main", "interactive resize dropped: bad edge {edge}");
                    }
                }
            } else {
                tracing::warn!(target: "Main", "interactive resize dropped: no seat");
            }
        }
        WindowCommand::Fullscreen(request) => {
            let on = request.resolve(matches!(state.mode, WindowMode::Fullscreen));
            apply_fullscreen(state, on);
        }
        WindowCommand::Maximized(request) => {
            if request.resolve(matches!(state.mode, WindowMode::Maximized)) {
                state.window.set_maximized();
            } else {
                state.window.unset_maximized();
            }
        }
        WindowCommand::Minimize => state.window.set_minimized(),
        WindowCommand::SetBackground(bg) => {
            if bg != state.bg {
                state.bg = bg;
                if let (Some(size), Some(present)) = (state.current_size, state.present) {
                    state.rebuild_background(size.w(), size.h(), present);
                    state.rt.root().request_present();
                }
            }
        }
        #[cfg(feature = "kde-palette")]
        WindowCommand::SetTitlebarPalette(path) => {
            if let Some(p) = &state.palette {
                p.set_palette(path);
            } else {
                tracing::warn!(target: "Main", "titlebar palette dropped: no palette manager");
            }
        }
    }
    let _ = state.conn.flush();
}

fn apply_fullscreen(state: &mut RootState, on: bool) {
    if on {
        if !matches!(state.mode, crate::window_state::WindowMode::Fullscreen) {
            state.pre_fs_maximized =
                matches!(state.mode, crate::window_state::WindowMode::Maximized);
        }
        state.window.set_fullscreen(None);
    } else {
        state.window.unset_fullscreen();
        if state.pre_fs_maximized {
            state.window.set_maximized();
            state.pre_fs_maximized = false;
        }
    }
    let _ = state.conn.flush();
}

struct RootThread {
    stop: Arc<AtomicBool>,
    ping: calloop::ping::Ping,
    handle: Mutex<Option<JoinHandle<()>>>,
}
pub(crate) fn cleanup(rt: &'static WlRuntime) {
    let Some(t) = rt.root().thread.get() else {
        return;
    };
    t.stop.store(true, Ordering::Relaxed);
    rt.root().wake();
    if let Some(h) = t.handle.lock().take() {
        let _ = h.join();
    }
}

fn vo_display(rt: &WlRuntime) -> Option<crate::app_conn::AppDisplay> {
    crate::app_conn::app_display(rt)
}

struct Required {
    compositor: CompositorState,
    shm: ShmGlobal,
    xdg_shell: XdgShell,
    viewporter: WpViewporter,
}

fn bind_required(
    globals: &wayland_client::globals::GlobalList,
    qh: &QueueHandle<RootState>,
) -> Result<Required, InitError> {
    Ok(Required {
        compositor: CompositorState::bind(globals, qh).map_err(bind_error("wl_compositor"))?,
        shm: ShmGlobal::new(globals.bind(qh, 1..=1, ()).map_err(bind_error("wl_shm"))?),
        xdg_shell: XdgShell::bind(globals, qh).map_err(bind_error("xdg_wm_base"))?,
        viewporter: globals
            .bind(qh, 1..=1, ())
            .map_err(bind_error("wp_viewporter"))?,
    })
}

fn has_decoration_manager(globals: &wayland_client::globals::GlobalList) -> bool {
    globals.contents().with_list(|list| {
        list.iter()
            .any(|g| g.interface == "zxdg_decoration_manager_v1")
    })
}

pub(crate) fn ensure_started(rt: &'static WlRuntime) {
    if rt.root().started.load(Ordering::Acquire) {
        return;
    }
    let Some(display) = vo_display(rt) else {
        return;
    };
    if rt.root().started.swap(true, Ordering::AcqRel) {
        return;
    }

    let backend =
        unsafe { wayland_backend::client::Backend::from_foreign_display(display.as_ptr().cast()) };
    let conn = Connection::from_backend(backend);
    let (globals, queue) = match registry_queue_init::<RootState>(&conn) {
        Ok(g) => g,
        Err(e) => {
            tracing::error!(target: "Main", "root window: {}", InitError::from(e));
            return;
        }
    };
    let qh = queue.handle();

    let Required {
        compositor,
        shm,
        xdg_shell,
        viewporter,
    } = match bind_required(&globals, &qh) {
        Ok(bound) => bound,
        Err(e) => {
            tracing::error!(target: "Main", "root window: {e}");
            return;
        }
    };
    let decoration_request = rt.root().decoration_request();
    let window = xdg_shell.create_window(
        compositor.create_surface(&qh),
        decoration_request.to_sctk(),
        &qh,
    );
    let surface = window.wl_surface().clone();
    if let Some(p) = std::ptr::NonNull::new(surface.id().as_ptr().cast()) {
        let _ = rt.root().root_surface.set(RootSurfaceHandle(p));
    }
    window.set_title(TITLE);
    window.set_app_id(APP_ID);

    let boot = rt.root().boot_geometry();
    let (boot_w, boot_h, boot_max) = (boot.w, boot.h, boot.maximized);
    if boot_max {
        window.set_maximized();
    }

    let viewport = viewporter.get_viewport(&surface, &qh, ());

    let FractionalScale {
        manager: frac_mgr,
        scale: frac_scale,
    } = bind_fractional_scale(rt, &globals, &qh, &surface);
    let decorations_negotiated = negotiate_decorations(rt, &globals, decoration_request);

    #[cfg(feature = "kde-palette")]
    let palette: Option<OrgKdeKwinServerDecorationPalette> = globals
        .bind::<OrgKdeKwinServerDecorationPaletteManager, _, _>(&qh, 1..=1, ())
        .ok()
        .map(|mgr| mgr.create(&surface, &qh, ()));

    let seat: Option<WlSeat> = globals.bind(&qh, 1..=8, ()).ok();

    window
        .xdg_surface()
        .set_window_geometry(0, 0, boot_w, boot_h);
    surface.commit();
    let _ = conn.flush();

    let _ = rt.root().window.set(window.clone());

    let (ping, stop_source) = match calloop::ping::make_ping() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "Main", "root window: ping: {e}");
            return;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));

    let state = RootState {
        rt,
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        conn: conn.clone(),
        window,
        decorations_negotiated,
        seat,
        #[cfg(feature = "kde-palette")]
        palette,
        shm_pool: new_slot_pool(&shm, "root window"),
        viewport,
        bg_buffer: None,
        bg: BG,
        frac_mgr,
        frac_scale,
        current_size: None,
        pending_w: None,
        pending_h: None,
        mode: crate::window_state::WindowMode::Floating,
        suspended: false,
        floating: {
            let mut f = FloatingRestore::EMPTY;
            f.record(crate::window_state::WindowMode::Floating, boot_w, boot_h);
            f
        },
        pending_configure: None,
        present: None,
        pre_fs_maximized: false,
        stop: stop.clone(),
    };

    spawn_root_thread(rt, conn, queue, state, stop, ping, stop_source);
}

struct FractionalScale {
    manager: Option<WpFractionalScaleManagerV1>,
    scale: Option<WpFractionalScaleV1>,
}

fn bind_fractional_scale(
    rt: &'static WlRuntime,
    globals: &wayland_client::globals::GlobalList,
    qh: &QueueHandle<RootState>,
    surface: &WlSurface,
) -> FractionalScale {
    let manager: Option<WpFractionalScaleManagerV1> = globals.bind(qh, 1..=1, ()).ok();
    let scale = manager
        .as_ref()
        .map(|m| m.get_fractional_scale(surface, qh, ()));
    if manager.is_none() {
        tracing::warn!(target: "Main", "root window: no wp_fractional_scale_manager_v1; no preferred_scale will arrive");
    }
    let probed = crate::scale_probe::probe_scale_bounded(
        crate::scale_probe::ProbeTarget::FirstOutput,
        SCALE_PROBE_TIMEOUT,
    );
    match (probed, manager.is_some()) {
        (Ok(scale), _) => rt.window().seed_scale(scale),
        (Err(e), true) => tracing::error!(
            target: "Main",
            "root window: no output stated a scale ({e}); waiting for preferred_scale"
        ),
        (Err(e), false) => {
            tracing::error!(
                target: "Main",
                "root window: no output stated a scale ({e}) and no wp_fractional_scale_manager_v1 to send one"
            );
            rt.window().resolve_unstated_scale();
        }
    }
    FractionalScale { manager, scale }
}

fn negotiate_decorations(
    rt: &'static WlRuntime,
    globals: &wayland_client::globals::GlobalList,
    request: DecorationRequest,
) -> bool {
    let negotiated = has_decoration_manager(globals);
    if !negotiated {
        if request == DecorationRequest::ServerSide {
            tracing::warn!(target: "Main", "root window: no zxdg_decoration_manager_v1; server-side requested, drawing no titlebar");
            if rt.root().effective.store(EffectiveDecorations::ServerSide) {
                jfn_platform_abi::notify_decorations_changed();
            }
        } else {
            tracing::warn!(target: "Main", "root window: no zxdg_decoration_manager_v1; client-side decorations");
        }
    }
    negotiated
}

fn spawn_root_thread(
    rt: &'static WlRuntime,
    conn: Connection,
    queue: EventQueue<RootState>,
    state: RootState,
    stop: Arc<AtomicBool>,
    ping: calloop::ping::Ping,
    stop_source: PingSource,
) {
    match thread::Builder::new()
        .name("wl-root".into())
        .spawn(move || root_loop(conn, queue, state, stop_source))
    {
        Ok(handle) => {
            let _ = rt.root().thread.set(RootThread {
                stop,
                ping,
                handle: Mutex::new(Some(handle)),
            });
        }
        Err(e) => {
            tracing::error!(target: "Main", "root window: thread spawn: {e}");
        }
    }
}

fn service_root_requests(state: &mut RootState) -> bool {
    let mut applied = false;
    let root: &'static RootShared = state.rt.root();
    for cmd in root.commands_rx.try_iter() {
        applied = true;
        apply_command(state, cmd);
    }
    applied
}

impl RootState {
    fn settle(&mut self) {
        loop {
            let mut progressed = false;
            progressed |= service_root_requests(self);
            if let Some(present) = self.present
                && self
                    .rt
                    .root()
                    .pending_present
                    .swap(false, Ordering::Acquire)
            {
                self.present_transaction(present);
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
        let _ = self.conn.flush();
    }
}

fn root_loop(
    conn: Connection,
    queue: EventQueue<RootState>,
    mut state: RootState,
    stop_source: PingSource,
) {
    let mut event_loop = match EventLoop::<RootState>::try_new() {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(target: "Main", "root window: event loop: {e}");
            state.rt.callbacks().close();
            return;
        }
    };
    let handle = event_loop.handle();
    let signal: LoopSignal = event_loop.get_signal();
    if let Err(e) = handle.insert_source(stop_source, move |(), (), state: &mut RootState| {
        if state.stop.load(Ordering::Relaxed) {
            signal.stop();
        }
    }) {
        tracing::error!(target: "Main", "root window: stop source: {e}");
        state.rt.callbacks().close();
        return;
    }
    let inserted = handle.insert_source(
        WaylandSource::new(conn, queue),
        |_, queue, state: &mut RootState| {
            let dispatched = queue.dispatch_pending(state)?;
            crate::wl_state::pump_events(state.rt);
            Ok(dispatched)
        },
    );
    if let Err(e) = inserted {
        tracing::error!(target: "Main", "root window: wayland source: {e}");
        state.rt.callbacks().close();
        return;
    }
    state.settle();
    if let Err(e) = event_loop.run(None, &mut state, RootState::settle) {
        tracing::error!(target: "Main", "root window: event loop: {e}");
    }
    state.rt.callbacks().close();
    state.bg_buffer = None;
}

impl CompositorHandler for RootState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }
}

impl RootState {
    fn report_output_refresh(&self, output: &WlOutput) {
        if let Some(refresh) = self
            .output_state
            .info(output)
            .and_then(|info| {
                info.modes
                    .iter()
                    .find(|m| m.current)
                    .map(|m| m.refresh_rate)
            })
            .filter(|mhz| *mhz > 0)
            && let Some(rate) = jfn_gpu_paint::RefreshRate::from_millihertz(refresh)
        {
            jfn_gpu_paint::report_refresh(jfn_gpu_paint::RefreshSource::OutputMode, rate);
        }
    }
}

impl OutputHandler for RootState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        self.report_output_refresh(&output);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        self.report_output_refresh(&output);
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl WindowHandler for RootState {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        jfn_playback::shutdown::jfn_shutdown_initiate();
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        let (w, h) = configure.new_size;
        self.pending_w = w.and_then(logical_extent);
        self.pending_h = h.and_then(logical_extent);

        self.mode = if configure.is_fullscreen() {
            crate::window_state::WindowMode::Fullscreen
        } else if configure.is_maximized() {
            crate::window_state::WindowMode::Maximized
        } else if configure.state.intersects(WindowState::TILED) {
            crate::window_state::WindowMode::Tiled
        } else {
            crate::window_state::WindowMode::Floating
        };

        let suspended = configure.state.contains(WindowState::SUSPENDED);
        if suspended != self.suspended {
            self.suspended = suspended;
            crate::window_state::feed_suspended(suspended);
        }

        if self.decorations_negotiated {
            let effective = match configure.decoration_mode {
                sctk_window::DecorationMode::Client => EffectiveDecorations::ClientSide,
                sctk_window::DecorationMode::Server => EffectiveDecorations::ServerSide,
            };
            if self.rt.root().effective.store(effective) {
                tracing::info!(target: "Main", "decorations: compositor set {effective:?}");
                jfn_platform_abi::notify_decorations_changed();
            }
        }

        self.pending_configure = Some(present_cap::acked(&configure));
        self.try_present();
    }
}

fn logical_extent(v: std::num::NonZeroU32) -> Option<NonZeroI32> {
    NonZeroI32::new(i32::try_from(v.get()).ok()?)
}

impl Dispatch<WpFractionalScaleV1, ()> for RootState {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let Some(scale) = crate::scale::Scale120::from_wire(scale) else {
                return;
            };
            state.rt.window().report_scale(scale);
            state.try_present();
        }
    }
}

macro_rules! noop_dispatch {
    ($($ty:ty),+ $(,)?) => {
        $(impl Dispatch<$ty, ()> for RootState {
            fn event(
                _: &mut Self,
                _: &$ty,
                _: <$ty as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {}
        })+
    };
}

noop_dispatch!(
    WlShm,
    WpViewporter,
    WpViewport,
    WpFractionalScaleManagerV1,
    WlSeat,
);

#[cfg(feature = "kde-palette")]
impl Dispatch<OrgKdeKwinServerDecorationPaletteManager, ()> for RootState {
    fn event(
        _: &mut Self,
        _: &OrgKdeKwinServerDecorationPaletteManager,
        _: <OrgKdeKwinServerDecorationPaletteManager as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(feature = "kde-palette")]
impl Dispatch<OrgKdeKwinServerDecorationPalette, ()> for RootState {
    fn event(
        _: &mut Self,
        _: &OrgKdeKwinServerDecorationPalette,
        _: <OrgKdeKwinServerDecorationPalette as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl ProvidesRegistryState for RootState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_dispatch2!(RootState);
delegate_registry!(RootState);

#[cfg(test)]
mod tests {
    use super::ModeRequest;
    use super::presentation::{Inputs, Step, plan};
    use super::resolve_logical_size;
    use crate::popup_protocol::popup_place::Placed;
    use crate::window_state::{WindowMode, WindowSize};
    use jfn_platform_abi::{
        LogicalPoint, LogicalSize, MenuPlacement, PhysicalSize, Scale, WindowExtent,
    };
    use std::num::NonZeroI32;

    #[test]
    fn set_ignores_the_current_mode_and_toggle_negates_it() {
        for current in [false, true] {
            assert!(ModeRequest::Set(true).resolve(current));
            assert!(!ModeRequest::Set(false).resolve(current));
            assert_eq!(ModeRequest::Toggle.resolve(current), !current);
        }
    }

    fn place(x: i32) -> MenuPlacement {
        let physical = PhysicalSize { w: 10, h: 10 };
        let Some(view) = WindowExtent::new(physical, Scale::ONE, LogicalSize { w: 10, h: 10 })
        else {
            unreachable!()
        };
        MenuPlacement {
            anchor: LogicalPoint { x, y: 0 },
            view,
        }
    }

    #[test]
    fn an_unmapped_popup_holds_a_placement_until_the_map() {
        let mut p = Placed::created(place(0));
        p.hold(place(1));
        p.hold(place(2));
        assert_eq!(p.on_map(), Some(place(2)));
    }

    #[test]
    fn a_held_placement_equal_to_the_create_never_reaches_the_wire() {
        let mut p = Placed::created(place(0));
        p.hold(place(1));
        p.hold(place(0));
        assert_eq!(p.on_map(), None);
    }

    #[test]
    fn a_mapped_popup_sends_only_a_changed_placement() {
        let mut p = Placed::created(place(0));
        assert_eq!(p.send(place(0)), None);
        assert_eq!(p.send(place(1)), Some(place(1)));
        assert_eq!(p.send(place(1)), None);
    }

    #[test]
    fn a_consumed_hold_is_not_replayed_by_a_later_map() {
        let mut p = Placed::created(place(0));
        p.hold(place(1));
        assert_eq!(p.on_map(), Some(place(1)));
        assert_eq!(p.on_map(), None);
        assert_eq!(p.send(place(1)), None);
    }

    fn inputs(mapped: bool, pending_configure: bool, size: bool) -> Inputs {
        Inputs {
            mapped,
            pending_configure,
            size: size.then(|| WindowSize::new(1280, 720)).flatten(),
        }
    }

    #[test]
    fn no_configure_and_unmapped_waits() {
        for size in [false, true] {
            assert_eq!(plan(inputs(false, false, size)), Step::Wait);
        }
    }

    #[test]
    fn unresolvable_size_waits() {
        assert_eq!(plan(inputs(false, true, false)), Step::Wait);
        assert_eq!(plan(inputs(true, false, false)), Step::Wait);
    }

    #[test]
    fn presents_once_configured_and_sized() {
        assert_eq!(plan(inputs(false, true, true)), Step::Present);
        assert_eq!(plan(inputs(true, true, true)), Step::Present);
        assert_eq!(plan(inputs(true, false, true)), Step::Present);
    }

    const NONE: (Option<NonZeroI32>, Option<NonZeroI32>) = (None, None);

    fn pending(w: i32, h: i32) -> (Option<NonZeroI32>, Option<NonZeroI32>) {
        (NonZeroI32::new(w), NonZeroI32::new(h))
    }

    fn size(w: i32, h: i32) -> Option<WindowSize> {
        WindowSize::new(w, h)
    }

    #[test]
    fn maximized_without_compositor_size_defers() {
        assert_eq!(
            resolve_logical_size(NONE, None, size(1280, 720), WindowMode::Maximized),
            None
        );
        assert_eq!(
            resolve_logical_size(NONE, None, size(1280, 720), WindowMode::Fullscreen),
            None
        );
    }

    #[test]
    fn tiled_defers_like_maximized_not_floating() {
        assert_eq!(
            resolve_logical_size(NONE, None, size(1280, 720), WindowMode::Tiled),
            None
        );
        assert!(!WindowMode::Tiled.uses_floating_restore());
    }

    #[test]
    fn floating_without_compositor_size_uses_floating() {
        assert_eq!(
            resolve_logical_size(NONE, None, size(1280, 720), WindowMode::Floating),
            size(1280, 720)
        );
    }

    #[test]
    fn unmaximize_uses_floating_not_stale_cur() {
        assert_eq!(
            resolve_logical_size(NONE, size(1920, 1080), size(800, 600), WindowMode::Floating),
            size(800, 600)
        );
    }

    #[test]
    fn compositor_size_wins_for_every_mode() {
        for mode in [
            WindowMode::Floating,
            WindowMode::Tiled,
            WindowMode::Maximized,
            WindowMode::Fullscreen,
        ] {
            assert_eq!(
                resolve_logical_size(pending(2560, 1440), size(800, 600), size(1280, 720), mode),
                size(2560, 1440)
            );
        }
    }

    #[test]
    fn last_completed_size_bridges_a_bare_configure() {
        assert_eq!(
            resolve_logical_size(
                NONE,
                size(2560, 1440),
                size(1280, 720),
                WindowMode::Maximized
            ),
            size(2560, 1440)
        );
    }
}
