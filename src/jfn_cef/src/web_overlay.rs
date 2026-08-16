//! The process's one web overlay: jellyfin-web's browser, the platform
//! surface it paints into, and the size it is driven at.
//!
//! It owns its surface and its browser handle; every caller that drives it
//! holds a [`WebOverlay`] clone. Its size is a pure function of the window
//! snapshot and the strip the shell overlay publishes, and the browser is
//! created as soon as that function yields one.

pub mod size;

use std::sync::Arc;
use std::time::Duration;

use cef::rc::Rc;
use cef::{ImplTask, Task, ThreadId, WrapTask, post_task, wrap_task};
use parking_lot::Mutex;

use jfn_platform_abi::Visibility;

use crate::client::{Inner, post_close_and_collect, post_set_hidden};
use jfn_platform_abi::SurfaceSize;
use size::view_size;

/// Bound on the TID_UI close-and-collect round trip. A TID_UI that is already
/// dead never runs the posted task, and shutdown must still reach
/// `wake_main_loop`.
const CLOSE_COLLECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct WebOverlayConfig {
    pub frame_rate: f64,
    pub shared_textures: bool,
}

struct Overlay {
    client: Arc<Inner>,
    surface: jfn_platform_abi::SurfaceHandle,
    /// Set once the browser has been asked for; the request is made exactly
    /// once, at the first size with a positive width and height.
    created: Mutex<bool>,
}

#[derive(Clone)]
pub struct WebOverlay {
    inner: Arc<Overlay>,
}

/// The started overlay, for callers that hold no handle. Written once by
/// [`WebOverlay::start`].
static STARTED: Mutex<Option<WebOverlay>> = Mutex::new(None);

impl WebOverlay {
    /// Allocates the platform surface, installs the jfn-input web sink and
    /// jellyfin-web's message handlers, subscribes to the window snapshot and
    /// to [`jfn_input::on_shell_state`], and creates the browser as soon as
    /// both yield a size with a positive width and height.
    pub fn start(config: WebOverlayConfig) -> WebOverlay {
        let frame_rate = if config.frame_rate > 0.0 {
            (config.frame_rate + 0.5) as i32
        } else {
            0
        };
        crate::client::set_default_frame_rate(frame_rate);
        crate::client::set_use_shared_textures(config.shared_textures);

        let surface = jfn_platform_abi::get().alloc_surface(Visibility::Hidden);
        jfn_platform_abi::stack::occupy(jfn_platform_abi::Plane::WebOverlay, surface);
        let client = Inner::new();
        client.set_name("web");
        client.set_surface(surface);

        let overlay = WebOverlay {
            inner: Arc::new(Overlay {
                client,
                surface,
                created: Mutex::new(false),
            }),
        };

        crate::business_web::install(&overlay);
        crate::web_input::install(overlay.clone());

        *STARTED.lock() = Some(overlay.clone());
        jfn_platform_abi::subscribe_window_changed(sync_started);
        jfn_input::on_shell_state(Box::new({
            let overlay = overlay.clone();
            move |_| overlay.sync()
        }));

        // Subscribing runs every request bring-up has already produced, so a
        // probe or a navigation issued before CEF existed is executed now.
        jfn_bringup::subscribe(run_requests);
        overlay.sync();
        overlay
    }

    pub(crate) fn client(&self) -> &Arc<Inner> {
        &self.inner.client
    }

    pub fn surface(&self) -> jfn_platform_abi::SurfaceHandle {
        self.inner.surface
    }

    /// Posts [`WebOverlay::sync_on_ui`] onto TID_UI. Its callers include the
    /// window-snapshot listener, which the compositor's own dispatch loop runs
    /// inline.
    fn sync(&self) {
        let mut task = SyncTask::new(self.clone());
        let _ = post_task(ThreadId::UI, Some(&mut task));
    }

    /// Re-derives the size, creates the browser at the first size, and shows
    /// the surface — on TID_UI, so every acknowledgement it awaits is delivered
    /// by a thread that is not this one.
    fn sync_on_ui(&self) {
        let Some(state) = jfn_input::shell_state() else {
            return;
        };
        let snapshot = jfn_platform_abi::get().window_owner().source().snapshot();
        let Some(size) = view_size(&snapshot, state.reserved_strip) else {
            return;
        };
        self.inner.client.apply_view_size(size);
        self.ensure_browser(size);
    }

    /// Create the browser once, with the view already sized: CEF reads the view
    /// rect during creation, and a zero-sized one aborts Chromium on the first
    /// navigation.
    fn ensure_browser(&self, size: SurfaceSize) {
        let mut created = self.inner.created.lock();
        if *created {
            return;
        }
        *created = true;
        jfn_logging::log(
            jfn_logging::CATEGORY_CEF,
            jfn_logging::LEVEL_INFO,
            &format!(
                "CreateBrowser(web) logical={}x{}+{} physical={}x{}+{} scale={}",
                size.extent.logical().w,
                size.extent.logical().h,
                size.logical_top,
                size.extent.physical().w,
                size.extent.physical().h,
                size.physical_top,
                size.extent.scale(),
            ),
        );
        self.inner.client.create("");
        let _ = self.set_visibility(Visibility::Shown);
    }

    /// Shown once the browser exists, hidden before it closes. This surface's
    /// visibility is written here and nowhere else.
    fn set_visibility(&self, visibility: Visibility) -> Visibility {
        jfn_platform_abi::get()
            .set_surface_visibility(self.inner.surface, visibility)
            .acknowledged()
    }

    pub fn set_refresh_rate(&self, hz: f64) {
        if hz <= 0.0 {
            return;
        }
        crate::client::set_default_frame_rate((hz + 0.5) as i32);
        self.inner.client.set_refresh_rate(hz);
    }

    /// Thread-agnostic; posts a TID_UI task that calls `WasHidden(hidden)`.
    pub fn set_hidden(&self, hidden: bool) {
        post_set_hidden(Arc::clone(&self.inner.client), hidden);
    }

    /// No-op where the platform does not drive frames itself.
    pub fn send_external_begin_frame(&self) {
        self.inner.client.send_external_begin_frame();
    }

    /// Loads `url` as `navigation` and stamps every frame produced afterwards
    /// with it. TID_UI only.
    fn navigate(&self, navigation: jfn_bringup::Navigation, url: &str) {
        self.inner.client.set_navigation(navigation, url);
        self.inner.client.load_url(url);
    }

    /// Drops `navigation`: the browser stops naming it and loads a blank
    /// document, so the page it loaded is gone from behind the connect screen.
    /// TID_UI only.
    fn abandon(&self, navigation: jfn_bringup::Navigation) {
        self.inner.client.abandon_navigation(navigation);
    }

    pub fn exec_js(&self, js: &str) {
        self.inner.client.exec_js(js);
    }

    /// Posts one TID_UI close and blocks until `OnBeforeClose` has fired.
    /// Callable from any non-TID_UI thread.
    pub fn close_blocking(&self) {
        if !*self.inner.created.lock() {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded::<Arc<Inner>>(1);
        post_close_and_collect(Arc::clone(&self.inner.client), tx);
        match rx.recv_timeout(CLOSE_COLLECT_TIMEOUT) {
            Ok(inner) => inner.wait_for_close(),
            Err(e) => jfn_logging::log(
                jfn_logging::CATEGORY_CEF,
                jfn_logging::LEVEL_WARN,
                &format!("close wait set never arrived: {e}"),
            ),
        }
    }

    /// Closes the browser, frees the platform surface, and drops the overlay.
    pub fn shutdown(self) {
        *STARTED.lock() = None;
        let _ = self.set_visibility(Visibility::Hidden);
        self.close_blocking();
        if !self.inner.surface.is_none() {
            jfn_platform_abi::stack::vacate(jfn_platform_abi::Plane::WebOverlay);
            jfn_platform_abi::get().free_surface(self.inner.surface);
        }
    }
}

/// Subscribed into the window snapshot at [`WebOverlay::start`]; posts the
/// overlay's sync and returns, so the thread that publishes the change waits
/// for nothing.
fn sync_started() {
    if let Some(overlay) = STARTED.lock().clone() {
        overlay.sync();
    }
}

/// Subscribed into bring-up at [`WebOverlay::start`]; posts the drain onto
/// TID_UI and returns, so the thread that advanced bring-up executes nothing
/// itself. A post that never runs leaves the requests queued for the next one.
fn run_requests() {
    let mut task = RequestsTask::new();
    let _ = post_task(ThreadId::UI, Some(&mut task));
}

/// Runs every request bring-up has produced, oldest first, including those it
/// produced before CEF existed. TID_UI only: setting a navigation and loading
/// its URL, and dropping a navigation and blanking its document, are each
/// indivisible against every other request.
fn run_requests_on_ui() {
    let Some(overlay) = STARTED.lock().clone() else {
        return;
    };
    for request in jfn_bringup::take_requests() {
        match request {
            jfn_bringup::Request::Probe { cycle, url } => probe(cycle, &url),
            jfn_bringup::Request::Navigate { navigation, url } => {
                overlay.navigate(navigation, &url);
            }
            jfn_bringup::Request::Abandon { navigation } => overlay.abandon(navigation),
        }
    }
}

/// The probe answers on the CEF UI thread, once CEF exists; its outcome reaches
/// bring-up as the cycle it cites and nothing else.
fn probe(cycle: u64, url: &str) {
    let url = url.to_owned();
    crate::ready::on_cef_ready(Box::new(move || {
        let probe = crate::server_probe::Probe::start(
            &url,
            Box::new(move |resolved| {
                jfn_bringup::advance(match resolved {
                    Some(base) => jfn_bringup::Event::Resolved { cycle, base },
                    None => jfn_bringup::Event::Unresolved { cycle },
                });
            }),
        );
        *PROBE.lock() = Some(probe);
    }));
}

/// The in-flight probe, kept alive for the length of the request it made.
static PROBE: Mutex<Option<crate::server_probe::Probe>> = Mutex::new(None);

wrap_task! {
    struct SyncTask {
        overlay: WebOverlay,
    }
    impl Task {
        fn execute(&self) {
            self.overlay.sync_on_ui();
        }
    }
}

wrap_task! {
    struct RequestsTask {
    }
    impl Task {
        fn execute(&self) {
            run_requests_on_ui();
        }
    }
}
