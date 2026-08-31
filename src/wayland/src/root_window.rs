use std::ffi::c_void;
use std::num::NonZeroI32;

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};

use calloop::{EventLoop, LoopSignal, ping::PingSource};
use calloop_wayland_source::WaylandSource;
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Surface};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::csd_frame::WindowState;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use smithay_client_toolkit::shell::xdg::window::{
    self as sctk_window, Window, WindowConfigure, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::{XdgPositioner, XdgShell, XdgSurface as _};
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
use wayland_protocols::xdg::shell::client::{
    xdg_positioner::{Anchor, ConstraintAdjustment, Gravity},
    xdg_toplevel,
};
#[cfg(feature = "kde-palette")]
use wayland_protocols_plasma::server_decoration_palette::client::{
    org_kde_kwin_server_decoration_palette::OrgKdeKwinServerDecorationPalette,
    org_kde_kwin_server_decoration_palette_manager::OrgKdeKwinServerDecorationPaletteManager,
};

use jfn_platform_abi::{
    EffectiveDecorations, Generation, LogicalPoint, LogicalSize, MenuPaint, MenuPlacement,
    WindowDecorations,
};

use crate::input::SeatShared;
use crate::runtime::WlRuntime;
use crate::wl_state::{InitError, ShmGlobal, bind_error, new_slot_pool};

const APP_ID: &str = "net.nullsum.JelliumDesktop";
const TITLE: &str = "Jellium Desktop";

// Background behind the video/overlay, matching kBgColor (0x101010).
const BG: [u8; 3] = [0x10, 0x10, 0x10];

const DEFAULT_W: i32 = 1280;
const DEFAULT_H: i32 = 720;

/// The user's explicit decoration preference; `Auto` sends no `set_mode`, so
/// the compositor's preferred mode (delivered in the decoration configure)
/// decides.
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

/// The root window's cross-thread surface: everything the dispatch thread
/// shares with its requesters. The thread's own `RootState` stays on its stack.
pub(crate) struct RootShared {
    decoration_request: Mutex<DecorationRequest>,
    effective: EffectiveState,
    boot: Mutex<BootGeometry>,
    started: AtomicBool,
    commands_tx: Sender<WindowCommand>,
    commands_rx: Receiver<WindowCommand>,
    /// Coalesced request for one root commit; every producer that needs to
    /// present latches it and the root thread drains it once per pass.
    pending_present: AtomicBool,
    /// Protocol id of the most recent menu popup `wl_surface`, overwritten by
    /// each create and never cleared: the keyboard-leave that follows a
    /// teardown names the surface that is already gone, and the input thread
    /// must still read it as menu plumbing rather than real focus loss.
    menu_surface_id: AtomicU32,
    root_surface: OnceLock<RootSurfaceHandle>,
    /// The toplevel, parked for the life of the process. SCTK's `Window`
    /// destroys the root `wl_surface` when its last handle drops, and the CEF
    /// and mpv subsurfaces name that surface as their parent — so one handle
    /// must outlive the root thread's `RootState`.
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
            menu_surface_id: AtomicU32::new(0),
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

    pub(crate) fn menu_surface_id(&self) -> u32 {
        self.menu_surface_id.load(Ordering::Acquire)
    }

    pub(crate) fn root_surface_handle(&self) -> Option<RootSurfaceHandle> {
        self.root_surface.get().copied()
    }

    fn wake(&self) {
        if let Some(t) = self.thread.get() {
            t.ping.ping();
        }
    }

    /// Queue a request for the root thread and wake it. Sending and waking are
    /// one operation so a queued request can't sit unnoticed. The receiver is a
    /// sibling field of the leaked runtime, so the send never fails.
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

/// The decoration mode in effect. `ClientSide` until a decoration configure
/// — or, absent the decoration protocol, an explicit server-side request —
/// grants otherwise.
struct EffectiveState(Mutex<EffectiveDecorations>);

impl EffectiveState {
    fn load(&self) -> EffectiveDecorations {
        *self.0.lock()
    }

    /// Returns true when the stored value changed.
    fn store(&self, mode: EffectiveDecorations) -> bool {
        std::mem::replace(&mut *self.0.lock(), mode) != mode
    }
}

struct RootState {
    rt: &'static WlRuntime,
    registry_state: RegistryState,
    output_state: OutputState,
    conn: Connection,
    qh: QueueHandle<Self>,
    window: Window,
    decorations_negotiated: bool,
    // Single-owner protocol objects for window-control commands, owned by this
    // thread. `seat` also drives interactive move/resize grabs.
    seat: Option<WlSeat>,
    #[cfg(feature = "kde-palette")]
    palette: Option<OrgKdeKwinServerDecorationPalette>,
    shm_pool: Option<SlotPool>,
    compositor: CompositorState,
    xdg_shell: XdgShell,
    viewporter: WpViewporter,
    menu_pool: Option<SlotPool>,
    menu: MenuPopup,
    /// Highest menu generation ever created. Generations are handed out under
    /// the core lock but the creates carrying them are posted after that lock
    /// drops, so two menus racing can queue their creates out of order.
    armed_gen: u64,
    viewport: WpViewport,
    bg_buffer: Option<SlotBuffer>,
    bg: [u8; 3],
    // Held alive so the compositor keeps delivering preferred_scale.
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

/// Upper bound on the bring-up output probe: it round-trips on a second
/// display connection, which a wedged compositor can stall forever.
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

/// Buffer attach/commit take a [`Presented`], mintable only by [`acked`] from a
/// [`WindowConfigure`] — so "never commit a buffer before acking a configure" is
/// a type error rather than a review comment.
mod present_cap {
    use super::WindowConfigure;

    #[derive(Clone, Copy)]
    pub(super) struct Presented(());

    pub(super) fn acked(_: &WindowConfigure) -> Presented {
        Presented(())
    }
}
use present_cap::Presented;

/// Pure presentation state machine. Given what the root window currently
/// knows — mapped or not, pending configure or not, scale known or not, and
/// the resolvable logical size — [`presentation::plan`] decides the next step.
/// All Wayland I/O and cross-subsystem notifications stay in the effect layer
/// ([`RootState::try_present`] / [`RootState::execute_present`]).
mod presentation {
    use std::num::NonZeroI32;

    use crate::window_state::{WindowMode, WindowSize};

    /// Everything `plan` needs, free of protocol objects so it is unit-testable.
    #[derive(Clone, Copy)]
    pub(super) struct Inputs {
        pub(super) mapped: bool,
        pub(super) pending_configure: bool,
        pub(super) size: Option<WindowSize>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Step {
        /// Nothing presentable: no configure yet, or no resolvable size.
        Wait,
        /// Consume the pending configure (if any), update geometry, and request
        /// the root commit.
        Present,
    }

    pub(super) fn plan(i: Inputs) -> Step {
        // Never commit a buffer before a configure was acked (protocol
        // violation); before the first map that means waiting for one.
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

    /// Effect layer around the pure [`presentation::plan`]: gathers inputs and
    /// runs the decided step's Wayland I/O and notifications. May run inside an
    /// event callback, so it must never block.
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
        // Never commit the root here: the loop's latch drain issues the one root
        // commit that presents geometry with the overlay/video subtree.
        self.window.xdg_surface().set_window_geometry(0, 0, w, h);
        self.fill_background(w, h, present);
        self.current_size = Some(size);
        self.floating.record(self.mode, w, h);
        if first {
            tracing::info!(target: "Main", "root window: first configure {w}x{h} (app toplevel is live)");
        }

        // Pass logical (not physical) size: mpv and the overlay apply scale
        // themselves, so a physical size here would double-scale.
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
        // Build the replacement before retiring the current buffer so an
        // allocation failure leaves a valid buffer owned rather than none.
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
            // ARGB8888 little-endian byte order = [B, G, R, A].
            dst.copy_from_slice(&[bg[2], bg[1], bg[0], 0xFF]);
            true
        })
    }
}

/// Opaque handle to the app root `wl_surface`, carrying the live `wl_proxy`
/// pointer — the only representation valid across the two wayland-client
/// `Backend`s that share this one `wl_display` — so `wl_state` can rebuild the
/// surface under its own `Backend` via `ObjectId::from_ptr`.
#[derive(Copy, Clone)]
pub(crate) struct RootSurfaceHandle(std::ptr::NonNull<c_void>);

// Process-lifetime `wl_proxy` owned by the root thread; the handle only
// republishes it for reconstruction and never destroys it.
unsafe impl Send for RootSurfaceHandle {}
unsafe impl Sync for RootSurfaceHandle {}

impl RootSurfaceHandle {
    pub(crate) fn as_ptr(self) -> *mut c_void {
        self.0.as_ptr()
    }
}

// Window-control requests queued here and applied on the root thread by
// `apply_command`. The toplevel/seat proxies are single-owner and live on that
// thread, so requests cross this queue rather than caching proxy clones that
// could be used after teardown. Move/resize carry the input serial captured at
// request time. Mode toggles resolve against `RootState.mode` on that thread
// — its sole mutator/reader — so a configure can't flip the mode between the
// read and the protocol request.
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
    /// Applied on the root thread, which owns the surface, so the background
    /// rebuild lands in the single owner commit.
    SetBackground([u8; 3]),
    #[cfg(feature = "kde-palette")]
    SetTitlebarPalette(String),
    Popup(PopupCommand),
}

/// Menu-popup requests. Create, paint, reposition and destroy must reach the
/// compositor in the order they were issued, so they share one queue.
pub(crate) enum PopupCommand {
    Arm {
        generation: Generation,
        anchor: LogicalPoint,
        /// The press or key serial the grab cites. Captured on the input
        /// thread at request time; by the time this is applied the seat's last
        /// serial has moved on.
        serial: u32,
    },
    MapArmed {
        generation: Generation,
    },
    Reposition {
        generation: Generation,
        place: MenuPlacement,
    },
    Paint(MenuPaint),
    Destroy {
        generation: Generation,
    },
}

/// A requested window mode: explicit, or the opposite of the current one.
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
                // Not re-queued: the serial is only valid for the input event it
                // came from, so replaying it once a seat exists would be stale.
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
                // current_size is only set once presented, so the capability
                // is present too; requiring it keeps the buffer attach behind
                // an ack.
                if let (Some(size), Some(present)) = (state.current_size, state.present) {
                    state.rebuild_background(size.w(), size.h(), present);
                    // Apply via the single owner commit, not a standalone one.
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
        WindowCommand::Popup(cmd) => match cmd {
            PopupCommand::Arm {
                generation,
                anchor,
                serial,
            } => state.arm_menu_popup(generation, anchor, serial),
            PopupCommand::MapArmed { generation } => state.map_armed_menu_popup(generation),
            PopupCommand::Reposition { generation, place } => {
                state.reposition_menu_popup(generation, place);
            }
            PopupCommand::Paint(paint) => state.paint_menu_popup(paint),
            PopupCommand::Destroy { generation } => state.destroy_menu_popup(generation),
        },
    }
    let _ = state.conn.flush();
}

fn apply_fullscreen(state: &mut RootState, on: bool) {
    if on {
        // A fullscreen-enter received while already fullscreen must not overwrite
        // the saved restore mode, so capture it only when entering from another mode.
        if !matches!(state.mode, crate::window_state::WindowMode::Fullscreen) {
            state.pre_fs_maximized =
                matches!(state.mode, crate::window_state::WindowMode::Maximized);
        }
        state.window.set_fullscreen(None);
    } else {
        state.window.unset_fullscreen();
        // The compositor need not restore the pre-fullscreen maximized state, so
        // re-request it (the final mode is still confirmed via a configure).
        if state.pre_fs_maximized {
            state.window.set_maximized();
            state.pre_fs_maximized = false;
        }
    }
    let _ = state.conn.flush();
}

/// Placement bookkeeping for one menu popup, free of protocol objects: it owns
/// what the compositor has been given and what it may still be sent, so "never
/// reposition an unmapped popup" is decided here and nowhere else.
mod popup_place {
    use super::MenuPlacement;

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub(super) struct Placed {
        sent: Option<MenuPlacement>,
        held: Option<MenuPlacement>,
    }

    impl Placed {
        /// A popup armed with no menu on it: the compositor holds no placement.
        pub(super) fn armed() -> Placed {
            Placed {
                sent: None,
                held: None,
            }
        }

        #[cfg(test)]
        pub(super) fn created(sent: MenuPlacement) -> Placed {
            Placed {
                sent: Some(sent),
                held: None,
            }
        }

        /// What an unmapped popup wants next. A want equal to `sent` clears the
        /// hold, so a placement already on the wire is never re-sent.
        pub(super) fn hold(&mut self, want: MenuPlacement) {
            self.held = (Some(want) != self.sent).then_some(want);
        }

        /// The placement the mapping commit must be followed with, consumed as
        /// it is read. `None` when the create-time placement still stands.
        pub(super) fn on_map(&mut self) -> Option<MenuPlacement> {
            let place = self.held.take()?;
            self.sent = Some(place);
            Some(place)
        }

        /// The placement to put on the wire now; `None` when the compositor
        /// already holds it.
        pub(super) fn send(&mut self, want: MenuPlacement) -> Option<MenuPlacement> {
            (Some(want) != self.sent).then(|| {
                self.sent = Some(want);
                want
            })
        }
    }
}
use popup_place::Placed;

/// One live menu popup and everything that names its `wl_surface`.
struct LivePopup {
    generation: Generation,
    popup: Popup,
    viewport: WpViewport,
    place: Placed,
}

impl LivePopup {
    /// Crop, attach, damage and commit the menu buffer; the first one maps the
    /// popup.
    fn attach(&self, buffer: &crate::wl_state::AttachedBuffer, paint: &MenuPaint) {
        let surface = self.popup.wl_surface();
        self.viewport.set_source(
            0.0,
            f64::from(paint.scroll),
            f64::from(paint.buffer.w),
            f64::from(paint.view.physical().h),
        );
        self.viewport
            .set_destination(paint.view.logical().w, paint.view.logical().h);
        buffer.attach_to(surface);
        surface.damage_buffer(0, 0, paint.buffer.w, paint.buffer.h);
        surface.commit();
    }

    /// Attach [`ARMED_PIXEL`] and commit: the surface maps holding a grab and
    /// showing no menu. The viewport crops the one pixel to [`ARMED_SIZE`],
    /// the size the popup was configured at.
    fn attach_armed(&self, buffer: &crate::wl_state::AttachedBuffer) {
        let surface = self.popup.wl_surface();
        self.viewport.set_source(0.0, 0.0, 1.0, 1.0);
        self.viewport.set_destination(ARMED_SIZE.w, ARMED_SIZE.h);
        buffer.attach_to(surface);
        surface.damage_buffer(0, 0, 1, 1);
        surface.commit();
    }

    /// `xdg_popup.reposition`; every caller stands in a mapped state.
    fn reposition(&self, xdg_shell: &XdgShell, place: MenuPlacement) {
        let Some(positioner) = menu_positioner(xdg_shell, place.anchor, place.view.logical())
        else {
            return;
        };
        self.popup.reposition(&positioner, 0);
    }
}

// The viewport names the `wl_surface` that dropping the popup destroys, so it
// goes first.
impl Drop for LivePopup {
    fn drop(&mut self) {
        self.viewport.destroy();
    }
}

/// The menu popup's mapping state: `Unmapped` has no path to
/// [`LivePopup::reposition`], so a placement requested there is held until the
/// commit that maps the popup applies it.
#[derive(Default)]
enum MenuPopup {
    #[default]
    None,
    Unmapped {
        live: LivePopup,
    },
    Mapped {
        live: LivePopup,
        buffer: crate::wl_state::AttachedBuffer,
    },
}

impl MenuPopup {
    fn generation(&self) -> Option<Generation> {
        Some(self.live()?.generation)
    }

    fn live(&self) -> Option<&LivePopup> {
        match self {
            Self::None => None,
            Self::Unmapped { live } | Self::Mapped { live, .. } => Some(live),
        }
    }

    fn reposition(&mut self, xdg_shell: &XdgShell, want: MenuPlacement) {
        match self {
            Self::None => {}
            Self::Unmapped { live } => live.place.hold(want),
            Self::Mapped { live, .. } => {
                if let Some(place) = live.place.send(want) {
                    live.reposition(xdg_shell, place);
                }
            }
        }
    }

    /// Commits the armed buffer, mapping the popup so its grab takes effect,
    /// then sends whatever placement was held since the arm. A popup already
    /// mapped keeps the buffer it has.
    fn map_armed(&mut self, xdg_shell: &XdgShell, buffer: crate::wl_state::AttachedBuffer) {
        let mut live = match std::mem::take(self) {
            Self::None => return,
            Self::Unmapped { live } => live,
            Self::Mapped { live, buffer } => {
                *self = Self::Mapped { live, buffer };
                return;
            }
        };
        live.attach_armed(&buffer);
        if let Some(place) = live.place.on_map() {
            live.reposition(xdg_shell, place);
        }
        *self = Self::Mapped { live, buffer };
    }

    /// Commits `buffer`, mapping an unmapped popup and then sending whatever
    /// placement was held since the arm.
    fn paint(
        &mut self,
        xdg_shell: &XdgShell,
        buffer: crate::wl_state::AttachedBuffer,
        paint: &MenuPaint,
    ) {
        let (mut live, retired) = match std::mem::take(self) {
            Self::None => return,
            Self::Unmapped { live } => (live, None),
            Self::Mapped { live, buffer } => (live, Some(buffer)),
        };
        live.attach(&buffer, paint);
        // Retired only once the replacement is committed, so the surface is
        // never left naming a destroyed buffer.
        drop(retired);
        if let Some(place) = live.place.on_map() {
            live.reposition(xdg_shell, place);
        }
        *self = Self::Mapped { live, buffer };
    }
}

/// The smallest size `xdg_positioner.set_size` admits: it answers a width or
/// height that is not positive with `invalid_input`.
const ARMED_SIZE: LogicalSize = LogicalSize { w: 1, h: 1 };

/// One transparent premultiplied BGRA pixel: the buffer whose commit maps the
/// armed popup.
const ARMED_PIXEL: [u8; 4] = [0, 0, 0, 0];

/// The positioner for a surface of `size` logical px whose top-left sits at
/// `anchor`. The anchor rect is one pixel because the anchor is a point.
fn menu_positioner(
    xdg_shell: &XdgShell,
    anchor: LogicalPoint,
    size: LogicalSize,
) -> Option<XdgPositioner> {
    let p = XdgPositioner::new(xdg_shell)
        .inspect_err(|e| tracing::error!(target: "Main", "menu positioner: {e}"))
        .ok()?;
    p.set_size(size.w, size.h);
    p.set_anchor_rect(anchor.x, anchor.y, 1, 1);
    p.set_anchor(Anchor::TopLeft);
    p.set_gravity(Gravity::BottomRight);
    p.set_constraint_adjustment(
        ConstraintAdjustment::FlipX
            | ConstraintAdjustment::FlipY
            | ConstraintAdjustment::SlideX
            | ConstraintAdjustment::SlideY,
    );
    Some(p)
}

impl RootState {
    /// Creates the popup that holds the grab, with no menu on it, sized
    /// [`ARMED_SIZE`]. The grab cites the input thread's last press serial
    /// (button or key) — valid here only because every app connection shares
    /// one wl_client.
    fn arm_menu_popup(&mut self, generation: Generation, anchor: LogicalPoint, serial: u32) {
        // Each generation drives exactly one create, so `<=` (not `<`) also
        // blocks resurrecting a just-destroyed popup: teardown leaves armed_gen
        // at its peak.
        if generation.get() <= self.armed_gen {
            return;
        }
        self.armed_gen = generation.get();
        self.menu = MenuPopup::None;
        let Some(positioner) = menu_positioner(&self.xdg_shell, anchor, ARMED_SIZE) else {
            return;
        };
        self.open_menu_popup(generation, &positioner, serial, Placed::armed());
    }

    /// Creates the popup against `positioner` and takes the grab; `place` is
    /// what the compositor is thereafter considered to hold.
    fn open_menu_popup(
        &mut self,
        generation: Generation,
        positioner: &XdgPositioner,
        serial: u32,
        place: Placed,
    ) {
        let surface = match Surface::new(&self.compositor, &self.qh) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(target: "Main", "menu surface: {e}");
                return;
            }
        };
        let viewport = self
            .viewporter
            .get_viewport(surface.wl_surface(), &self.qh, ());
        // xdg_popup.grab is only honored before the popup's first commit, so
        // the grab and the commit below must stay in that order.
        let popup = match Popup::from_surface(
            Some(self.window.xdg_surface()),
            positioner,
            &self.qh,
            surface,
            &self.xdg_shell,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(target: "Main", "menu popup: {e}");
                viewport.destroy();
                return;
            }
        };
        if let Some(seat) = &self.seat {
            popup.xdg_popup().grab(seat, serial);
        }
        popup.wl_surface().commit();
        self.rt
            .root()
            .menu_surface_id
            .store(popup.wl_surface().id().protocol_id(), Ordering::Release);
        self.menu = MenuPopup::Unmapped {
            live: LivePopup {
                generation,
                popup,
                viewport,
                place,
            },
        };
    }

    /// Drops the request when `generation` no longer owns the popup.
    fn map_armed_menu_popup(&mut self, generation: Generation) {
        if self.menu.generation() != Some(generation) {
            return;
        }
        let Some(buffer) = self
            .menu_pool
            .as_mut()
            .and_then(|pool| crate::wl_state::draw_from_pixels(pool, &ARMED_PIXEL, 1, 1))
            .map(crate::wl_state::AttachedBuffer::Shm)
        else {
            tracing::error!(target: "Main", "menu: no buffer to map the armed popup with");
            return;
        };
        self.menu.map_armed(&self.xdg_shell, buffer);
    }

    fn reposition_menu_popup(&mut self, generation: Generation, place: MenuPlacement) {
        if self.menu.generation() != Some(generation) {
            return;
        }
        self.menu.reposition(&self.xdg_shell, place);
    }

    fn paint_menu_popup(&mut self, paint: MenuPaint) {
        if self.menu.generation() != Some(paint.generation) {
            return;
        }
        let Some(buffer) = self
            .menu_pool
            .as_mut()
            .and_then(|pool| {
                crate::wl_state::draw_from_pixels(
                    pool,
                    &paint.pixels,
                    paint.buffer.w,
                    paint.buffer.h,
                )
            })
            .map(crate::wl_state::AttachedBuffer::Shm)
        else {
            return;
        };
        self.menu.paint(&self.xdg_shell, buffer, &paint);
    }

    /// Tear the popup down, but only if `generation` still owns it — a newer
    /// menu may have taken the role in the gap between a stale teardown being
    /// decided and this call, and must not be torn down by it.
    fn destroy_menu_popup(&mut self, generation: Generation) {
        if self.menu.generation() != Some(generation) {
            return;
        }
        self.menu = MenuPopup::None;
    }

    fn menu_generation(&self, popup: &Popup) -> Option<Generation> {
        let live = self.menu.live()?;
        (&live.popup == popup).then_some(live.generation)
    }
}

pub(crate) fn popup(rt: &WlRuntime, cmd: PopupCommand) {
    rt.root().send(WindowCommand::Popup(cmd));
}

// The root `wl_surface.commit` is issued by exactly one owner — this dispatch
// thread. Every other producer (CEF paint paths, mpv) that needs to present
// requests it here, so geometry, overlay and video always land in one
// uninterruptible root commit; no other thread can commit the root between a
// geometry change and its children.
// Teardown handle for the dispatch thread. Without it the thread sleeps in
// calloop holding a `wl_display` read barrier; when no video ever played the
// display is quiet, so the barrier is never released and mpv's VO-teardown
// roundtrip hangs forever. `cleanup` signals + joins before that roundtrip.
struct RootThread {
    stop: Arc<AtomicBool>,
    ping: calloop::ping::Ping,
    handle: Mutex<Option<JoinHandle<()>>>,
}
/// Stop and join the dispatch thread, releasing its `wl_display` read barrier.
/// Must run before mpv's VO teardown, or that roundtrip deadlocks on the barrier.
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

/// Create the app-owned toplevel and start its dispatch thread. The toplevel
/// must exist before the VO-wait gate (which reads its size + scale), but the
/// mpv VO display it needs only appears mid-wait — so this is idempotent and
/// polled each tick until the display is available.
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
    // Publish the root wl_proxy so wl_state can parent its CEF overlay under this
    // surface: same libwayland wl_display, but a different wayland-client Backend,
    // so it must be reconstructed there via ObjectId::from_ptr.
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
    // Roleless commit (no buffer attached) to elicit the first
    // xdg_surface.configure — and, on compositors that send preferred_scale only
    // in response to a commit, the first scale. It must not be gated on scale:
    // xdg-shell requires this commit to obtain the configure that scale may
    // itself depend on.
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
        qh: qh.clone(),
        window,
        decorations_negotiated,
        seat,
        #[cfg(feature = "kde-palette")]
        palette,
        shm_pool: new_slot_pool(&shm, "root window"),
        compositor,
        xdg_shell,
        viewporter,
        menu_pool: new_slot_pool(&shm, "menu"),
        menu: MenuPopup::None,
        armed_gen: 0,
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

/// The fractional-scale objects for the root surface, when the compositor
/// offers the protocol.
struct FractionalScale {
    manager: Option<WpFractionalScaleManagerV1>,
    scale: Option<WpFractionalScaleV1>,
}

/// Binds fractional scale for `surface` and seeds the window's scale from the
/// first output that states one. Without a stated scale the window waits for
/// `preferred_scale`, or, where nothing can send one, resolves it itself.
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

/// Whether the decoration protocol will negotiate the mode. Without it a
/// server-side request is honored blind — no titlebar is drawn — and anything
/// else stays client-side.
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

// Apply queued window-control requests. Runs on the root thread each
// iteration before it blocks, so a request enqueued before the wake fd could
// ring is still serviced without waiting for another event.
fn service_root_requests(state: &mut RootState) -> bool {
    let mut applied = false;
    // Drained without a lock, so a command queued by an applied command's own
    // effects is serviced in this same pass.
    let root: &'static RootShared = state.rt.root();
    for cmd in root.commands_rx.try_iter() {
        applied = true;
        apply_command(state, cmd);
    }
    applied
}

impl RootState {
    /// Everything that must happen before the loop sleeps, repeated until it
    /// stops making progress: a step's effects (a fed scale raising the present
    /// latch, a command queued by a popup callback) are themselves work for the
    /// steps around it, so one pass can leave the state unsettled.
    fn settle(&mut self) {
        loop {
            let mut progressed = false;
            // Service queued control work before the sleep, not only after a
            // wake: the ping is a no-op until RootThread is published, so a
            // request stored during that startup window rings no fd and would
            // otherwise sleep here until an unrelated compositor event arrives.
            progressed |= service_root_requests(self);
            // Drain the latch before the sleep: an event handler (configure,
            // scale) that raised it during dispatch must commit now, or the loop
            // sleeps with the compositor still awaiting our commit. Gate on the
            // present capability so a pre-configure request stays latched, not
            // lost — swapping the latch only once we can present.
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

// The root queue is driven by calloop: `WaylandSource` owns the prepare_read /
// poll / read dance, which must coordinate with the other readers on the shared
// fd (a blocking dispatch here would deadlock them). A stop ping ends the loop
// so the `wl_display` read barrier is released at shutdown.
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
            // This thread is the sole reader of the shared display; the read
            // that woke us distributed events to every queue on it. Pump the CEF
            // overlay queue so its `wl_buffer.release` events are processed and
            // retired buffers get destroyed.
            crate::wl_state::pump_events(state.rt);
            Ok(dispatched)
        },
    );
    if let Err(e) = inserted {
        tracing::error!(target: "Main", "root window: wayland source: {e}");
        state.rt.callbacks().close();
        return;
    }
    // `run` calls its callback only after a dispatch, so settle once here or
    // work queued before the loop started would wait for the first event.
    state.settle();
    if let Err(e) = event_loop.run(None, &mut state, RootState::settle) {
        tracing::error!(target: "Main", "root window: event loop: {e}");
    }
    // This loop is the only dispatcher of compositor acknowledgements; once it
    // is gone none can ever resolve, so every waiter is released here.
    state.rt.callbacks().close();
    // Do not drain the bg's release here: this thread shares the wl_display fd
    // with the other readers, so a blocking roundtrip would deadlock them.
    state.bg_buffer = None;
}

/// Scaling is owned by `wp_fractional_scale_v1`, which SCTK does not implement,
/// so every compositor callback here is deliberately inert.
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
        // `wl_output`'s mode refresh is in mHz.
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

    /// SCTK has already acked the serial and coalesced the toplevel size,
    /// states, and decoration mode into `configure`.
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
            // Any single tiled edge means compositor-tiled; `is_tiled` demands
            // all four.
            crate::window_state::WindowMode::Tiled
        } else {
            crate::window_state::WindowMode::Floating
        };

        let suspended = configure.state.contains(WindowState::SUSPENDED);
        if suspended != self.suspended {
            self.suspended = suspended;
            crate::window_state::feed_suspended(suspended);
        }

        // Absent the decoration protocol SCTK reports its client-side default,
        // which would overwrite the boot-time decision.
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
            // Scale arrives without a configure (output change, or the first
            // scale completing a withheld configure), so drive a present here too.
            state.try_present();
        }
    }
}

impl PopupHandler for RootState {
    /// SCTK has already acked the serial.
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        _: PopupConfigure,
    ) {
        if let Some(generation) = self.menu_generation(popup) {
            self.rt.menu().on_ready(generation);
        }
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        let Some(generation) = self.menu_generation(popup) else {
            return;
        };
        // SCTK holds its own handle to `popup` for the length of this call, so
        // the teardown the menu emits has to reach the queue, not the popup.
        self.rt.menu().on_done(generation);
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
    use super::popup_place::Placed;
    use super::presentation::{Inputs, Step, plan};
    use super::resolve_logical_size;
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
        // The consumed hold is now what the compositor holds.
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
        // Whatever else is known, nothing may happen before the first configure.
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
        // Re-present without a new configure (size change after map).
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
        // Tiled is compositor-dictated: without a compositor size it must defer,
        // not fall back to the saved floating size.
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
