//! Browser state.
//!
//! Holds the browser handle the web overlay drives, the size it last applied
//! to it, its menu-session slot, the resize-debounce and the CEF browser ops
//! dispatch that schedules `WasResized`, `NotifyScreenInfoChanged`,
//! `Invalidate`, `SetWindowlessFrameRate`, `SendExternalBeginFrame`, and
//! `ExecuteJavaScript` calls on TID_UI.
//!
//! Lifetime model: `Arc<Inner>`, so posted CEF tasks keep a clone alive past
//! the overlay's own drop.

use cef::{Browser, RunContextMenuCallback};
use crossbeam_utils::atomic::AtomicCell;
use parking_lot::{Condvar, Mutex};
use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use crate::ipc::BrowserMessage;
use crate::menu_ownership::{MenuOwnership, Session};
use crate::platform_ops;
use crate::web_overlay::size::ViewSize;

use crate::paint_scheduler::{PaintMode, PaintScheduler};

mod accel;
mod browser_ops;
mod callbacks;
mod events;
mod lifecycle;
mod ops;
mod paint;
mod popup;
mod resize;
mod tasks;
pub(crate) use ops::{set_default_frame_rate, set_use_shared_textures};
pub(crate) use tasks::{post_close_and_collect, post_set_hidden};

// A word-sized handle must never fall back to AtomicCell's global-lock
// path: it is read on every paint.
const _: () = assert!(AtomicCell::<platform_ops::SurfaceHandle>::is_lock_free());

/// The document a browser is left showing once its navigation is abandoned.
const BLANK: &str = "about:blank";

const STATE_NORMAL: i32 = 0;
const STATE_PENDING_RESET: i32 = 1;
const STATE_RECREATING: i32 = 2;

// Process-wide defaults set once at startup by `WebOverlay::start`; consumed
// by Inner::cef_create_browser when building WindowInfo + BrowserSettings.
/// Zero until a display reports a refresh; CEF then runs at its own default
/// rather than at a rate this process invented.
static DEFAULT_FRAME_RATE: AtomicI32 = AtomicI32::new(0);
static PAINT_MODE: OnceLock<PaintMode> = OnceLock::new();

/// Which navigation this browser's pixels belong to.
enum Painting {
    /// No navigation has been issued.
    None,
    /// `navigation` was issued for `base`, and no main-frame load under `base`
    /// has finished; the pixels this browser produces are another document's.
    Awaiting {
        navigation: jfn_bringup::Navigation,
        base: String,
    },
    /// A main-frame load under `base` finished; every frame produced from here
    /// on is that navigation's document.
    Loaded {
        navigation: jfn_bringup::Navigation,
        base: String,
    },
}

impl Painting {
    /// The state after a main-frame load of `url` finished: a load that is a
    /// page of the awaited base promotes it, and every other load leaves the
    /// state alone.
    fn loaded(self, url: &str) -> Painting {
        match self {
            Painting::Awaiting { navigation, base } if jfn_jellyfin::is_page_of(&base, url) => {
                Painting::Loaded { navigation, base }
            }
            other => other,
        }
    }

    /// The navigation a main-frame load of `url` belongs to.
    fn navigation_of(&self, url: &str) -> Option<jfn_bringup::Navigation> {
        match self {
            Painting::Awaiting { navigation, base } | Painting::Loaded { navigation, base }
                if jfn_jellyfin::is_page_of(base, url) =>
            {
                Some(*navigation)
            }
            _ => None,
        }
    }

    /// Whether this browser is painting `navigation`.
    fn names(&self, navigation: jfn_bringup::Navigation) -> bool {
        match self {
            Painting::Awaiting {
                navigation: live, ..
            }
            | Painting::Loaded {
                navigation: live, ..
            } => *live == navigation,
            Painting::None => false,
        }
    }

    /// The navigation a frame produced now can witness.
    fn witness(&self) -> Option<jfn_bringup::Navigation> {
        match self {
            Painting::Loaded { navigation, .. } => Some(*navigation),
            Painting::None | Painting::Awaiting { .. } => None,
        }
    }
}

/// The browser handle and the size last applied to it, under one lock: a size
/// derived while no browser exists is never recorded as applied.
pub(crate) struct BrowserState {
    pub(crate) browser: Option<Browser>,
    pub(crate) applied: Option<ViewSize>,
}

pub(crate) struct Inner {
    // identity / state queries (slice 1)
    name: Mutex<String>,
    closed: AtomicBool,
    close_mtx: Mutex<()>,
    close_cv: Condvar,
    /// Which navigation this browser's pixels belong to, written on TID_UI
    /// alone — by the requests bring-up produces and by the main-frame load
    /// callback — and read on every paint.
    painting: Mutex<Painting>,

    // Stored cef::Browser captured at LifeSpanHandler::on_after_created.
    // All CEF host/frame ops on TID_UI route through this; dropped on
    // OnBeforeClose.
    browser: Mutex<BrowserState>,
    // Pending RunContextMenuCallback — held while a context menu is open.
    pending_menu_callback: Mutex<Option<RunContextMenuCallback>>,
    // The one context-menu session slot for this browser.
    menu: Mutex<MenuOwnership>,
    // Opaque per-layer surface handle (PlatformSurface*); passed back to the
    // C++ platform vtable for surface_resize / present / popup.
    surface: AtomicCell<platform_ops::SurfaceHandle>,

    // logical/physical dims (slice 3)
    width: AtomicI32,
    height: AtomicI32,
    physical_w: AtomicI32,
    physical_h: AtomicI32,

    paint_scheduler: PaintScheduler,

    // frame rate (slice 3): configured and last applied
    pub(crate) frame_rate: AtomicI32,
    current_frame_rate: AtomicI32,

    // resize-debounce (slice 3)
    resize_scheduled: AtomicBool,
    last_was_resized_ns: AtomicI64,

    // popup state (slice 4). Owned 1:1 with the platform surface; each
    // CefLayer owns its popup on the platform side. Two-phase reveal: rect
    // arrives via OnPopupSize, options via the "popupOptions" renderer IPC;
    // try_show_popup fires when popup_visible + size_received + options_received.
    popup: Mutex<PopupState>,
    dropdown: jfn_platform_abi::MenuDelivery,

    // lifecycle / reset state machine (slice 5)
    state: AtomicI32,
    pending_url: Mutex<String>,
    has_browser: AtomicBool,
    pending_internal_reset: AtomicBool,

    // app-level callback slots, stored as boxed closures.
    message_handler: Mutex<Option<Box<MessageFn>>>,
    created_callback: Mutex<Option<Box<CreatedFn>>>,
    before_close_callback: Mutex<Option<Box<BeforeCloseFn>>>,
    context_menu_builder: Mutex<Option<Box<ContextBuilderFn>>>,
    context_menu_dispatcher: Mutex<Option<Box<ContextDispatcherFn>>>,
}

// Typed closure signatures stored in each callback slot. `*mut c_void` args
// stay raw because callers may want to receive cef-rs handles or C++
// CefRefPtr objects depending on which side installed the handler.
pub(crate) type MessageFn = dyn Fn(BrowserMessage) -> bool + Send + Sync;
pub type CreatedFn = dyn Fn() + Send + Sync;
pub type BeforeCloseFn = dyn Fn() + Send + Sync;
pub type ContextBuilderFn = dyn Fn(*mut c_void) + Send + Sync;
pub type ContextDispatcherFn = dyn Fn(c_int) -> bool + Send + Sync;

#[derive(Default)]
struct PopupState {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    visible: bool,
    options: Vec<String>,
    selected_idx: i32,
    // Option indices an arrow key can land on (disabled/optgroup-disabled
    // excluded). Used to drive CEF's own popup to the chosen row.
    selectable: Vec<i32>,
    // Bottom-left corner of the <select> element in view coordinates.
    anchor: Option<(i32, i32)>,
    size_received: bool,
    options_received: bool,
}

// SAFETY: `Inner` is not auto-Send/Sync only because of the CEF ref-counted
// handles it stores (`Browser`, `RunContextMenuCallback`); those live behind
// `Inner`'s own mutexes and CEF ref-counts them atomically.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Inner {
    pub(crate) fn new() -> Arc<Self> {
        let paint_scheduler = PAINT_MODE
            .get_or_init(|| PaintMode::new(false))
            .make_scheduler();
        Arc::new(Self {
            name: Mutex::new(String::new()),
            closed: AtomicBool::new(false),
            close_mtx: Mutex::new(()),
            close_cv: Condvar::new(),
            painting: Mutex::new(Painting::None),
            browser: Mutex::new(BrowserState {
                browser: None,
                applied: None,
            }),
            pending_menu_callback: Mutex::new(None),
            menu: Mutex::new(MenuOwnership::default()),
            surface: AtomicCell::new(platform_ops::SurfaceHandle::NONE),
            width: AtomicI32::new(0),
            height: AtomicI32::new(0),
            physical_w: AtomicI32::new(0),
            physical_h: AtomicI32::new(0),
            paint_scheduler,
            frame_rate: AtomicI32::new(0),
            current_frame_rate: AtomicI32::new(0),
            resize_scheduled: AtomicBool::new(false),
            last_was_resized_ns: AtomicI64::new(0),
            popup: Mutex::new(PopupState {
                selected_idx: -1,
                ..PopupState::default()
            }),
            dropdown: jfn_platform_abi::menu_delivery(jfn_platform_abi::MenuKind::Dropdown),
            state: AtomicI32::new(STATE_NORMAL),
            pending_url: Mutex::new(String::new()),
            has_browser: AtomicBool::new(false),
            pending_internal_reset: AtomicBool::new(false),
            message_handler: Mutex::new(None),
            created_callback: Mutex::new(None),
            before_close_callback: Mutex::new(None),
            context_menu_builder: Mutex::new(None),
            context_menu_dispatcher: Mutex::new(None),
        })
    }

    pub(crate) fn set_name(&self, name: &str) {
        *self.name.lock() = name.to_owned();
    }

    fn name_str(&self) -> String {
        self.name.lock().clone()
    }

    pub(crate) fn set_surface(&self, surface: platform_ops::SurfaceHandle) {
        self.surface.store(surface);
    }

    fn surface_handle(&self) -> platform_ops::SurfaceHandle {
        self.surface.load()
    }

    /// The strip this browser's view was sized below, logical pixels. Zero
    /// until a size has been applied to a live browser.
    pub(crate) fn view_top(&self) -> c_int {
        self.browser
            .lock()
            .applied
            .map_or(0, |size| size.logical_top)
    }

    /// Hand `size` to the platform surface and, when a browser exists, to CEF.
    /// A size derived before the browser exists is applied but never recorded,
    /// so the next reconcile applies it again once the browser is there.
    pub(crate) fn apply_view_size(self: &Arc<Self>, size: ViewSize) {
        {
            let mut state = self.browser.lock();
            if state.browser.is_none() {
                state.applied = None;
            } else if state.applied == Some(size) {
                return;
            } else {
                state.applied = Some(size);
            }
        }
        self.resize(
            size.logical.w,
            size.logical.h,
            size.physical.w,
            size.physical.h,
            size.logical_top,
            size.physical_top,
        );
    }

    pub(crate) fn menu_open(&self) -> Option<Session> {
        self.menu.lock().open()
    }

    pub(crate) fn menu_resolve(&self, session: Session) -> bool {
        self.menu.lock().resolve(session)
    }

    pub(crate) fn menu_reset(&self) {
        self.menu.lock().reset();
    }

    pub(crate) fn set_message_handler(&self, f: Option<Box<MessageFn>>) {
        *self.message_handler.lock() = f;
    }
    pub(crate) fn set_created_callback(&self, f: Option<Box<CreatedFn>>) {
        *self.created_callback.lock() = f;
    }
    pub(crate) fn set_context_menu_builder(&self, f: Option<Box<ContextBuilderFn>>) {
        *self.context_menu_builder.lock() = f;
    }
    pub(crate) fn set_context_menu_dispatcher(&self, f: Option<Box<ContextDispatcherFn>>) {
        *self.context_menu_dispatcher.lock() = f;
    }
}

/// A frame this browser produced that did not reach the screen is replaced by
/// one this asks for: the view is invalidated, and where the host drives
/// frames, the next one is requested.
impl Inner {
    /// Bring-up issued `navigation` for `base`, which is stored verbatim: it is
    /// already the canonical base, and reducing it again drops the path a
    /// subpath-hosted server lives under. TID_UI only.
    pub(crate) fn set_navigation(&self, navigation: jfn_bringup::Navigation, base: &str) {
        *self.painting.lock() = Painting::Awaiting {
            navigation,
            base: base.to_owned(),
        };
    }

    /// A main-frame load of `url` finished. It names the navigation only when
    /// `url` is a page of that navigation's base, so the blank document a
    /// browser is created with, an `about:` URL, and the page the previous
    /// navigation left behind each name nothing.
    pub(crate) fn note_main_frame_loaded(&self, url: &str) {
        let mut painting = self.painting.lock();
        let previous = std::mem::replace(&mut *painting, Painting::None);
        *painting = previous.loaded(url);
    }

    /// Bring-up abandoned `navigation`: this browser stops stamping frames with
    /// it and is left showing [`BLANK`]. Identity while this browser is painting
    /// a different navigation. TID_UI only, so the load this decides cannot land
    /// after a later navigation's.
    pub(crate) fn abandon_navigation(&self, navigation: jfn_bringup::Navigation) {
        {
            let mut painting = self.painting.lock();
            if !painting.names(navigation) {
                return;
            }
            *painting = Painting::None;
        }
        self.load_url(BLANK);
    }

    /// The navigation a frame produced now can witness, and `None` until a
    /// requested document has finished loading.
    pub(crate) fn witness_navigation(&self) -> Option<jfn_bringup::Navigation> {
        self.painting.lock().witness()
    }

    /// The navigation a main-frame load of `url` belongs to, for charging that
    /// load's failure; `None` when `url` is a page of no navigation this
    /// browser was asked for.
    pub(crate) fn load_navigation(&self, url: &str) -> Option<jfn_bringup::Navigation> {
        self.painting.lock().navigation_of(url)
    }
}

impl Inner {
    /// This browser as the producer a frame it made names.
    pub(crate) fn frame_source(self: &Arc<Self>) -> Arc<dyn jfn_platform_abi::FrameSource> {
        Arc::clone(self) as Arc<dyn jfn_platform_abi::FrameSource>
    }
}

impl jfn_platform_abi::FrameSource for Inner {
    fn request_frame(&self) {
        if !self.browser_alive() {
            return;
        }
        self.invalidate_view();
        let external_bf = jfn_platform_abi::try_get()
            .and_then(|p| p.cef_host())
            .is_some_and(|h| h.external_begin_frame());
        if external_bf {
            self.send_external_begin_frame();
        }
    }
}

pub(crate) fn now_ns() -> i64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    Instant::now()
        .duration_since(*ORIGIN.get_or_init(Instant::now))
        .as_nanos() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBPATH_BASE: &str = "https://host/jellyfin";

    fn awaiting(base: &str) -> Painting {
        Painting::Awaiting {
            navigation: jfn_bringup::Navigation::for_test(1),
            base: base.to_owned(),
        }
    }

    #[test]
    fn a_subpath_hosted_navigation_is_promoted_by_its_own_page() {
        let painting = awaiting(SUBPATH_BASE).loaded("https://host/jellyfin/web/index.html");
        assert_eq!(
            painting.witness(),
            Some(jfn_bringup::Navigation::for_test(1))
        );
    }

    #[test]
    fn a_page_of_another_base_promotes_nothing() {
        let painting = awaiting(SUBPATH_BASE).loaded("https://host/web/index.html");
        assert_eq!(painting.witness(), None);
    }

    #[test]
    fn a_failed_load_of_a_subpath_hosted_page_charges_its_navigation() {
        assert_eq!(
            awaiting(SUBPATH_BASE).navigation_of("https://host/jellyfin/web/index.html"),
            Some(jfn_bringup::Navigation::for_test(1))
        );
    }
}
