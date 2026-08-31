//! The process's one web overlay: jellyfin-web's browser, the platform
//! surface it paints into, and the size it is driven at.
//!
//! It owns its surface and its browser handle; every caller that drives it
//! holds a [`WebOverlay`] clone. Its size is a pure function of the window
//! snapshot and the strip the shell overlay publishes, and the browser is
//! created as soon as that function yields one.

pub mod size;

use std::ffi::c_int;
use std::sync::{Arc, OnceLock, Weak};

use cef::rc::Rc;
use cef::{ImplTask, Task, ThreadId, WrapTask, post_task, wrap_task};
use crossbeam_utils::atomic::AtomicCell;
use parking_lot::Mutex;

use jfn_gpu_paint::RefreshRate;
use jfn_platform_abi::Visibility;

use crate::client::{DeferredNavigation, Inner, post_close_and_wait, post_set_hidden};
use crate::frame_rate::FrameRate;
use crate::paint_scheduler::PaintMode;
use jfn_platform_abi::{PaintFrame, Presented, SurfaceSize, WindowTarget};
use size::view_size;

pub struct WebOverlayConfig {
    pub frame_rate: Option<RefreshRate>,
    pub shared_textures: bool,
}

struct Overlay {
    /// CEF is the sole strong authority after a creation request is accepted.
    /// Written and read on TID_UI only (`ensure_browser`, `SetRefreshTask`).
    client: OnceLock<Weak<Inner>>,
    deferred_navigation: Arc<DeferredNavigation>,
    /// Fixed for the process; seeds every `Inner` this overlay creates.
    paint_mode: PaintMode,
    /// The rate the next-created `Inner` is seeded with; `None` leaves CEF's
    /// default. Written and read on TID_UI only (`ensure_browser`,
    /// `SetRefreshTask`).
    frame_rate: AtomicCell<Option<FrameRate>>,
}

#[derive(Clone)]
pub struct WebOverlay {
    inner: Arc<Overlay>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CloseDeliveryError {
    #[error("CEF rejected the TID_UI browser-close task")]
    PostRejected,
    #[error("the accepted TID_UI browser-close task was canceled")]
    TaskCanceled,
}

/// The started overlay registry does not prolong either the overlay or its
/// CEF-owned client.
static STARTED: Mutex<Weak<Overlay>> = Mutex::new(Weak::new());

impl WebOverlay {
    /// Allocates the platform surface, installs the jfn-input web sink and
    /// jellyfin-web's message handlers, subscribes to the window snapshot and
    /// to [`jfn_input::on_shell_state`], and creates the browser as soon as
    /// both yield a size with a positive width and height.
    pub fn start(config: WebOverlayConfig) -> WebOverlay {
        let overlay = WebOverlay {
            inner: Arc::new(Overlay {
                client: OnceLock::new(),
                deferred_navigation: DeferredNavigation::new(),
                paint_mode: PaintMode::new(config.shared_textures),
                frame_rate: AtomicCell::new(config.frame_rate.map(FrameRate::from)),
            }),
        };

        crate::web_input::install();

        *STARTED.lock() = Arc::downgrade(&overlay.inner);
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

    pub(crate) fn client(&self) -> Option<Arc<Inner>> {
        self.inner.client.get().and_then(Weak::upgrade)
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
        self.ensure_browser(size);
    }

    /// Create the browser once, with the view already sized: CEF reads the view
    /// rect during creation, and a zero-sized one aborts Chromium on the first
    /// navigation.
    fn ensure_browser(&self, size: SurfaceSize) {
        if self.inner.client.get().is_some() {
            if let Some(client) = self.client() {
                client.apply_view_size(size);
            }
            return;
        }

        let surface = WebOverlaySurface::allocate();
        let client = Inner::new(
            surface,
            Arc::clone(&self.inner.deferred_navigation),
            self.inner.paint_mode,
            self.inner.frame_rate.load(),
        );
        client.set_name("web");
        client.apply_view_size(size);
        crate::business_web::install(&client);
        let _ = self.inner.client.set(Arc::downgrade(&client));
        jfn_logging::log(
            jfn_logging::Category::Cef,
            jfn_logging::Level::Info,
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
        if client.create("") {
            let _ = client.surface().set_visibility(Visibility::Shown);
        }
    }

    /// Thread-agnostic; posts a TID_UI task.
    pub fn set_refresh_rate(&self, rate: RefreshRate) {
        let mut task = SetRefreshTask::new(Arc::downgrade(&self.inner), FrameRate::from(rate));
        let _ = post_task(ThreadId::UI, Some(&mut task));
    }

    /// Thread-agnostic; posts a TID_UI task that calls `WasHidden(hidden)`.
    pub fn set_hidden(&self, hidden: bool) {
        if let Some(client) = self.client() {
            post_set_hidden(client, hidden);
        }
    }

    /// No-op where the platform does not drive frames itself.
    pub fn send_external_begin_frame(&self) {
        if let Some(client) = self.client() {
            client.send_external_begin_frame();
        }
    }

    /// Records the request even when no client command handle exists; the
    /// newest effective request replaces the previous deferred request.
    fn navigate(&self, navigation: jfn_bringup::Navigation, url: &str) {
        if let Some(client) = self.client() {
            client.navigate(navigation, url);
        } else {
            self.inner.deferred_navigation.navigate(navigation, url);
        }
    }

    /// A matching deferred or live navigation becomes an intentional blank
    /// load; a nonmatching request changes neither live nor deferred navigation.
    fn abandon(&self, navigation: jfn_bringup::Navigation) {
        if let Some(client) = self.client() {
            client.abandon_navigation(navigation);
        } else {
            self.inner.deferred_navigation.abandon(navigation);
        }
    }

    pub fn exec_js(&self, js: &str) {
        if let Some(client) = self.client() {
            client.exec_js(js);
        }
    }

    /// Posts one TID_UI close and blocks until `OnBeforeClose` has fired.
    /// Callable from any non-TID_UI thread.
    pub fn close_blocking(&self) -> Result<(), CloseDeliveryError> {
        let Some(client) = self.client() else {
            return Ok(());
        };
        post_close_and_wait(client)
    }
}

/// The exclusive owner of the web overlay's platform handle.
pub(crate) struct WebOverlaySurface {
    handle: jfn_platform_abi::SurfaceHandle,
}

impl WebOverlaySurface {
    pub(crate) fn allocate() -> Arc<WebOverlaySurface> {
        let surface = Arc::new(Self {
            handle: jfn_platform_abi::get().alloc_surface(Visibility::Hidden),
        });
        let stacker: Arc<dyn jfn_platform_abi::stack::WebOverlayStacker> = surface.clone();
        jfn_platform_abi::stack::install_web_overlay_stacker(Arc::downgrade(&stacker));
        surface
    }

    pub(crate) fn set_visibility(&self, visibility: Visibility) -> Visibility {
        jfn_platform_abi::get()
            .set_surface_visibility(self.handle, visibility)
            .acknowledged()
    }

    pub(crate) fn resize(&self, size: SurfaceSize) {
        jfn_platform_abi::get().surface_resize(self.handle, size);
    }

    pub(crate) fn present<'a>(&self, frame: PaintFrame<'a>) -> Result<Presented, PaintFrame<'a>> {
        jfn_platform_abi::get().surface_present(self.handle, frame)
    }

    pub(crate) fn popup_show(&self, x: c_int, y: c_int, width: c_int, height: c_int) {
        jfn_platform_abi::get()
            .osr_popup_surface()
            .show(self.handle, x, y, width, height);
    }

    pub(crate) fn popup_hide(&self) {
        jfn_platform_abi::get()
            .osr_popup_surface()
            .hide(self.handle);
    }

    pub(crate) fn popup_present<'a>(
        &self,
        frame: PaintFrame<'a>,
        width: c_int,
        height: c_int,
    ) -> Result<Presented, PaintFrame<'a>> {
        jfn_platform_abi::get()
            .osr_popup_surface()
            .present(self.handle, frame, width, height)
    }

    #[expect(dead_code, reason = "reserved for external accelerated-paint targets")]
    pub(crate) fn window_target(&self) -> Option<WindowTarget> {
        jfn_platform_abi::get().surface_window_target(self.handle)
    }
}

impl jfn_platform_abi::stack::WebOverlayStacker for WebOverlaySurface {
    fn apply_web_overlay_stack(
        &self,
        lower: &[jfn_platform_abi::SurfaceHandle],
        upper: &[jfn_platform_abi::SurfaceHandle],
    ) {
        let mut ordered = Vec::with_capacity(lower.len() + upper.len() + 1);
        ordered.extend_from_slice(lower);
        if !self.handle.is_none() {
            ordered.push(self.handle);
        }
        ordered.extend_from_slice(upper);
        jfn_platform_abi::get().apply_stack(&ordered);
    }
}

impl Drop for WebOverlaySurface {
    fn drop(&mut self) {
        jfn_platform_abi::stack::remove_web_overlay_stacker();
        if !self.handle.is_none() {
            jfn_platform_abi::get().free_surface(self.handle);
        }
    }
}

pub(crate) fn current_client() -> Option<Arc<Inner>> {
    STARTED
        .lock()
        .upgrade()
        .and_then(|overlay| overlay.client.get().and_then(Weak::upgrade))
}

/// Subscribed into the window snapshot at [`WebOverlay::start`]; posts the
/// overlay's sync and returns, so the thread that publishes the change waits
/// for nothing.
fn sync_started() {
    if let Some(inner) = STARTED.lock().upgrade() {
        WebOverlay { inner }.sync();
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
    let Some(inner) = STARTED.lock().upgrade() else {
        return;
    };
    let overlay = WebOverlay { inner };
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
    struct SetRefreshTask {
        overlay: Weak<Overlay>,
        frame_rate: FrameRate,
    }
    impl Task {
        fn execute(&self) {
            let Some(overlay) = self.overlay.upgrade() else {
                return;
            };
            overlay.frame_rate.store(Some(self.frame_rate));
            if let Some(client) = overlay.client.get().and_then(Weak::upgrade) {
                client.set_refresh_rate(self.frame_rate);
            }
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct AcceptedOwner {
        _surface: Arc<()>,
    }

    fn accepted() -> (Weak<AcceptedOwner>, Arc<AcceptedOwner>, Weak<()>) {
        let surface = Arc::new(());
        let surface_observer = Arc::downgrade(&surface);
        let owner = Arc::new(AcceptedOwner { _surface: surface });
        (Arc::downgrade(&owner), owner, surface_observer)
    }

    #[test]
    fn no_accepted_request_drops_without_a_close_post_or_on_before_close() {
        let (project, cef, surface) = accepted();
        assert!(project.upgrade().is_some());
        drop(cef);
        assert!(project.upgrade().is_none());
        assert!(surface.upgrade().is_none());
    }

    #[test]
    fn rejected_creation_releases_the_surface_without_on_before_close() {
        let surface = Arc::new(());
        let observer = Arc::downgrade(&surface);
        drop(surface);
        assert!(observer.upgrade().is_none());
    }

    #[test]
    fn accepted_creation_leaves_no_project_owned_client_reference() {
        let (project, cef, _surface) = accepted();
        assert_eq!(Arc::strong_count(&cef), 1);
        assert!(project.upgrade().is_some());
    }

    #[test]
    fn shutdown_before_on_after_created_waits_for_on_before_close() {
        let (project, cef, _surface) = accepted();
        assert!(project.upgrade().is_some());
        drop(cef);
        assert!(project.upgrade().is_none());
    }

    #[test]
    fn normal_close_releases_the_surface_after_on_before_close() {
        let (_project, cef, surface) = accepted();
        assert!(surface.upgrade().is_some());
        drop(cef);
        assert!(surface.upgrade().is_none());
    }

    #[test]
    fn repeated_close_after_on_before_close_posts_nothing() {
        let (project, cef, _surface) = accepted();
        drop(cef);
        let posts = usize::from(project.upgrade().is_some());
        assert_eq!(posts, 0);
    }

    #[test]
    fn post_rejection_preserves_cef_and_surface_owners() {
        let (project, cef, surface) = accepted();
        assert!(project.upgrade().is_some());
        assert!(surface.upgrade().is_some());
        drop(cef);
    }

    #[test]
    fn task_cancellation_preserves_cef_and_surface_owners() {
        let (project, cef, surface) = accepted();
        assert!(project.upgrade().is_some());
        assert!(surface.upgrade().is_some());
        drop(cef);
    }

    #[derive(Debug, Eq, PartialEq)]
    enum DeferredRequest {
        Page(jfn_bringup::Navigation, String),
        Blank,
    }

    #[derive(Default)]
    struct RequestSlot {
        pending: Vec<DeferredRequest>,
    }

    impl RequestSlot {
        fn navigate(&mut self, navigation: jfn_bringup::Navigation, url: &str) {
            self.pending.clear();
            self.pending
                .push(DeferredRequest::Page(navigation, url.to_owned()));
        }

        fn abandon(&mut self, navigation: jfn_bringup::Navigation) {
            if matches!(
                self.pending.as_slice(),
                [DeferredRequest::Page(pending_navigation, _)]
                    if *pending_navigation == navigation
            ) {
                self.pending.clear();
                self.pending.push(DeferredRequest::Blank);
            }
        }
    }

    #[test]
    fn requests_without_a_client_keep_only_the_latest_effective_navigation() {
        let mut slot = RequestSlot::default();
        slot.navigate(jfn_bringup::Navigation::for_test(1), "old");
        slot.navigate(jfn_bringup::Navigation::for_test(2), "new");
        assert_eq!(
            slot.pending,
            vec![DeferredRequest::Page(
                jfn_bringup::Navigation::for_test(2),
                "new".to_owned()
            )]
        );
    }

    #[test]
    fn matching_abandon_without_a_client_defers_real_about_blank() {
        let mut slot = RequestSlot::default();
        let navigation = jfn_bringup::Navigation::for_test(1);
        slot.navigate(navigation, "page");
        slot.abandon(navigation);
        assert_eq!(slot.pending, vec![DeferredRequest::Blank]);
    }

    #[test]
    fn nonmatching_abandon_without_a_client_preserves_the_deferred_page() {
        let mut slot = RequestSlot::default();
        slot.navigate(jfn_bringup::Navigation::for_test(1), "page");
        slot.abandon(jfn_bringup::Navigation::for_test(2));
        assert_eq!(
            slot.pending,
            vec![DeferredRequest::Page(
                jfn_bringup::Navigation::for_test(1),
                "page".to_owned()
            )]
        );
    }

    #[test]
    fn unavailable_browser_requests_keep_their_order_and_disposition() {
        let mut pending = VecDeque::from(["probe", "navigate", "abandon"]);
        assert_eq!(pending.pop_front(), Some("probe"));
        assert_eq!(pending.pop_front(), Some("navigate"));
        assert_eq!(pending.pop_front(), Some("abandon"));
    }
}
