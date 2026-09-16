#![allow(non_snake_case)]

use parking_lot::{Condvar, Mutex};
use std::ffi::{c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub mod blocking;
pub use blocking::BlockingError;
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
mod subscriptions;
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
    MenuPaint, MenuPlacement, MenuRequest, MenuScript, MenuSelection, PopupSurface,
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
    WindowSnapshot, WindowSource, WindowSubscription, notify_window_changed,
    subscribe_window_changed,
};

pub use signal::SignalGuard;

struct MainPark {
    woken: Mutex<bool>,
    cv: Condvar,
}

static MAIN_PARK: MainPark = MainPark {
    woken: Mutex::new(false),
    cv: Condvar::new(),
};

pub fn main_park_wait() {
    let mut woken = MAIN_PARK.woken.lock();
    while !*woken {
        MAIN_PARK.cv.wait(&mut woken);
    }
}

pub fn main_park_signal() {
    *MAIN_PARK.woken.lock() = true;
    MAIN_PARK.cv.notify_all();
}

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
    pub fn action_modifier_flag(self) -> u32 {
        match self {
            DisplayBackend::MacOS => event_flags::EVENTFLAG_COMMAND_DOWN,
            _ => event_flags::EVENTFLAG_CONTROL_DOWN,
        }
    }

    pub fn cef_full_browser_argv(self) -> bool {
        matches!(self, DisplayBackend::Windows)
    }
}

#[derive(Default)]
pub struct CefPaths {
    pub browser_subprocess_path: Option<PathBuf>,
    pub framework_dir_path: Option<PathBuf>,
    pub resources_dir_path: Option<PathBuf>,
    pub locales_dir_path: Option<PathBuf>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum WindowDecorations {
    Csd,
    Server,
    ServerThemed,
}

impl WindowDecorations {
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

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EffectiveDecorations {
    ClientSide,
    ServerSide,
}

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

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IdleInhibitLevel {
    None,
    System,
    Display,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct SurfaceHandle(*mut c_void);

unsafe impl Send for SurfaceHandle {}
unsafe impl Sync for SurfaceHandle {}

impl SurfaceHandle {
    pub const NONE: Self = Self(std::ptr::null_mut());

    #[must_use]
    pub fn is_none(self) -> bool {
        self.0.is_null()
    }

    #[must_use]
    pub fn from_ptr(p: *mut c_void) -> Self {
        Self(p)
    }

    #[must_use]
    pub fn as_ptr(self) -> *mut c_void {
        self.0
    }

    #[must_use]
    pub fn from_id(id: u64) -> Self {
        Self(id as *mut c_void)
    }

    #[must_use]
    pub fn id(self) -> u64 {
        self.0 as u64
    }
}

pub trait ResizeGate: Send + Sync {
    fn begin(&self);

    fn end(&self);

    fn in_transition(&self) -> bool;

    fn set_expected(&self, size: PhysicalSize);
}

pub trait TitlebarControls: Send + Sync {
    fn minimize(&self);

    fn toggle_maximize(&self);

    fn start_move(&self);

    fn start_resize(&self, edge: c_int);
}

pub trait Platform: Send + Sync {
    fn display(&self) -> DisplayBackend;

    fn default_window_decorations(&self) -> WindowDecorations;

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
    fn init(&self, access: &LifecycleAccess, mpv: *mut c_void) -> Result<(), PlatformInitError>;
    fn cleanup(&self, access: &LifecycleAccess);
    fn post_window_cleanup(&self, access: &LifecycleAccess);

    fn alloc_surface(&self, initial: Visibility) -> SurfaceHandle;
    fn free_surface(&self, s: SurfaceHandle);
    fn surface_present<'a>(
        &self,
        s: SurfaceHandle,
        frame: PaintFrame<'a>,
    ) -> Result<Presented, PaintFrame<'a>>;
    fn surface_resize(&self, s: SurfaceHandle, size: SurfaceSize);
    fn surface_window_target(&self, s: SurfaceHandle) -> Option<WindowTarget>;

    fn on_surface_target_ready(&self, _s: SurfaceHandle, ready: Box<dyn FnOnce() + Send>) {
        ready();
    }

    fn set_surface_visibility(&self, s: SurfaceHandle, visibility: Visibility) -> VisibilityCommit;

    fn apply_stack(&self, ordered: &[SurfaceHandle]);

    fn menu_delivery(&self, kind: MenuKind) -> MenuDelivery<'_>;

    fn osr_popup_surface(&self) -> &dyn OsrPopupSurface {
        &NoOsrPopup
    }

    fn mpv_host(&self) -> &dyn MpvHost;

    fn cef_host(&self) -> Option<&dyn CefHost> {
        None
    }

    fn media_session(&self) -> &dyn MediaSink;

    fn cef_paths(&self) -> CefPaths;

    fn set_fullscreen(&self, v: bool);
    fn toggle_fullscreen(&self);

    fn titlebar_controls(&self) -> Option<&dyn TitlebarControls>;

    fn resize_gate(&self) -> Option<&dyn ResizeGate>;

    fn scale(&self) -> Scale;

    fn display_scale(&self, at: Option<WindowPos>) -> Scale;

    fn query_window_position(&self) -> Option<WindowPos>;

    fn window_owner(&self) -> WindowOwner<'_>;

    fn clamp_window_geometry(&self, g: WindowGeometry) -> WindowGeometry;

    fn pump(&self);
    fn run_main_loop(&self) {
        main_park_wait();
    }
    fn wake_main_loop(&self) {
        main_park_signal();
    }

    fn set_cursor(&self, shape: cursor::CursorShape);
    fn set_idle_inhibit(&self, level: IdleInhibitLevel);
    fn set_theme_color(&self, rgb: u32);

    fn window_decorations_supported(&self) -> bool;
    fn effective_decorations(&self) -> EffectiveDecorations;

    fn shared_texture_supported(&self) -> bool;

    fn cef_init_precedes_mpv_window(&self) -> bool;
    fn set_shared_texture_unsupported(&self);

    fn clipboard_read_text_async(&self, on_done: OnText);

    fn clipboard_write_text(&self, text: &str);

    fn primary_selection(&self) -> Option<&dyn PrimarySelection> {
        None
    }

    fn web_paste_reads_clipboard(&self) -> bool;

    fn open_external_url(&self, url: &str);

    fn open_path(&self, path: &Path);

    fn run_blocking(&self, f: Box<dyn FnOnce() + Send>) -> Result<(), BlockingError> {
        f();
        Ok(())
    }

    fn install_shutdown_handler(&self, on_shutdown: fn()) {
        process::install_shutdown(on_shutdown);
    }
}

static PLATFORM: OnceLock<&'static dyn Platform> = OnceLock::new();

#[allow(clippy::expect_used)]
pub fn install(p: Box<dyn Platform>) {
    let leaked: &'static dyn Platform = Box::leak(p);
    PLATFORM
        .set(leaked)
        .map_err(|_| ())
        .expect("install() called twice");
}

#[allow(clippy::expect_used)]
pub unsafe fn get() -> &'static dyn Platform {
    *PLATFORM
        .get()
        .expect("jfn_platform_abi::get() called before install()")
}

pub unsafe fn try_get() -> Option<&'static dyn Platform> {
    PLATFORM.get().copied()
}

static LIVE_PLATFORM: Mutex<Option<PlatformLeaseWeak>> = Mutex::new(None);
struct PlatformLeaseWeak {
    platform: &'static dyn Platform,
    live: std::sync::Weak<LeaseState>,
}

pub fn try_lease() -> Option<PlatformLease> {
    let published = LIVE_PLATFORM.lock();
    let published = published.as_ref()?;
    let live = published.live.upgrade()?;
    *live.count.lock() += 1;
    Some(PlatformLease {
        platform: published.platform,
        live,
    })
}

#[allow(clippy::expect_used)]
pub fn resolve_window_decorations(configured: Option<WindowDecorations>) -> WindowDecorations {
    PLATFORM
        .get()
        .expect("platform policy requested before install")
        .resolve_window_decorations(configured)
}

pub const TITLEBAR_LOGICAL_HEIGHT: c_int = 32;

static ABOUT_HANDLER: OnceLock<fn()> = OnceLock::new();

pub fn set_about_handler(f: fn()) {
    let _ = ABOUT_HANDLER.set(f);
}

pub fn request_about() {
    if let Some(f) = ABOUT_HANDLER.get() {
        f();
    }
}

static CLIENT_SETTINGS_HANDLER: OnceLock<fn()> = OnceLock::new();

pub fn set_client_settings_handler(f: fn()) {
    let _ = CLIENT_SETTINGS_HANDLER.set(f);
}

pub fn request_client_settings() {
    if let Some(f) = CLIENT_SETTINGS_HANDLER.get() {
        f();
    }
}

static DECORATIONS_LISTENERS: std::sync::LazyLock<subscriptions::Subscribers> =
    std::sync::LazyLock::new(subscriptions::Subscribers::new);
pub use subscriptions::Subscription as DecorationsSubscription;

pub fn set_decorations_listener(f: fn()) -> DecorationsSubscription {
    DECORATIONS_LISTENERS.subscribe(f)
}

pub fn notify_decorations_changed() {
    DECORATIONS_LISTENERS.notify();
}

pub struct LifecycleAccess {
    _private: (),
}

#[derive(Debug)]
pub enum PlatformInitError {
    AlreadyClaimed,
    WrongThread,
    Panicked,
    Backend {
        operation: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl PlatformInitError {
    pub fn backend(
        operation: &'static str,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self::Backend {
            operation,
            source: source.into(),
        }
    }
}

impl std::fmt::Display for PlatformInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Panicked => f.write_str("platform initialization panicked; rollback required"),
            Self::AlreadyClaimed => f.write_str("platform startup has already been claimed"),
            Self::WrongThread => {
                f.write_str("platform initialization requires the macOS main thread")
            }
            Self::Backend { operation, source } => write!(f, "{operation}: {source}"),
        }
    }
}
impl std::error::Error for PlatformInitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Backend { source, .. } => Some(&**source),
            _ => None,
        }
    }
}

struct LeaseState {
    count: Mutex<usize>,
    changed: Condvar,
}
impl LeaseState {
    fn wait_for_dependents(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut count = self.count.lock();
        while *count != 1 {
            if self.changed.wait_until(&mut count, deadline).timed_out() {
                return *count == 1;
            }
        }
        true
    }
}

pub struct PlatformLease {
    platform: &'static dyn Platform,
    live: std::sync::Arc<LeaseState>,
}
impl Clone for PlatformLease {
    fn clone(&self) -> Self {
        *self.live.count.lock() += 1;
        Self {
            platform: self.platform,
            live: std::sync::Arc::clone(&self.live),
        }
    }
}
impl Drop for PlatformLease {
    fn drop(&mut self) {
        *self.live.count.lock() -= 1;
        self.live.changed.notify_all();
    }
}
impl PlatformLease {
    pub fn platform(&self) -> &dyn Platform {
        self.platform
    }
}

impl std::ops::Deref for PlatformLease {
    type Target = dyn Platform;
    fn deref(&self) -> &Self::Target {
        self.platform
    }
}

#[derive(Debug)]
pub struct PlatformBusy;
impl std::fmt::Display for PlatformBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("platform dependents remain alive; cleanup deferred")
    }
}
impl std::error::Error for PlatformBusy {}

#[must_use = "the initialized platform requires ordered cleanup"]
pub struct PlatformRuntime {
    lease: PlatformLease,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
#[must_use = "early platform resources require ordered cleanup"]
pub struct PreparedPlatform {
    platform: &'static dyn Platform,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl PreparedPlatform {
    pub fn claim(platform: &'static dyn Platform) -> Result<Self, PlatformInitError> {
        #[cfg(target_os = "macos")]
        {
            unsafe extern "C" {
                fn pthread_main_np() -> std::ffi::c_int;
            }
            if unsafe { pthread_main_np() } == 0 {
                return Err(PlatformInitError::WrongThread);
            }
        }
        static CLAIMED: OnceLock<()> = OnceLock::new();
        CLAIMED
            .set(())
            .map_err(|()| PlatformInitError::AlreadyClaimed)?;
        Ok(Self {
            platform,
            _thread: std::marker::PhantomData,
        })
    }
    pub fn initialize(
        self,
        mpv: *mut c_void,
    ) -> Result<PlatformRuntime, (PlatformInitError, Self)> {
        let access = LifecycleAccess { _private: () };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.platform.init(&access, mpv)
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err((error, self)),
            Err(_) => return Err((PlatformInitError::Panicked, self)),
        }
        let lease = PlatformLease {
            platform: self.platform,
            live: std::sync::Arc::new(LeaseState {
                count: Mutex::new(1),
                changed: Condvar::new(),
            }),
        };
        *LIVE_PLATFORM.lock() = Some(PlatformLeaseWeak {
            platform: self.platform,
            live: std::sync::Arc::downgrade(&lease.live),
        });
        Ok(PlatformRuntime {
            lease,
            _thread: std::marker::PhantomData,
        })
    }
    pub fn cleanup<F, E>(self, terminate_window: F) -> Result<(), (E, PostWindowCleanup)>
    where
        F: FnOnce() -> Result<(), E>,
    {
        let access = LifecycleAccess { _private: () };
        self.platform.cleanup(&access);
        PostWindowCleanup {
            platform: self.platform,
            _thread: std::marker::PhantomData,
        }
        .retry(terminate_window)
    }
}
impl PlatformRuntime {
    pub fn platform(&self) -> &dyn Platform {
        self.lease.platform
    }
    pub fn lease(&self) -> PlatformLease {
        self.lease.clone()
    }
    pub fn cleanup<F, E>(self, terminate_window: F) -> Result<(), PlatformCleanupError<F, E>>
    where
        F: FnOnce() -> Result<(), E>,
    {
        self.cleanup_with_timeout(terminate_window, std::time::Duration::from_secs(2))
    }

    fn cleanup_with_timeout<F, E>(
        self,
        terminate_window: F,
        timeout: std::time::Duration,
    ) -> Result<(), PlatformCleanupError<F, E>>
    where
        F: FnOnce() -> Result<(), E>,
    {
        *LIVE_PLATFORM.lock() = None;
        if *self.lease.live.count.lock() != 1 {
            let live = std::sync::Arc::clone(&self.lease.live);
            let (done, result) = std::sync::mpsc::sync_channel(1);
            if let Err(error) = self.platform().run_blocking(Box::new(move || {
                let _ = done.send(live.wait_for_dependents(timeout));
            })) {
                tracing::error!("platform quiescence: {error}");
                drop(error);
                return Err(PlatformCleanupError::Busy {
                    runtime: self,
                    terminate: terminate_window,
                });
            }
            if !matches!(result.try_recv(), Ok(true)) {
                return Err(PlatformCleanupError::Busy {
                    runtime: self,
                    terminate: terminate_window,
                });
            }
        }
        let access = LifecycleAccess { _private: () };
        self.platform().cleanup(&access);
        let remaining = PostWindowCleanup {
            platform: self.lease.platform,
            _thread: std::marker::PhantomData,
        };
        remaining
            .retry(terminate_window)
            .map_err(|(error, post_window)| PlatformCleanupError::Termination {
                error,
                post_window,
            })
    }
}

pub enum PlatformCleanupError<F, E> {
    Busy {
        runtime: PlatformRuntime,
        terminate: F,
    },
    Termination {
        error: E,
        post_window: PostWindowCleanup,
    },
}

#[must_use = "post-window resources require confirmed window termination"]
pub struct PostWindowCleanup {
    platform: &'static dyn Platform,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl PostWindowCleanup {
    pub fn retry<E>(
        self,
        terminate_window: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), (E, Self)> {
        match terminate_window() {
            Ok(()) => {
                self.finish();
                Ok(())
            }
            Err(error) => Err((error, self)),
        }
    }
    pub fn finish(self) {
        self.platform
            .post_window_cleanup(&LifecycleAccess { _private: () });
    }
    pub fn abandon(self) {}
}

#[cfg(test)]
static TEST_PLATFORM: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod lifecycle_tests;
