use cef::{Browser, RunContextMenuCallback};
use crossbeam_channel::{Receiver, Sender};
use crossbeam_utils::atomic::AtomicCell;
use parking_lot::Mutex;
use std::convert::Infallible;
use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use crate::ipc::BrowserMessage;
use crate::menu_ownership::{MenuOwnership, Session};
use crate::web_overlay::WebOverlaySurface;

use crate::frame_rate::FrameRate;
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
pub(crate) use tasks::{post_close_and_wait, post_set_hidden};

const BLANK: &str = "about:blank";

enum PendingNavigation {
    Page {
        navigation: crate::Navigation,
        url: String,
    },
    Blank,
}

pub(crate) struct DeferredNavigation {
    pending: Mutex<Vec<PendingNavigation>>,
    pub(crate) on_event: std::sync::OnceLock<crate::WebEventHandler>,
}

impl DeferredNavigation {
    pub(crate) fn new() -> Arc<DeferredNavigation> {
        Arc::new(Self {
            pending: Mutex::new(Vec::new()),
            on_event: std::sync::OnceLock::new(),
        })
    }

    pub(crate) fn navigate(&self, navigation: crate::Navigation, url: &str) {
        let mut pending = self.pending.lock();
        pending.clear();
        pending.push(PendingNavigation::Page {
            navigation,
            url: url.to_owned(),
        });
    }

    pub(crate) fn abandon(&self, navigation: crate::Navigation) {
        let mut pending = self.pending.lock();
        if matches!(
            pending.as_slice(),
            [PendingNavigation::Page {
                navigation: pending_navigation,
                ..
            }] if *pending_navigation == navigation
        ) {
            pending.clear();
            pending.push(PendingNavigation::Blank);
        }
    }

    fn blank_if_absent(&self) {
        let mut pending = self.pending.lock();
        if pending.is_empty() {
            pending.push(PendingNavigation::Blank);
        }
    }
}

enum Painting {
    None,
    Awaiting {
        navigation: crate::Navigation,
        base: String,
    },
    Loaded {
        navigation: crate::Navigation,
        base: String,
        presented: bool,
    },
}

impl Painting {
    fn loaded(self, url: &str) -> Painting {
        match self {
            Painting::Awaiting { navigation, base } if jfn_jellyfin::is_page_of(&base, url) => {
                Painting::Loaded {
                    navigation,
                    base,
                    presented: false,
                }
            }
            other => other,
        }
    }

    fn navigation_of(&self, url: &str) -> Option<crate::Navigation> {
        match self {
            Painting::Awaiting { navigation, base }
            | Painting::Loaded {
                navigation, base, ..
            } if jfn_jellyfin::is_page_of(base, url) => Some(*navigation),
            _ => None,
        }
    }

    fn names(&self, navigation: crate::Navigation) -> bool {
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

    fn mark_presented(&mut self, navigation: crate::Navigation) -> bool {
        match self {
            Painting::Loaded {
                navigation: live,
                presented,
                ..
            } if *live == navigation && !*presented => {
                *presented = true;
                true
            }
            _ => false,
        }
    }

    fn witness(&self) -> Option<crate::Navigation> {
        match self {
            Painting::Loaded {
                navigation,
                presented: false,
                ..
            } => Some(*navigation),
            _ => None,
        }
    }
}

pub(crate) struct BrowserState {
    pub(crate) browser: Option<Browser>,
    pub(crate) applied: Option<jfn_platform_abi::SurfaceSize>,
}

pub(crate) struct Inner {
    pub(crate) session: Arc<crate::runtime::Session>,
    name: Mutex<String>,
    _owner_connected: Sender<Infallible>,
    owner_disconnected: Receiver<Infallible>,
    painting: Mutex<Painting>,

    browser: Mutex<BrowserState>,
    pending_menu_callback: Mutex<Option<RunContextMenuCallback>>,
    menu: Mutex<MenuOwnership>,
    surface: Arc<WebOverlaySurface>,

    width: AtomicI32,
    height: AtomicI32,
    scale: AtomicCell<Option<jfn_platform_abi::Scale>>,

    pub(super) paint_mode: PaintMode,
    paint_scheduler: PaintScheduler,

    pub(crate) frame_rate: AtomicCell<Option<FrameRate>>,

    resize_scheduled: AtomicBool,
    last_was_resized_ns: AtomicI64,

    popup: Mutex<PopupState>,

    deferred_navigation: Arc<DeferredNavigation>,

    message_handler: Mutex<Option<Box<MessageFn>>>,
    created_callback: Mutex<Option<Arc<CreatedFn>>>,
    context_menu_builder: Mutex<Option<Box<ContextBuilderFn>>>,
    context_menu_dispatcher: Mutex<Option<Box<ContextDispatcherFn>>>,
}

pub(crate) type MessageFn = dyn Fn(BrowserMessage) -> bool + Send + Sync;
pub type CreatedFn = dyn Fn() + Send + Sync;
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
    selectable: Vec<i32>,
    anchor: Option<(i32, i32)>,
    size_received: bool,
    options_received: bool,
}

unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Inner {
    pub(crate) fn report_presented(
        &self,
        navigation: crate::Navigation,
        presented: jfn_gpu_paint::Presented,
    ) {
        let first = self.painting.lock().mark_presented(navigation);
        if first {
            self.report_web_event(crate::WebEvent::FramePresented(
                crate::NavigationPresented::witnessed(navigation, presented),
            ));
        }
    }

    pub(crate) fn report_web_event(&self, event: crate::WebEvent) {
        self.session.dispatch(|| {
            if let Some(handler) = self.deferred_navigation.on_event.get() {
                handler(event);
            }
        });
    }

    pub(crate) fn new(
        session: Arc<crate::runtime::Session>,
        surface: Arc<WebOverlaySurface>,
        deferred_navigation: Arc<DeferredNavigation>,
        paint_mode: PaintMode,
        frame_rate: Option<FrameRate>,
    ) -> Arc<Self> {
        let paint_scheduler = paint_mode.make_scheduler();
        let (owner_connected, owner_disconnected) = crossbeam_channel::unbounded();
        Arc::new(Self {
            session,
            name: Mutex::new(String::new()),
            _owner_connected: owner_connected,
            owner_disconnected,
            painting: Mutex::new(Painting::None),
            browser: Mutex::new(BrowserState {
                browser: None,
                applied: None,
            }),
            pending_menu_callback: Mutex::new(None),
            menu: Mutex::new(MenuOwnership::default()),
            surface,
            width: AtomicI32::new(0),
            height: AtomicI32::new(0),
            scale: AtomicCell::new(None),
            paint_mode,
            paint_scheduler,
            frame_rate: AtomicCell::new(frame_rate),
            resize_scheduled: AtomicBool::new(false),
            last_was_resized_ns: AtomicI64::new(0),
            popup: Mutex::new(PopupState {
                selected_idx: -1,
                ..PopupState::default()
            }),
            deferred_navigation,
            message_handler: Mutex::new(None),
            created_callback: Mutex::new(None),
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

    pub(crate) fn owner_disconnection(&self) -> Receiver<Infallible> {
        self.owner_disconnected.clone()
    }

    pub(crate) fn surface(&self) -> &WebOverlaySurface {
        &self.surface
    }

    pub(crate) fn view_top(&self) -> c_int {
        self.browser
            .lock()
            .applied
            .map_or(0, |size| size.logical_top)
    }

    pub(crate) fn apply_view_size(self: &Arc<Self>, size: jfn_platform_abi::SurfaceSize) {
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
        self.resize(size);
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
    pub(crate) fn set_created_callback(&self, f: Option<Arc<CreatedFn>>) {
        *self.created_callback.lock() = f;
    }
    pub(crate) fn set_context_menu_builder(&self, f: Option<Box<ContextBuilderFn>>) {
        *self.context_menu_builder.lock() = f;
    }
    pub(crate) fn set_context_menu_dispatcher(&self, f: Option<Box<ContextDispatcherFn>>) {
        *self.context_menu_dispatcher.lock() = f;
    }
}

impl Inner {
    pub(crate) fn navigate(&self, navigation: crate::Navigation, url: &str) {
        self.deferred_navigation.navigate(navigation, url);
        *self.painting.lock() = Painting::Awaiting {
            navigation,
            base: url.to_owned(),
        };
        self.deliver_deferred_navigation();
    }

    pub(crate) fn note_main_frame_loaded(&self, url: &str) {
        let mut painting = self.painting.lock();
        let previous = std::mem::replace(&mut *painting, Painting::None);
        *painting = previous.loaded(url);
    }

    pub(crate) fn abandon_navigation(&self, navigation: crate::Navigation) {
        self.deferred_navigation.abandon(navigation);
        let matched_live_navigation = {
            let mut painting = self.painting.lock();
            if painting.names(navigation) {
                *painting = Painting::None;
                true
            } else {
                false
            }
        };
        if matched_live_navigation {
            self.deferred_navigation.blank_if_absent();
        }
        self.deliver_deferred_navigation();
    }

    pub(crate) fn witness_navigation(&self) -> Option<crate::Navigation> {
        self.painting.lock().witness()
    }

    pub(crate) fn load_navigation(&self, url: &str) -> Option<crate::Navigation> {
        self.painting.lock().navigation_of(url)
    }
}

impl Inner {
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
        let external_bf = jfn_platform_abi::try_lease()
            .is_some_and(|p| p.cef_host().is_some_and(|h| h.external_begin_frame()));
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
            navigation: crate::Navigation::new(1),
            base: base.to_owned(),
        }
    }

    #[test]
    fn presentation_is_reported_once_and_only_for_the_loaded_navigation() {
        let navigation = crate::Navigation::new(1);
        let mut painting = awaiting(SUBPATH_BASE);
        assert!(!painting.mark_presented(navigation));
        let mut painting = painting.loaded("https://host/jellyfin/web/index.html");
        assert!(!painting.mark_presented(crate::Navigation::new(2)));
        assert_eq!(painting.witness(), Some(navigation));
        assert!(painting.mark_presented(navigation));
        assert_eq!(painting.witness(), None);
        assert!(!painting.mark_presented(navigation));
        assert_eq!(painting.navigation_of(SUBPATH_BASE), Some(navigation));
    }

    #[test]
    fn a_subpath_hosted_navigation_is_promoted_by_its_own_page() {
        let painting = awaiting(SUBPATH_BASE).loaded("https://host/jellyfin/web/index.html");
        assert_eq!(painting.witness(), Some(crate::Navigation::new(1)));
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
            Some(crate::Navigation::new(1))
        );
    }
}
