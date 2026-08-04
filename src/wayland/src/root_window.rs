use std::ffi::c_void;
use std::num::{NonZeroI32, NonZeroU64};

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
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

use jfn_platform_abi::{EffectiveDecorations, WindowDecorations};

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
    decoration_request: AtomicU8,
    effective: EffectiveState,
    boot: Mutex<BootGeometry>,
    started: AtomicBool,
    scale_fallback_fed: AtomicBool,
    commands_tx: Sender<WindowCommand>,
    commands_rx: Receiver<WindowCommand>,
    pending_fs: AtomicU8,
    maximized: AtomicBool,
    pending_bg: AtomicU32,
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
            decoration_request: AtomicU8::new(DecorationRequest::Auto as u8),
            effective: EffectiveState(AtomicU8::new(0)),
            boot: Mutex::new(BootGeometry {
                w: DEFAULT_W,
                h: DEFAULT_H,
                maximized: false,
            }),
            started: AtomicBool::new(false),
            scale_fallback_fed: AtomicBool::new(false),
            commands_tx,
            commands_rx,
            pending_fs: AtomicU8::new(FS_NONE),
            maximized: AtomicBool::new(false),
            pending_bg: AtomicU32::new(0),
            pending_present: AtomicBool::new(false),
            menu_surface_id: AtomicU32::new(0),
            root_surface: OnceLock::new(),
            window: OnceLock::new(),
            thread: OnceLock::new(),
        }
    }

    fn decoration_request(&self) -> DecorationRequest {
        match self.decoration_request.load(Ordering::Acquire) {
            v if v == DecorationRequest::ClientSide as u8 => DecorationRequest::ClientSide,
            v if v == DecorationRequest::ServerSide as u8 => DecorationRequest::ServerSide,
            _ => DecorationRequest::Auto,
        }
    }

    pub(crate) fn set_decorations(&self, configured: Option<WindowDecorations>) {
        let request = match configured {
            None => DecorationRequest::Auto,
            Some(WindowDecorations::Csd) => DecorationRequest::ClientSide,
            Some(_) => DecorationRequest::ServerSide,
        };
        self.decoration_request
            .store(request as u8, Ordering::Release);
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
        self.pending_fs
            .store(if on { FS_ON } else { FS_OFF }, Ordering::Release);
        self.wake();
    }

    pub(crate) fn toggle_fullscreen(&self) {
        self.pending_fs.store(FS_TOGGLE, Ordering::Release);
        self.wake();
    }

    pub(crate) fn set_maximized(&self, on: bool) {
        self.send(WindowCommand::SetMaximized(on));
    }

    pub(crate) fn toggle_maximize(&self) {
        let next = !self.maximized.load(Ordering::Relaxed);
        self.maximized.store(next, Ordering::Relaxed);
        self.set_maximized(next);
    }

    pub(crate) fn sync_maximized_command_state(&self, maximized: bool) {
        self.maximized.store(maximized, Ordering::Relaxed);
    }

    pub(crate) fn set_minimized(&self) {
        self.send(WindowCommand::Minimize);
    }

    pub(crate) fn set_background_color(&self, r: u8, g: u8, b: u8) {
        let rgb = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
        self.pending_bg.store(BG_SET | rgb, Ordering::Release);
        self.wake();
    }

    fn pending_bg(&self) -> Option<[u8; 3]> {
        let v = self.pending_bg.load(Ordering::Acquire);
        (v & BG_SET != 0).then_some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
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
struct EffectiveState(AtomicU8);

impl EffectiveState {
    fn encode(mode: EffectiveDecorations) -> u8 {
        match mode {
            EffectiveDecorations::ClientSide => 0,
            EffectiveDecorations::ServerSide => 1,
        }
    }

    fn load(&self) -> EffectiveDecorations {
        if self.0.load(Ordering::Acquire) == Self::encode(EffectiveDecorations::ServerSide) {
            EffectiveDecorations::ServerSide
        } else {
            EffectiveDecorations::ClientSide
        }
    }

    /// Returns true when the stored value changed.
    fn store(&self, mode: EffectiveDecorations) -> bool {
        self.0.swap(Self::encode(mode), Ordering::AcqRel) != Self::encode(mode)
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
    viewporter: Option<WpViewporter>,
    menu_pool: Option<SlotPool>,
    menu: MenuPopup,
    /// Highest menu generation ever created. Generations are handed out under
    /// the core lock but the creates carrying them are posted after that lock
    /// drops, so two menus racing can queue their creates out of order.
    armed_gen: u64,
    viewport: Option<WpViewport>,
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
    scale_discovery: ScaleDiscovery,
    pre_fs_maximized: bool,
    stop: Arc<AtomicBool>,
}

impl RootState {
    fn surface(&self) -> &WlSurface {
        self.window.wl_surface()
    }
}

/// Upper bound on the fallback probe: it round-trips on a second display
/// connection, which a wedged compositor can stall forever.
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
        pub(super) scale_known: bool,
        pub(super) size: Option<WindowSize>,
    }

    /// Progress of the deferred first-configure scale fallback.
    ///
    /// Ordering, per compositor style:
    /// - Normal compositors send `preferred_scale` before or alongside the
    ///   first configure. The scale is known when the configure dispatches,
    ///   `plan` never returns `DiscoverScale`, and this stays `Idle`.
    /// - Hyprland-style compositors withhold `preferred_scale` until the
    ///   window maps — which never happens while the first buffer waits on the
    ///   scale. The configure handler then requests discovery (`Requested`),
    ///   but the probe is NOT run inside the callback: the root loop first
    ///   finishes dispatching the current event batch, so a `preferred_scale`
    ///   queued later in the same batch wins and dissolves the request. Only
    ///   if the scale is still unknown after the drain does the loop spawn the
    ///   bounded off-thread probe (`Spawned`), which feeds a provisional scale
    ///   (or the unit fallback) back through `window_state` and wakes the root
    ///   thread to present. The authoritative `preferred_scale` corrects it
    ///   after map.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum ScaleDiscovery {
        Idle,
        Requested,
        Spawned,
    }

    impl ScaleDiscovery {
        /// `plan` chose [`Step::DiscoverScale`] inside an event callback: only
        /// note the request; never probe here.
        pub(super) fn request(self) -> Self {
            match self {
                Self::Idle => Self::Requested,
                other => other,
            }
        }

        /// The event batch is drained: decide whether the probe must actually
        /// run. A scale that arrived meanwhile dissolves the request; a spawned
        /// probe is never re-spawned.
        pub(super) fn after_batch_drained(self, scale_known: bool) -> (Self, bool) {
            match self {
                Self::Requested if scale_known => (Self::Idle, false),
                Self::Requested => (Self::Spawned, true),
                other => (other, false),
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Step {
        /// Nothing presentable: no configure yet, or no resolvable size.
        Wait,
        /// A first configure is waiting but no scale is known; scale discovery
        /// must run, then planning re-runs.
        DiscoverScale,
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
        if !i.scale_known {
            // Scale can only be missing before the first map; after map the
            // compositor has spoken (or the fallback already fed one).
            return if i.mapped {
                Step::Wait
            } else {
                Step::DiscoverScale
            };
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
use presentation::{ScaleDiscovery, resolve_logical_size};

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
    /// event callback, so it must never block — scale discovery is only
    /// requested here and serviced by the root loop between dispatch batches.
    fn try_present(&mut self) {
        let step = presentation::plan(presentation::Inputs {
            mapped: self.present.is_some(),
            pending_configure: self.pending_configure.is_some(),
            scale_known: self.rt.window().known_scale().is_some(),
            size: self.resolve_logical(),
        });
        match step {
            presentation::Step::Wait => {}
            presentation::Step::DiscoverScale => {
                self.scale_discovery = self.scale_discovery.request();
            }
            presentation::Step::Present => self.execute_present(),
        }
    }

    /// Runs on the root loop after `dispatch_pending` has drained the current
    /// event batch. If a `preferred_scale` arrived later in that batch the
    /// request dissolves; otherwise spawn the bounded off-thread probe. See
    /// [`ScaleDiscovery`] for the full ordering contract.
    fn service_scale_discovery(&mut self) {
        let (next, spawn) = self
            .scale_discovery
            .after_batch_drained(self.rt.window().known_scale().is_some());
        self.scale_discovery = next;
        if !spawn {
            return;
        }
        let rt = self.rt;
        let spawned = thread::Builder::new()
            .name("wl-scale-fallback".into())
            .spawn(move || {
                match crate::scale_probe::probe_scale_bounded(
                    crate::scale_probe::ProbeTarget::FirstOutput,
                    SCALE_PROBE_TIMEOUT,
                ) {
                    Ok(scale) => {
                        tracing::info!(
                            target: "Main",
                            "root window: no preferred_scale before first configure; using probed scale {scale}"
                        );
                        rt.window()
                            .feed_scale(scale, crate::window_state::ScaleProvenance::Provisional);
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "Main",
                            "root window: no preferred_scale before first configure and probe failed ({e}); assuming scale 1.0"
                        );
                        rt.window().feed_unit_scale();
                    }
                }
                rt.root().scale_fallback_fed.store(true, Ordering::Release);
                rt.root().wake();
            })
            .is_ok();
        if !spawned {
            // No thread, no probe: unblock presentation with the unit fallback.
            self.rt.window().feed_unit_scale();
            self.try_present();
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
        if let Some(vp) = &self.viewport {
            vp.set_destination(w, h);
        }
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
        if let Some(vp) = &self.viewport {
            vp.set_destination(w, h);
        }
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
// request time.
enum WindowCommand {
    Move {
        serial: u32,
    },
    Resize {
        serial: u32,
        edge: u32,
    },
    SetMaximized(bool),
    Minimize,
    #[cfg(feature = "kde-palette")]
    SetTitlebarPalette(String),
    Popup(PopupCommand),
}

/// Menu-popup requests. Create, paint, reposition and destroy must reach the
/// compositor in the order they were issued — a reposition ahead of the paint
/// that maps the popup is a protocol error — so they share one queue.
enum PopupCommand {
    Create {
        generation: NonZeroU64,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        /// The press or key serial the grab cites. Captured on the input
        /// thread at request time; by the time this is applied the seat's last
        /// serial has moved on.
        serial: u32,
    },
    Reposition {
        generation: NonZeroU64,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    Paint(PopupPaint),
    Destroy {
        generation: NonZeroU64,
    },
}

pub(crate) struct PopupPaint {
    pub(crate) generation: NonZeroU64,
    /// BGRA, `pw` x `ph`.
    pub(crate) pixels: Vec<u8>,
    pub(crate) pw: i32,
    pub(crate) ph: i32,
    /// Scroll offset into the buffer, physical px.
    pub(crate) scroll: i32,
    /// Visible height of the crop, physical px.
    pub(crate) view_ph: i32,
    pub(crate) lw: i32,
    pub(crate) lh: i32,
}

fn apply_command(state: &mut RootState, cmd: WindowCommand) {
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
        WindowCommand::SetMaximized(on) => {
            if on {
                state.window.set_maximized();
            } else {
                state.window.unset_maximized();
            }
        }
        WindowCommand::Minimize => state.window.set_minimized(),
        #[cfg(feature = "kde-palette")]
        WindowCommand::SetTitlebarPalette(path) => {
            if let Some(p) = &state.palette {
                p.set_palette(path);
            } else {
                tracing::warn!(target: "Main", "titlebar palette dropped: no palette manager");
            }
        }
        WindowCommand::Popup(cmd) => match cmd {
            PopupCommand::Create {
                generation,
                x,
                y,
                w,
                h,
                serial,
            } => state.create_menu_popup(generation, x, y, w, h, serial),
            PopupCommand::Reposition {
                generation,
                x,
                y,
                w,
                h,
            } => state.reposition_menu_popup(generation, x, y, w, h),
            PopupCommand::Paint(paint) => state.paint_menu_popup(paint),
            PopupCommand::Destroy { generation } => state.destroy_menu_popup(generation),
        },
    }
    let _ = state.conn.flush();
}

// Fullscreen requests posted here and applied on the root thread by
// `apply_fullscreen`. The mode read and the protocol request must stay on that
// thread — the sole mutator/reader of `RootState.mode` — so a configure can't
// flip the mode between them and make toggle send the wrong command.
const FS_NONE: u8 = 0;
const FS_TOGGLE: u8 = 1;
const FS_ON: u8 = 2;
const FS_OFF: u8 = 3;
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

/// SCTK's `Popup` destroys the menu `wl_surface` when it drops, so the
/// viewport and buffer naming that surface live and die with it.
#[derive(Default)]
struct MenuPopup {
    popup: Option<Popup>,
    viewport: Option<WpViewport>,
    buffer: Option<crate::wl_state::AttachedBuffer>,
    generation: Option<NonZeroU64>,
}

fn build_menu_positioner(
    xdg_shell: &XdgShell,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Option<XdgPositioner> {
    let p = XdgPositioner::new(xdg_shell)
        .inspect_err(|e| tracing::error!(target: "Main", "menu positioner: {e}"))
        .ok()?;
    p.set_size(w.max(1), h.max(1));
    p.set_anchor_rect(x, y, 1, 1);
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
    /// Create the grab popup, replacing whatever menu still stands. The grab
    /// cites the input thread's last press serial (button or key) — valid here
    /// only because every app connection shares one wl_client.
    fn create_menu_popup(
        &mut self,
        generation: NonZeroU64,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        serial: u32,
    ) {
        // Each generation drives exactly one create, so `<=` (not `<`) also
        // blocks resurrecting a just-destroyed popup: teardown leaves armed_gen
        // at its peak.
        if generation.get() <= self.armed_gen {
            return;
        }
        self.armed_gen = generation.get();
        self.tear_down_menu_popup();
        let Some(positioner) = build_menu_positioner(&self.xdg_shell, x, y, w, h) else {
            return;
        };
        let surface = match Surface::new(&self.compositor, &self.qh) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(target: "Main", "menu surface: {e}");
                return;
            }
        };
        let viewport = self
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(surface.wl_surface(), &self.qh, ()));
        // xdg_popup.grab is only honored before the popup's first commit, so
        // the grab and the commit below must stay in that order.
        let popup = match Popup::from_surface(
            Some(self.window.xdg_surface()),
            &positioner,
            &self.qh,
            surface,
            &self.xdg_shell,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(target: "Main", "menu popup: {e}");
                if let Some(vp) = viewport {
                    vp.destroy();
                }
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
        self.menu = MenuPopup {
            popup: Some(popup),
            viewport,
            buffer: None,
            generation: Some(generation),
        };
    }

    /// Requires the popup to already be mapped.
    fn reposition_menu_popup(&mut self, generation: NonZeroU64, x: i32, y: i32, w: i32, h: i32) {
        if self.menu.generation != Some(generation) {
            return;
        }
        let Some(popup) = self.menu.popup.as_ref() else {
            return;
        };
        let Some(positioner) = build_menu_positioner(&self.xdg_shell, x, y, w, h) else {
            return;
        };
        popup.reposition(&positioner, 0);
    }

    fn paint_menu_popup(&mut self, paint: PopupPaint) {
        if self.menu.generation != Some(paint.generation) {
            return;
        }
        let buf = self
            .menu_pool
            .as_mut()
            .and_then(|pool| {
                crate::wl_state::draw_from_pixels(pool, &paint.pixels, paint.pw, paint.ph)
            })
            .map(crate::wl_state::AttachedBuffer::Shm);
        let (Some(buf), Some(popup)) = (buf, self.menu.popup.as_ref()) else {
            return;
        };
        let surface = popup.wl_surface();
        if let Some(vp) = self.menu.viewport.as_ref() {
            vp.set_source(
                0.0,
                f64::from(paint.scroll),
                f64::from(paint.pw),
                f64::from(paint.view_ph),
            );
            vp.set_destination(paint.lw, paint.lh);
        }
        buf.attach_to(surface);
        surface.damage_buffer(0, 0, paint.pw, paint.ph);
        surface.commit();
        drop(self.menu.buffer.replace(buf));
    }

    /// Tear the popup down, but only if `generation` still owns it — a newer
    /// menu may have taken the role in the gap between a stale teardown being
    /// decided and this call, and must not be torn down by it.
    fn destroy_menu_popup(&mut self, generation: NonZeroU64) {
        if self.menu.generation != Some(generation) {
            return;
        }
        self.tear_down_menu_popup();
    }

    /// Unqualified teardown, safe only behind the `armed_gen` guard; every
    /// other caller goes through `destroy_menu_popup`.
    fn tear_down_menu_popup(&mut self) {
        // The viewport names the wl_surface that dropping the popup destroys,
        // so it goes first.
        if let Some(vp) = self.menu.viewport.take() {
            vp.destroy();
        }
        self.menu = MenuPopup::default();
    }

    fn menu_generation(&self, popup: &Popup) -> Option<NonZeroU64> {
        if self.menu.popup.as_ref()? != popup {
            return None;
        }
        self.menu.generation
    }
}

pub(crate) fn popup_create(rt: &WlRuntime, generation: NonZeroU64, x: i32, y: i32, w: i32, h: i32) {
    rt.root().send(WindowCommand::Popup(PopupCommand::Create {
        generation,
        x,
        y,
        w,
        h,
        serial: rt.seat().last_input_serial(),
    }));
}

pub(crate) fn popup_reposition(
    rt: &WlRuntime,
    generation: NonZeroU64,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    rt.root()
        .send(WindowCommand::Popup(PopupCommand::Reposition {
            generation,
            x,
            y,
            w,
            h,
        }));
}

pub(crate) fn popup_paint(rt: &WlRuntime, paint: PopupPaint) {
    rt.root()
        .send(WindowCommand::Popup(PopupCommand::Paint(paint)));
}

pub(crate) fn popup_destroy(rt: &WlRuntime, generation: NonZeroU64) {
    rt.root()
        .send(WindowCommand::Popup(PopupCommand::Destroy { generation }));
}

// High bit marks "set"; the low 24 bits are RGB. Applied on the dispatch thread,
// which owns the surface, so commits don't race the configure handler.
const BG_SET: u32 = 1 << 24;

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
}

fn bind_required(
    globals: &wayland_client::globals::GlobalList,
    qh: &QueueHandle<RootState>,
) -> Result<Required, InitError> {
    Ok(Required {
        compositor: CompositorState::bind(globals, qh).map_err(bind_error("wl_compositor"))?,
        shm: ShmGlobal::new(globals.bind(qh, 1..=1, ()).map_err(bind_error("wl_shm"))?),
        xdg_shell: XdgShell::bind(globals, qh).map_err(bind_error("xdg_wm_base"))?,
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
    } = match bind_required(&globals, &qh) {
        Ok(bound) => bound,
        Err(e) => {
            tracing::error!(target: "Main", "root window: {e}");
            return;
        }
    };
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();

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

    let viewport = viewporter
        .as_ref()
        .map(|vp| vp.get_viewport(&surface, &qh, ()));
    if viewport.is_none() {
        tracing::warn!(target: "Main", "root window: no wp_viewporter; background unscaled");
    }

    let frac_mgr: Option<WpFractionalScaleManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let frac_scale = frac_mgr
        .as_ref()
        .map(|m| m.get_fractional_scale(&surface, &qh, ()));
    if frac_mgr.is_none() {
        // No preferred_scale will ever arrive, so satisfy the boot scale gate —
        // otherwise it waits forever.
        tracing::warn!(target: "Main", "root window: no wp_fractional_scale_manager_v1; assuming scale 1.0");
        rt.window().feed_unit_scale();
    }

    let decorations_negotiated = has_decoration_manager(&globals);
    if !decorations_negotiated {
        if decoration_request == DecorationRequest::ServerSide {
            tracing::warn!(target: "Main", "root window: no zxdg_decoration_manager_v1; server-side requested, drawing no titlebar");
            if rt.root().effective.store(EffectiveDecorations::ServerSide) {
                jfn_platform_abi::notify_decorations_changed();
            }
        } else {
            tracing::warn!(target: "Main", "root window: no zxdg_decoration_manager_v1; client-side decorations");
        }
    }

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
        menu: MenuPopup::default(),
        armed_gen: 0,
        viewport,
        bg_buffer: None,
        bg: rt.root().pending_bg().unwrap_or(BG),
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
        scale_discovery: ScaleDiscovery::Idle,
        pre_fs_maximized: false,
        stop: stop.clone(),
    };

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

// Apply queued fullscreen / window-control / background-color requests. Runs on
// the root thread each iteration before it blocks, so a request enqueued before
// the wake fd could ring is still serviced without waiting for another event.
fn service_root_requests(state: &mut RootState) -> bool {
    let mut applied = false;
    match state.rt.root().pending_fs.swap(FS_NONE, Ordering::Acquire) {
        FS_ON => {
            apply_fullscreen(state, true);
            applied = true;
        }
        FS_OFF => {
            apply_fullscreen(state, false);
            applied = true;
        }
        FS_TOGGLE => {
            let on = !matches!(state.mode, crate::window_state::WindowMode::Fullscreen);
            apply_fullscreen(state, on);
            applied = true;
        }
        _ => {}
    }
    // Drained without a lock, so a command queued by an applied command's own
    // effects is serviced in this same pass.
    let root: &'static RootShared = state.rt.root();
    for cmd in root.commands_rx.try_iter() {
        applied = true;
        apply_command(state, cmd);
    }
    if let Some(bg) = state.rt.root().pending_bg()
        && bg != state.bg
    {
        state.bg = bg;
        applied = true;
        // current_size is only set once presented, so the capability is present
        // too; requiring it keeps the buffer attach behind an ack.
        if let (Some(size), Some(present)) = (state.current_size, state.present) {
            let (w, h) = (size.w(), size.h());
            state.rebuild_background(w, h, present);
            // Apply via the single owner commit, not a standalone one.
            state
                .rt
                .root()
                .pending_present
                .store(true, Ordering::Release);
        }
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
            if self
                .rt
                .root()
                .scale_fallback_fed
                .swap(false, Ordering::AcqRel)
            {
                self.try_present();
                progressed = true;
            }
            // The batch is drained: a preferred_scale queued behind the
            // configure has dispatched by now, so only a genuinely absent scale
            // spawns the fallback probe.
            self.service_scale_discovery();
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
        return;
    }
    // `run` calls its callback only after a dispatch, so settle once here or
    // work queued before the loop started would wait for the first event.
    state.settle();
    if let Err(e) = event_loop.run(None, &mut state, RootState::settle) {
        tracing::error!(target: "Main", "root window: event loop: {e}");
    }
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

impl OutputHandler for RootState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
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
            state
                .rt
                .window()
                .feed_scale(scale, crate::window_state::ScaleProvenance::Authoritative);
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
            crate::popup::on_ready(self.rt, generation);
        }
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        let Some(generation) = self.menu_generation(popup) else {
            return;
        };
        crate::popup::on_done(self.rt, generation);
        // SCTK holds its own handle to `popup` for the length of this call, so
        // the teardown has to happen after it returns.
        popup_destroy(self.rt, generation);
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
mod model_tests {
    //! Model-driven suite for the root-window startup/scale state machine.
    //! [`Model`] mirrors the effect layer exactly — every decision routes
    //! through the same pure functions the live code uses (`plan`,
    //! `ScaleDiscovery::{request, after_batch_drained}`, `scale_displaces`) —
    //! while recording effects (configures presented, commits, probe spawns)
    //! for assertion.

    use super::presentation::{Inputs, ScaleDiscovery, Step, plan};
    use crate::window_state::{ScaleProvenance, WindowSize, scale_displaces};

    struct Model {
        mapped: bool,
        pending_configure: bool,
        scale: Option<ScaleProvenance>,
        discovery: ScaleDiscovery,
        configures: u32,
        commits: u32,
        probe_spawns: u32,
    }

    #[derive(Clone, Copy)]
    enum Ev {
        /// WindowHandler::configure dispatches (SCTK has acked the serial).
        Configure,
        /// wp_fractional_scale.preferred_scale dispatches.
        PreferredScale,
        /// dispatch_pending returned: the current batch is fully drained.
        BatchDrained,
        /// The bounded fallback probe fed a provisional scale (probe result or
        /// unit fallback — identical control flow).
        ProbeCompletes,
        /// Boot found no wp_fractional_scale_manager_v1.
        BootUnitScale,
    }

    impl Model {
        fn new() -> Self {
            Self {
                mapped: false,
                pending_configure: false,
                scale: None,
                discovery: ScaleDiscovery::Idle,
                configures: 0,
                commits: 0,
                probe_spawns: 0,
            }
        }

        fn feed(&mut self, incoming: ScaleProvenance) {
            if scale_displaces(self.scale, incoming) {
                self.scale = Some(incoming);
            }
        }

        /// Mirrors `RootState::try_present`.
        fn drive(&mut self) {
            let step = plan(Inputs {
                mapped: self.mapped,
                pending_configure: self.pending_configure,
                scale_known: self.scale.is_some(),
                // The boot floating size is always recorded, so a floating
                // root always resolves; size-resolution corner cases are
                // covered by the resolve_logical_size tests.
                size: WindowSize::new(1280, 720),
            });
            match step {
                Step::Wait => {}
                Step::DiscoverScale => self.discovery = self.discovery.request(),
                Step::Present => {
                    assert!(
                        self.pending_configure || self.mapped,
                        "present without any configure"
                    );
                    if self.pending_configure {
                        self.pending_configure = false;
                        self.configures += 1;
                        self.mapped = true;
                    }
                    // One latch raise = exactly one root commit per transaction.
                    self.commits += 1;
                }
            }
        }

        fn ev(&mut self, e: Ev) {
            match e {
                Ev::Configure => {
                    self.pending_configure = true;
                    self.drive();
                }
                Ev::PreferredScale => {
                    self.feed(ScaleProvenance::Authoritative);
                    self.drive();
                }
                Ev::BatchDrained => {
                    let (next, spawn) = self.discovery.after_batch_drained(self.scale.is_some());
                    self.discovery = next;
                    if spawn {
                        self.probe_spawns += 1;
                    }
                }
                Ev::ProbeCompletes => {
                    self.feed(ScaleProvenance::Provisional);
                    self.drive();
                }
                Ev::BootUnitScale => self.feed(ScaleProvenance::Provisional),
            }
        }

        fn run(&mut self, evs: &[Ev]) {
            for &e in evs {
                self.ev(e);
            }
        }
    }

    #[test]
    fn preferred_scale_before_configure() {
        let mut m = Model::new();
        m.run(&[Ev::PreferredScale]);
        // Scale alone must not touch the surface before the first configure.
        assert_eq!(m.commits, 0);
        m.run(&[Ev::Configure, Ev::BatchDrained]);
        assert_eq!((m.configures, m.commits, m.probe_spawns), (1, 1, 0));
        assert_eq!(m.discovery, ScaleDiscovery::Idle);
    }

    #[test]
    fn preferred_scale_later_in_same_batch_wins_without_probe() {
        let mut m = Model::new();
        m.run(&[Ev::Configure]);
        // Mid-batch: the callback only requested discovery — it must not probe.
        assert_eq!(m.discovery, ScaleDiscovery::Requested);
        assert_eq!(m.commits, 0);
        m.run(&[Ev::PreferredScale, Ev::BatchDrained]);
        assert_eq!((m.configures, m.commits, m.probe_spawns), (1, 1, 0));
        assert_eq!(m.discovery, ScaleDiscovery::Idle);
    }

    #[test]
    fn withheld_scale_probes_after_drain_then_authoritative_corrects() {
        let mut m = Model::new();
        m.run(&[Ev::Configure, Ev::BatchDrained]);
        // Hyprland style: nothing else in the batch, so exactly one probe.
        assert_eq!(m.probe_spawns, 1);
        assert_eq!(m.commits, 0);
        m.run(&[Ev::ProbeCompletes]);
        assert_eq!((m.configures, m.commits), (1, 1));
        assert_eq!(m.scale, Some(ScaleProvenance::Provisional));
        m.run(&[Ev::PreferredScale]);
        assert_eq!(m.scale, Some(ScaleProvenance::Authoritative));
        assert_eq!((m.configures, m.commits), (1, 2));
    }

    #[test]
    fn probe_failure_presents_with_unit_fallback() {
        // A failed probe feeds the unit scale through the identical path, so
        // startup still completes.
        let mut m = Model::new();
        m.run(&[Ev::Configure, Ev::BatchDrained, Ev::ProbeCompletes]);
        assert_eq!((m.configures, m.commits), (1, 1));
        assert_eq!(m.scale, Some(ScaleProvenance::Provisional));
    }

    #[test]
    fn late_probe_result_never_clobbers_authoritative_scale() {
        let mut m = Model::new();
        m.run(&[
            Ev::Configure,
            Ev::BatchDrained,
            Ev::PreferredScale,
            Ev::ProbeCompletes,
        ]);
        assert_eq!(m.scale, Some(ScaleProvenance::Authoritative));
    }

    #[test]
    fn no_fractional_scale_manager_uses_unit_scale_without_discovery() {
        let mut m = Model::new();
        m.run(&[Ev::BootUnitScale, Ev::Configure, Ev::BatchDrained]);
        assert_eq!((m.configures, m.commits, m.probe_spawns), (1, 1, 0));
        assert_eq!(m.discovery, ScaleDiscovery::Idle);
    }

    #[test]
    fn repeated_unchanged_scales_re_present_without_new_ack_or_probe() {
        let mut m = Model::new();
        m.run(&[Ev::PreferredScale, Ev::Configure, Ev::BatchDrained]);
        m.run(&[Ev::PreferredScale, Ev::PreferredScale, Ev::BatchDrained]);
        assert_eq!((m.configures, m.commits, m.probe_spawns), (1, 3, 0));
    }

    #[test]
    fn output_migration_scale_without_configure_re_presents() {
        let mut m = Model::new();
        m.run(&[Ev::PreferredScale, Ev::Configure, Ev::BatchDrained]);
        // Moving to another output delivers only a new preferred_scale.
        m.run(&[Ev::PreferredScale, Ev::BatchDrained]);
        assert_eq!((m.configures, m.commits), (1, 2));
    }

    #[test]
    fn bare_configure_ack_after_map() {
        let mut m = Model::new();
        m.run(&[Ev::PreferredScale, Ev::Configure, Ev::BatchDrained]);
        m.run(&[Ev::Configure, Ev::BatchDrained]);
        assert_eq!((m.configures, m.commits), (2, 2));
    }

    #[test]
    fn pending_fallback_never_respawns_and_never_blocks_shutdown() {
        let mut m = Model::new();
        m.run(&[Ev::Configure, Ev::BatchDrained]);
        assert_eq!(m.probe_spawns, 1);
        // The probe never completes (wedged compositor): further drains must
        // not spawn again — the loop stays free to exit at shutdown, and the
        // orphaned probe thread owns only its private connection.
        m.run(&[Ev::BatchDrained, Ev::BatchDrained]);
        assert_eq!(m.probe_spawns, 1);
        assert_eq!(m.discovery, ScaleDiscovery::Spawned);
        assert_eq!(m.commits, 0);
    }

    #[test]
    fn callbacks_never_spawn_probes() {
        // Spawning happens only on BatchDrained, whatever callbacks arrive.
        let mut m = Model::new();
        m.run(&[
            Ev::Configure,
            Ev::Configure,
            Ev::PreferredScale,
            Ev::Configure,
        ]);
        assert_eq!(m.probe_spawns, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::presentation::{Inputs, ScaleDiscovery, Step, plan};
    use super::resolve_logical_size;
    use crate::window_state::{WindowMode, WindowSize};
    use std::num::NonZeroI32;

    #[test]
    fn discovery_request_is_idempotent_and_never_downgrades() {
        assert_eq!(ScaleDiscovery::Idle.request(), ScaleDiscovery::Requested);
        assert_eq!(
            ScaleDiscovery::Requested.request(),
            ScaleDiscovery::Requested
        );
        // A spawned probe must not be re-requested by later configures.
        assert_eq!(ScaleDiscovery::Spawned.request(), ScaleDiscovery::Spawned);
    }

    #[test]
    fn discovery_batch_drain_transitions() {
        assert_eq!(
            ScaleDiscovery::Requested.after_batch_drained(true),
            (ScaleDiscovery::Idle, false)
        );
        assert_eq!(
            ScaleDiscovery::Requested.after_batch_drained(false),
            (ScaleDiscovery::Spawned, true)
        );
        for known in [false, true] {
            assert_eq!(
                ScaleDiscovery::Idle.after_batch_drained(known),
                (ScaleDiscovery::Idle, false)
            );
            assert_eq!(
                ScaleDiscovery::Spawned.after_batch_drained(known),
                (ScaleDiscovery::Spawned, false)
            );
        }
    }

    fn inputs(mapped: bool, pending_configure: bool, scale_known: bool, size: bool) -> Inputs {
        Inputs {
            mapped,
            pending_configure,
            scale_known,
            size: size.then(|| WindowSize::new(1280, 720).unwrap()),
        }
    }

    #[test]
    fn no_configure_and_unmapped_waits() {
        // Whatever else is known, nothing may happen before the first configure.
        for scale_known in [false, true] {
            for size in [false, true] {
                assert_eq!(plan(inputs(false, false, scale_known, size)), Step::Wait);
            }
        }
    }

    #[test]
    fn first_configure_without_scale_discovers() {
        for size in [false, true] {
            assert_eq!(plan(inputs(false, true, false, size)), Step::DiscoverScale);
        }
    }

    #[test]
    fn mapped_without_scale_waits_instead_of_probing() {
        // After map the compositor owns the scale; a re-present must not probe.
        assert_eq!(plan(inputs(true, true, false, true)), Step::Wait);
        assert_eq!(plan(inputs(true, false, false, true)), Step::Wait);
    }

    #[test]
    fn unresolvable_size_waits() {
        assert_eq!(plan(inputs(false, true, true, false)), Step::Wait);
        assert_eq!(plan(inputs(true, false, true, false)), Step::Wait);
    }

    #[test]
    fn presents_once_configured_scaled_and_sized() {
        assert_eq!(plan(inputs(false, true, true, true)), Step::Present);
        assert_eq!(plan(inputs(true, true, true, true)), Step::Present);
        // Re-present without a new configure (scale/size change after map).
        assert_eq!(plan(inputs(true, false, true, true)), Step::Present);
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
