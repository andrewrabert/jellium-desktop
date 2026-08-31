//! `Platform` trait + global handle held by `jfn_app_main`.
//!
//! Each backend crate (`jfn-wayland`, `jfn-x11`, `jfn-macos`, `jfn-windows`)
//! returns a concrete type implementing this trait via its
//! `make_*_platform()` factory. The binary installs the chosen backend into
//! the [`OnceLock`] below via [`install`] / [`get`].
//!
//! `JfnRect` stays `#[repr(C)]` because CEF's `OnAcceleratedPaint` accel-paint
//! info hands it across the C ABI surface; the popup request and other
//! payloads are plain Rust.

#![allow(non_snake_case)]

use parking_lot::{Condvar, Mutex};
use std::ffi::{c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub mod cef_host;
pub mod geometry;
pub mod instance;
pub mod media_sink;
pub mod menu;
pub mod mpv_host;
pub mod osr_popup;
pub mod paint;
#[cfg_attr(unix, path = "process_unix.rs")]
#[cfg_attr(not(unix), path = "process_other.rs")]
mod process;
pub mod selection;
#[cfg_attr(unix, path = "signal_unix.rs")]
#[cfg_attr(not(unix), path = "signal_other.rs")]
mod signal;
pub mod stack;
pub mod visibility;
pub mod window_owner;
pub mod window_source;

pub use cef_host::CefHost;
pub use geometry::{
    BootGeometry, COVERED_SCALES, LogicalPoint, LogicalSize, PhysicalPoint, PhysicalSize, Scale,
    SurfaceSize, WindowExtent, WindowGeometry, WindowPos,
};
pub use instance::{Instance, InstanceId};
pub use jfn_gpu_paint::DamageRect as JfnRect;
pub use jfn_gpu_paint::WindowTarget;
pub use media_sink::MediaSink;
pub use menu::{
    Generation, MENU_DISMISSED, MenuClose, MenuDelivery, MenuHost, MenuItem, MenuKind, MenuMetrics,
    MenuPaint, MenuPlacement, MenuRequest, MenuScript, MenuSelection, PopupSurface, menu_delivery,
    menu_has_selectable, menu_initial_row, menu_scripts,
};
pub use mpv_host::{DefaultMpvHost, MpvHost, VoWait};
pub use osr_popup::{NoOsrPopup, OsrPopupSurface};
pub use paint::{Content, FrameRetry, FrameSource, PaintFrame, Presented, Superseded};
pub use selection::{OnText, PrimarySelection};
pub use stack::Plane;
pub use visibility::{Ack, Visibility, VisibilityCommit};
pub use window_owner::{AppCreatedWindow, MpvBootWindow, MpvCreatedWindow, WindowOwner};
pub use window_source::{
    WindowSnapshot, WindowSource, notify_window_changed, subscribe_window_changed,
};

/// Preserves the process's SIGINT/SIGTERM dispositions across a scope.
///
/// `chrome/browser/chrome_browser_main_posix.cc` installs SIGINT/SIGTERM
/// handlers during `CefInitialize`, and that path is NOT gated by
/// `disable_signal_handlers`. Snapshot the caller's handlers on
/// construction and restore them on drop, confining Chromium's installs to
/// the guarded window. No-op off unix.
pub use signal::SignalGuard;

// =====================================================================
// Main-thread park (non-macOS default for run_main_loop/wake_main_loop)
// =====================================================================
//
// Non-macOS backends have no native run loop to block the process main
// thread on. The default `Platform::run_main_loop` parks here until the
// shutdown manager calls `wake_main_loop`, at which point main runs the
// teardown tail. A latching `bool` + `Condvar` is enough — it's a single
// dedicated blocking wait (not a `poll()` multiplexer), so no fd is needed
// and there's no `playback`-crate dependency. macOS overrides both methods
// (`[NSApp run]` / stop-NSApp) and never touches this.

struct MainPark {
    woken: Mutex<bool>,
    cv: Condvar,
}

static MAIN_PARK: MainPark = MainPark {
    woken: Mutex::new(false),
    cv: Condvar::new(),
};

/// Block until [`main_park_signal`] is called. Returns immediately if the
/// signal already fired (latched), so a wake racing ahead of the wait is
/// not lost.
pub fn main_park_wait() {
    let mut woken = MAIN_PARK.woken.lock();
    while !*woken {
        MAIN_PARK.cv.wait(&mut woken);
    }
}

/// Release [`main_park_wait`]. Idempotent and safe from any thread.
pub fn main_park_signal() {
    *MAIN_PARK.woken.lock() = true;
    MAIN_PARK.cv.notify_all();
}

/// Fixed `cef_cursor_type_t` shapes. `CT_CUSTOM` (a bitmap) and the `CT_DND_*`
/// cursors are excluded — listing them here would map a non-fixed cursor to a
/// fixed shape.
pub mod cursor {
    use cef::sys::cef_cursor_type_t as ct;

    macro_rules! cursor_shape {
        ($($variant:ident = $ct:ident),* $(,)?) => {
            #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
            #[repr(i32)]
            pub enum CursorShape {
                $($variant = ct::$ct as i32,)*
            }

            impl CursorShape {
                pub fn from_cef(raw: i32) -> Option<Self> {
                    $(if raw == ct::$ct as i32 { return Some(Self::$variant); })*
                    None
                }

                pub const fn as_raw(self) -> i32 {
                    self as i32
                }
            }
        };
    }

    cursor_shape! {
        Pointer = CT_POINTER,
        Cross = CT_CROSS,
        Hand = CT_HAND,
        IBeam = CT_IBEAM,
        Wait = CT_WAIT,
        Help = CT_HELP,
        EastResize = CT_EASTRESIZE,
        NorthResize = CT_NORTHRESIZE,
        NorthEastResize = CT_NORTHEASTRESIZE,
        NorthWestResize = CT_NORTHWESTRESIZE,
        SouthResize = CT_SOUTHRESIZE,
        SouthEastResize = CT_SOUTHEASTRESIZE,
        SouthWestResize = CT_SOUTHWESTRESIZE,
        WestResize = CT_WESTRESIZE,
        NorthSouthResize = CT_NORTHSOUTHRESIZE,
        EastWestResize = CT_EASTWESTRESIZE,
        NorthEastSouthWestResize = CT_NORTHEASTSOUTHWESTRESIZE,
        NorthWestSouthEastResize = CT_NORTHWESTSOUTHEASTRESIZE,
        ColumnResize = CT_COLUMNRESIZE,
        RowResize = CT_ROWRESIZE,
        MiddlePanning = CT_MIDDLEPANNING,
        EastPanning = CT_EASTPANNING,
        NorthPanning = CT_NORTHPANNING,
        NorthEastPanning = CT_NORTHEASTPANNING,
        NorthWestPanning = CT_NORTHWESTPANNING,
        SouthPanning = CT_SOUTHPANNING,
        SouthEastPanning = CT_SOUTHEASTPANNING,
        SouthWestPanning = CT_SOUTHWESTPANNING,
        WestPanning = CT_WESTPANNING,
        Move = CT_MOVE,
        VerticalText = CT_VERTICALTEXT,
        Cell = CT_CELL,
        ContextMenu = CT_CONTEXTMENU,
        Alias = CT_ALIAS,
        Progress = CT_PROGRESS,
        NoDrop = CT_NODROP,
        Copy = CT_COPY,
        None = CT_NONE,
        NotAllowed = CT_NOTALLOWED,
        ZoomIn = CT_ZOOMIN,
        ZoomOut = CT_ZOOMOUT,
        Grab = CT_GRAB,
        Grabbing = CT_GRABBING,
        MiddlePanningVertical = CT_MIDDLE_PANNING_VERTICAL,
        MiddlePanningHorizontal = CT_MIDDLE_PANNING_HORIZONTAL,
    }
}

/// Canonical `cef_event_flags_t` modifier bits — the single source of truth
/// for the CEF `EVENTFLAG_*` masks that flow through key/mouse dispatch.
/// Derived from the generated CEF bindings (a newtype with associated
/// constants) so backends import these instead of hand-copying bit shifts
/// that can silently drift. Typed `u32` to match the dispatch ABI.
pub mod event_flags {
    use cef::sys::cef_event_flags_t as ef;

    macro_rules! flag_consts {
        ($($name:ident),* $(,)?) => {
            $(pub const $name: u32 = ef::$name.0 as u32;)*
        };
    }

    flag_consts! {
        EVENTFLAG_CAPS_LOCK_ON, EVENTFLAG_SHIFT_DOWN, EVENTFLAG_CONTROL_DOWN,
        EVENTFLAG_ALT_DOWN, EVENTFLAG_LEFT_MOUSE_BUTTON, EVENTFLAG_MIDDLE_MOUSE_BUTTON,
        EVENTFLAG_RIGHT_MOUSE_BUTTON, EVENTFLAG_COMMAND_DOWN, EVENTFLAG_NUM_LOCK_ON,
        EVENTFLAG_IS_KEY_PAD, EVENTFLAG_IS_LEFT, EVENTFLAG_IS_RIGHT, EVENTFLAG_ALTGR_DOWN,
        EVENTFLAG_IS_REPEAT, EVENTFLAG_PRECISION_SCROLLING_DELTA, EVENTFLAG_SCROLL_BY_PAGE,
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DisplayBackend {
    Wayland,
    X11,
    Windows,
    MacOS,
}

impl DisplayBackend {
    /// The modifier that means "application action" in keyboard shortcuts.
    pub fn action_modifier_flag(self) -> u32 {
        match self {
            DisplayBackend::MacOS => event_flags::EVENTFLAG_COMMAND_DOWN,
            _ => event_flags::EVENTFLAG_CONTROL_DOWN,
        }
    }

    /// Whether CEF's browser-process `MainArgs` carries the full argv for
    /// Chromium to parse. When false the caller hands CEF a clean
    /// `[argv[0]]` and pushes switches explicitly instead.
    pub fn cef_full_browser_argv(self) -> bool {
        matches!(self, DisplayBackend::Windows)
    }
}

/// Filesystem locations CEF needs written into `Settings` before
/// `CefInitialize`, resolved per platform. Each `None` field is left at
/// CEF's own default rather than cleared.
#[derive(Default)]
pub struct CefPaths {
    pub browser_subprocess_path: Option<PathBuf>,
    pub framework_dir_path: Option<PathBuf>,
    pub resources_dir_path: Option<PathBuf>,
    pub locales_dir_path: Option<PathBuf>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum WindowDecorations {
    /// Client-side: the app draws its own titlebar in-page.
    Csd,
    Server,
    ServerThemed,
}

impl WindowDecorations {
    /// Wire/persistence contract: settings.json, the JS↔Rust IPC, and the web
    /// settings UI all speak these literals.
    pub fn as_str(self) -> &'static str {
        match self {
            WindowDecorations::Csd => "csd",
            WindowDecorations::Server => "server",
            WindowDecorations::ServerThemed => "serverThemed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "csd" => Some(WindowDecorations::Csd),
            "server" => Some(WindowDecorations::Server),
            "serverThemed" => Some(WindowDecorations::ServerThemed),
            _ => None,
        }
    }
}

/// The decoration mode in effect right now — the app-level projection of
/// whatever authority the platform has (on Wayland, the compositor's
/// xdg-decoration verdict). Two-valued and total: protocol lifecycle states
/// (no such protocol, verdict pending) never cross this boundary.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EffectiveDecorations {
    /// The app draws its own titlebar.
    ClientSide,
    /// The OS/compositor owns decorations; the app draws none.
    ServerSide,
}

/// The set of decoration modes a backend can actually honor. CSD is a member
/// of every set — the app can always draw its own titlebar — so only the
/// server-side modes vary, and resolution against a set is total: anything
/// the set lacks falls back to CSD, which [`Self::contains`] always accepts.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct DecorationOptions {
    server: bool,
    server_themed: bool,
}

impl DecorationOptions {
    pub fn csd_only() -> Self {
        Self {
            server: false,
            server_themed: false,
        }
    }

    pub fn with_server(themed: bool) -> Self {
        Self {
            server: true,
            server_themed: themed,
        }
    }

    pub fn all() -> Self {
        Self::with_server(true)
    }

    pub fn contains(self, mode: WindowDecorations) -> bool {
        match mode {
            WindowDecorations::Csd => true,
            WindowDecorations::Server => self.server,
            WindowDecorations::ServerThemed => self.server_themed,
        }
    }

    /// Whether there is anything to choose — a CSD-only set leaves the user
    /// no decision, so the settings entry is hidden.
    pub fn has_choice(self) -> bool {
        self.server
    }

    pub fn iter(self) -> impl Iterator<Item = WindowDecorations> {
        [
            Some(WindowDecorations::Csd),
            self.server.then_some(WindowDecorations::Server),
            self.server_themed
                .then_some(WindowDecorations::ServerThemed),
        ]
        .into_iter()
        .flatten()
    }
}

/// Idle-inhibit level.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IdleInhibitLevel {
    None,
    System,
    Display,
}

/// Backend-allocated per-surface handle: an opaque, backend-defined id.
///
/// Callers hold it as a value and pass it back verbatim; no caller may
/// dereference it. Its representation is one machine word so it round-trips
/// losslessly through the CEF C++ layer's `void*` slot. Pointer-backed
/// backends (Wayland/Windows/macOS) bridge through [`SurfaceHandle::from_ptr`]
/// / [`SurfaceHandle::as_ptr`]; the X11 backend stores a generational id via
/// [`SurfaceHandle::from_id`] / [`SurfaceHandle::id`] and never treats it as a
/// pointer.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct SurfaceHandle(*mut c_void);

// A word-sized opaque id; the wrapped pointer is never dereferenced by the ABI
// itself. Backends carry their own `unsafe impl Send` on the state they map it to.
unsafe impl Send for SurfaceHandle {}
unsafe impl Sync for SurfaceHandle {}

impl SurfaceHandle {
    /// The absent handle (allocation failed / no surface).
    pub const NONE: Self = Self(std::ptr::null_mut());

    #[must_use]
    pub fn is_none(self) -> bool {
        self.0.is_null()
    }

    /// Bridge for pointer-backed backends: wrap a backend surface pointer.
    #[must_use]
    pub fn from_ptr(p: *mut c_void) -> Self {
        Self(p)
    }

    /// Bridge for pointer-backed backends: recover the backend surface pointer.
    #[must_use]
    pub fn as_ptr(self) -> *mut c_void {
        self.0
    }

    /// Bridge for id-backed backends (X11): pack a generational id. The value
    /// is never dereferenced.
    #[must_use]
    pub fn from_id(id: u64) -> Self {
        Self(id as *mut c_void)
    }

    /// Bridge for id-backed backends (X11): recover the generational id.
    #[must_use]
    pub fn id(self) -> u64 {
        self.0 as u64
    }
}

/// The gate a backend arms at the physical size a resize transition settles
/// at. Reachable only through [`Platform::resize_gate`].
pub trait ResizeGate: Send + Sync {
    fn begin(&self);

    fn end(&self);

    fn in_transition(&self) -> bool;

    fn set_expected(&self, size: PhysicalSize);
}

/// The window controls an app-drawn titlebar drives. Reachable only through
/// [`Platform::titlebar_controls`].
pub trait TitlebarControls: Send + Sync {
    fn minimize(&self);

    fn toggle_maximize(&self);

    /// Begin an interactive, compositor-driven window move. Must be called in
    /// response to a pointer button press on the titlebar drag region.
    fn start_move(&self);

    /// Begin an interactive, compositor-driven resize from the given edge.
    /// `edge` is the xdg_toplevel resize-edge mask: top=1, bottom=2, left=4,
    /// right=8, corners are the ORs.
    fn start_resize(&self, edge: c_int);
}

/// Process-wide platform handle.
///
/// A default body here expresses shared mechanism or an absence the type
/// makes total, and nothing else: every method carrying a platform answer is
/// required, so adding one fails to compile all four backends.
///
/// All methods take `&self` — backends keep their own interior mutability
/// (`Mutex`, `AtomicBool`, etc) where they need it.
pub trait Platform: Send + Sync {
    fn display(&self) -> DisplayBackend;

    fn default_window_decorations(&self) -> WindowDecorations;

    /// Decoration modes this backend can honor.
    fn window_decoration_options(&self) -> DecorationOptions;

    fn resolve_window_decorations(
        &self,
        configured: Option<WindowDecorations>,
    ) -> WindowDecorations {
        let wanted = configured.unwrap_or_else(|| self.default_window_decorations());
        if self.window_decoration_options().contains(wanted) {
            wanted
        } else {
            WindowDecorations::Csd
        }
    }

    fn early_init(&self);
    /// `mpv` is the opaque libmpv `mpv_handle` — a raw C handle, stays raw.
    fn init(&self, mpv: *mut c_void) -> bool;
    fn cleanup(&self);
    fn post_window_cleanup(&self);

    // Per-surface
    /// The surface starts in `initial`; no surface is born at a default.
    fn alloc_surface(&self, initial: Visibility) -> SurfaceHandle;
    fn free_surface(&self, s: SurfaceHandle);
    /// Presents `frame`, or hands it back undischarged when this surface has no
    /// commit stream for it.
    fn surface_present<'a>(
        &self,
        s: SurfaceHandle,
        frame: PaintFrame<'a>,
    ) -> Result<Presented, PaintFrame<'a>>;
    /// Applies `size` to `s` before returning.
    fn surface_resize(&self, s: SurfaceHandle, size: SurfaceSize);
    /// The swapchain target for `s`, or `None` until the backend has created
    /// the surface's window.
    ///
    /// Calling it declares that the caller presents to `s` itself: from the
    /// first call the backend attaches no buffer to `s`, never calls
    /// [`Platform::surface_present`] on it, grabs no input on it, and gives it
    /// an empty input region.
    fn surface_window_target(&self, s: SurfaceHandle) -> Option<WindowTarget>;

    /// Notify once the native target can be queried. Synchronous backends
    /// already have their target when allocation returns. Register before
    /// querying to avoid losing a concurrent creation notification.
    fn on_surface_target_ready(&self, _s: SurfaceHandle, ready: Box<dyn FnOnce() + Send>) {
        ready();
    }

    /// Issues the commit carrying `visibility` before returning.
    fn set_surface_visibility(&self, s: SurfaceHandle, visibility: Visibility) -> VisibilityCommit;

    /// Applies the whole order, bottom first, in one transaction, and pins the
    /// video plane below every named surface.
    fn apply_stack(&self, ordered: &[SurfaceHandle]);

    /// How this backend delivers `kind`; `Host` names the backend's own menu
    /// host.
    fn menu_delivery(&self, kind: MenuKind) -> MenuDelivery;

    fn osr_popup_surface(&self) -> &dyn OsrPopupSurface {
        &NoOsrPopup
    }

    /// How this platform hosts mpv's lifecycle (env prep, VO wait,
    /// teardown detach).
    fn mpv_host(&self) -> &dyn MpvHost;

    /// `Some` when the platform drives CEF's message loop itself
    /// (external pump); `None` runs CEF's multi-threaded message loop.
    fn cef_host(&self) -> Option<&dyn CefHost> {
        None
    }

    /// OS media-session integration. Non-optional — every platform has a
    /// sink.
    fn media_session(&self) -> &dyn MediaSink;

    fn cef_paths(&self) -> CefPaths;

    // Fullscreen
    fn set_fullscreen(&self, v: bool);
    fn toggle_fullscreen(&self);

    /// The controls an app-drawn titlebar drives, or `None` where the OS or
    /// the window manager draws the app window's titlebar.
    fn titlebar_controls(&self) -> Option<&dyn TitlebarControls>;

    /// The resize-transition gate, or `None` where this backend gates none.
    fn resize_gate(&self) -> Option<&dyn ResizeGate>;

    /// The display scale this backend reports for the app window.
    fn scale(&self) -> Scale;

    /// The display scale this backend reports for the display holding `at`,
    /// or for its own default display when `at` is `None`.
    fn display_scale(&self, at: Option<WindowPos>) -> Scale;

    /// The window's current position in backing pixels, or `None` when this
    /// backend has no window position to report.
    fn query_window_position(&self) -> Option<WindowPos>;

    /// Who creates the app window, and the live geometry authority for it.
    fn window_owner(&self) -> WindowOwner<'_>;

    /// Clamp saved geometry to stay on-screen.
    fn clamp_window_geometry(&self, g: WindowGeometry) -> WindowGeometry;

    fn pump(&self);
    /// Block the process main thread until [`wake_main_loop`] is called.
    /// Default parks on the process-wide [`main_park_wait`]; macOS overrides
    /// with `[NSApp run]`.
    fn run_main_loop(&self) {
        main_park_wait();
    }
    /// Release [`run_main_loop`] so main can run the teardown tail. Safe from
    /// any thread. Default signals [`main_park_signal`]; macOS overrides to
    /// stop the NSApp loop.
    fn wake_main_loop(&self) {
        main_park_signal();
    }

    fn set_cursor(&self, shape: cursor::CursorShape);
    fn set_idle_inhibit(&self, level: IdleInhibitLevel);
    fn set_theme_color(&self, rgb: u32);

    /// Whether the window-decorations setting (client-side vs server-side
    /// titlebar) applies on this platform. Gates the settings UI entry; the
    /// entry's option list comes from [`Platform::window_decoration_options`].
    fn window_decorations_supported(&self) -> bool;
    /// The decoration mode currently in effect. Changes are announced via
    /// [`notify_decorations_changed`].
    fn effective_decorations(&self) -> EffectiveDecorations;

    fn shared_texture_supported(&self) -> bool;

    /// True where `CefInitialize` depends on neither platform init nor a run
    /// loop the boot wait owns, so CEF's process bring-up may run while mpv's
    /// core thread creates the VO.
    fn cef_init_precedes_mpv_window(&self) -> bool;
    /// Revises [`Platform::shared_texture_supported`] to `false` where the
    /// backend resolves the shared-texture path after init.
    fn set_shared_texture_unsupported(&self);

    /// The OS clipboard's text.
    /// `None` when it holds no text, or the read failed.
    fn clipboard_read_text_async(&self, on_done: OnText);

    /// Places `text` on the OS clipboard.
    /// A backend that cannot take the selection leaves the previous contents.
    fn clipboard_write_text(&self, text: &str);

    /// The primary selection, on the backends that serve one.
    fn primary_selection(&self) -> Option<&dyn PrimarySelection> {
        None
    }

    /// Whether the web overlay pastes by reading the OS clipboard and
    /// injecting the text, rather than by calling `frame.Paste()`.
    /// Pinned per backend, and unrelated to the shell overlay's clipboard.
    fn web_paste_reads_clipboard(&self) -> bool;

    fn open_external_url(&self, url: &str);

    /// Open a filesystem path in the OS file manager.
    fn open_path(&self, path: &Path);

    /// Run `f` to completion without deadlocking work that needs the
    /// main thread (e.g. mpv's VO uninit doing `DispatchQueue.main.sync`).
    /// Default runs `f` inline; macOS runs it on a side thread while main
    /// pumps its run loop.
    fn run_blocking(&self, f: Box<dyn FnOnce() + Send>) {
        f();
    }

    /// `on_shutdown` must be async-signal-safe.
    fn install_shutdown_handler(&self, on_shutdown: fn()) {
        process::install_shutdown(on_shutdown);
    }
}

// =====================================================================
// Process-wide handle
// =====================================================================

// `OnceLock<Box<dyn Platform>>` doesn't give us a stable `'static` reference
// shape that's ergonomic for the existing `unsafe extern "C"` thunks below;
// store a raw fat pointer instead. Set exactly once during boot.
static PLATFORM: OnceLock<&'static dyn Platform> = OnceLock::new();

/// Install the platform backend. Must be called exactly once during boot,
/// before any other code dispatches through [`get`]. Panics if called
/// twice — there is no "swap backend at runtime" path.
#[allow(clippy::expect_used)] // boot invariant: install exactly once
pub fn install(p: Box<dyn Platform>) {
    let leaked: &'static dyn Platform = Box::leak(p);
    PLATFORM
        .set(leaked)
        .map_err(|_| ())
        .expect("install() called twice");
}

/// Returns the installed platform backend. Panics if [`install`] hasn't
/// been called yet — every call site is post-boot.
#[allow(clippy::expect_used)] // every call site is post-boot
pub fn get() -> &'static dyn Platform {
    *PLATFORM
        .get()
        .expect("jfn_platform_abi::get() called before install()")
}

/// Like [`get`] but returns `None` before install. Used by jfn_cef's
/// `OnConsoleMessage` and similar paths that may fire during early CEF
/// helper-process boot when no platform is installed.
pub fn try_get() -> Option<&'static dyn Platform> {
    PLATFORM.get().copied()
}

/// Titlebar height, logical pixels.
pub const TITLEBAR_LOGICAL_HEIGHT: c_int = 32;

static ABOUT_HANDLER: OnceLock<fn()> = OnceLock::new();

/// Register the callback that raises the about panel. Single listener,
/// installed once at boot.
pub fn set_about_handler(f: fn()) {
    let _ = ABOUT_HANDLER.set(f);
}

pub fn request_about() {
    if let Some(f) = ABOUT_HANDLER.get() {
        f();
    }
}

static CLIENT_SETTINGS_HANDLER: OnceLock<fn()> = OnceLock::new();

/// Register the callback that raises client settings. Single listener,
/// installed once at boot.
pub fn set_client_settings_handler(f: fn()) {
    let _ = CLIENT_SETTINGS_HANDLER.set(f);
}

pub fn request_client_settings() {
    if let Some(f) = CLIENT_SETTINGS_HANDLER.get() {
        f();
    }
}

static DECORATIONS_LISTENER: OnceLock<fn()> = OnceLock::new();

/// Register the callback fired when [`Platform::effective_decorations`]
/// changes. Single listener, installed once alongside the browser bridge.
pub fn set_decorations_listener(f: fn()) {
    let _ = DECORATIONS_LISTENER.set(f);
}

pub fn notify_decorations_changed() {
    if let Some(f) = DECORATIONS_LISTENER.get() {
        f();
    }
}
