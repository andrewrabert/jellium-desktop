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
    pub on_event: crate::WebEventHandler,
    pub application_menu: crate::ApplicationMenu,
    pub frame_rate: Option<RefreshRate>,
    pub shared_textures: bool,
}

struct Overlay {
    application_menu: Mutex<Option<crate::ApplicationMenu>>,
    session: Arc<crate::runtime::Session>,
    platform: Mutex<Option<jfn_platform_abi::PlatformLease>>,
    subscription: Mutex<Option<jfn_input::ShellStateSubscription>>,
    close: Mutex<()>,
    window_subscription: Mutex<Option<jfn_platform_abi::WindowSubscription>>,
    client: OnceLock<Weak<Inner>>,
    deferred_navigation: Arc<DeferredNavigation>,
    paint_mode: PaintMode,
    frame_rate: AtomicCell<Option<FrameRate>>,
}

#[derive(Clone)]
pub struct WebOverlay {
    inner: Arc<Overlay>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CloseDeliveryError {
    #[error("browser close cannot block the CEF UI thread")]
    WrongThread,
    #[error("CEF shutdown timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("CEF rejected the TID_UI browser-close task")]
    PostRejected,
    #[error("the accepted TID_UI browser-close task was canceled")]
    TaskCanceled,
}

#[derive(Debug, thiserror::Error)]
#[error("the session already owns a web overlay")]
pub struct OverlayStartError;

static STARTED: Mutex<Weak<Overlay>> = Mutex::new(Weak::new());

impl WebOverlay {
    pub fn start(
        runtime: &crate::InitializedCef,
        config: WebOverlayConfig,
    ) -> Result<WebOverlay, OverlayStartError> {
        if !runtime.session.register_overlay() {
            return Err(OverlayStartError);
        }
        let overlay = WebOverlay {
            inner: Arc::new(Overlay {
                application_menu: Mutex::new(Some(config.application_menu)),
                session: Arc::clone(&runtime.session),
                platform: Mutex::new(Some(runtime.platform_lease())),
                subscription: Mutex::new(None),
                close: Mutex::new(()),
                window_subscription: Mutex::new(None),
                client: OnceLock::new(),
                deferred_navigation: DeferredNavigation::new(),
                paint_mode: PaintMode::new(config.shared_textures),
                frame_rate: AtomicCell::new(config.frame_rate.map(FrameRate::from)),
            }),
        };

        let _ = overlay
            .inner
            .deferred_navigation
            .on_event
            .set(config.on_event);
        crate::web_input::install();

        *STARTED.lock() = Arc::downgrade(&overlay.inner);
        *overlay.inner.window_subscription.lock() =
            Some(jfn_platform_abi::subscribe_window_changed(sync_started));
        let subscription = jfn_input::on_shell_state_scoped(Box::new({
            let weak = Arc::downgrade(&overlay.inner);
            move |_| {
                if let Some(inner) = weak.upgrade() {
                    WebOverlay { inner }.sync();
                }
            }
        }));
        *overlay.inner.subscription.lock() = Some(subscription);

        overlay.sync();
        Ok(overlay)
    }

    pub(crate) fn client(&self) -> Option<Arc<Inner>> {
        self.inner.client.get().and_then(Weak::upgrade)
    }

    fn sync(&self) {
        self.inner.session.dispatch(|| {
            let mut task = SyncTask::new(self.clone());
            let _ = post_task(ThreadId::UI, Some(&mut task));
        });
    }

    fn sync_on_ui(&self) {
        if !self.inner.session.is_active() {
            return;
        }
        let Some(state) = jfn_input::shell_state() else {
            return;
        };
        let Some(platform) = self.inner.platform.lock().clone() else {
            return;
        };
        let snapshot = platform.platform().window_owner().source().snapshot();
        let Some(size) = view_size(&snapshot, state.reserved_strip) else {
            return;
        };
        self.ensure_browser(size);
    }

    fn ensure_browser(&self, size: SurfaceSize) {
        if self.inner.client.get().is_some() {
            if let Some(client) = self.client() {
                client.apply_view_size(size);
            }
            return;
        }

        let Some(platform) = self.inner.platform.lock().clone() else {
            return;
        };
        let Some(application_menu) = self.inner.application_menu.lock().clone() else {
            return;
        };
        let surface = WebOverlaySurface::allocate(platform);
        let client = Inner::new(
            Arc::clone(&self.inner.session),
            surface,
            Arc::clone(&self.inner.deferred_navigation),
            self.inner.paint_mode,
            self.inner.frame_rate.load(),
        );
        client.set_name("web");
        client.apply_view_size(size);
        crate::business_web::install(&client, application_menu);
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

    pub fn set_refresh_rate(&self, rate: RefreshRate) {
        self.inner.session.dispatch(|| {
            let mut task = SetRefreshTask::new(Arc::downgrade(&self.inner), FrameRate::from(rate));
            let _ = post_task(ThreadId::UI, Some(&mut task));
        });
    }

    pub fn set_hidden(&self, hidden: bool) {
        self.inner.session.dispatch(|| {
            if let Some(client) = self.client() {
                post_set_hidden(client, hidden);
            }
        });
    }

    pub fn send_external_begin_frame(&self) {
        self.inner.session.dispatch(|| {
            if let Some(client) = self.client() {
                client.send_external_begin_frame();
            }
        });
    }

    pub fn probe(&self, cycle: u64, url: &str) -> Result<(), crate::ready::ReadinessError> {
        self.post_operation(Operation::Probe {
            cycle,
            url: url.to_owned(),
        })
    }

    pub fn cancel_probe(&self) -> Result<(), crate::ready::ReadinessError> {
        self.post_operation(Operation::CancelProbe)
    }

    pub fn navigate(
        &self,
        navigation: crate::Navigation,
        url: &str,
    ) -> Result<(), crate::ready::ReadinessError> {
        self.post_operation(Operation::Navigate {
            navigation,
            url: url.to_owned(),
        })
    }

    pub fn abandon(
        &self,
        navigation: crate::Navigation,
    ) -> Result<(), crate::ready::ReadinessError> {
        self.post_operation(Operation::Abandon { navigation })
    }

    fn post_operation(&self, operation: Operation) -> Result<(), crate::ready::ReadinessError> {
        let overlay = self.clone();
        crate::ready::on_cef_ready(
            &self.inner.session,
            Box::new(move || match operation {
                Operation::Probe { cycle, url } => probe(&overlay, cycle, &url),
                Operation::CancelProbe => cancel_probe_on_ui(),
                Operation::Navigate { navigation, url } => overlay.navigate_on_ui(navigation, &url),
                Operation::Abandon { navigation } => overlay.abandon_on_ui(navigation),
            }),
        )
    }

    fn navigate_on_ui(&self, navigation: crate::Navigation, url: &str) {
        if let Some(client) = self.client() {
            client.navigate(navigation, url);
        } else {
            self.inner.deferred_navigation.navigate(navigation, url);
        }
    }

    fn abandon_on_ui(&self, navigation: crate::Navigation) {
        if let Some(client) = self.client() {
            client.abandon_navigation(navigation);
        } else {
            self.inner.deferred_navigation.abandon(navigation);
        }
    }

    pub fn exec_js(&self, js: &str) {
        self.inner.session.dispatch(|| {
            if let Some(client) = self.client() {
                client.exec_js(js);
            }
        });
    }

    pub fn close_blocking(&self) -> Result<(), CloseDeliveryError> {
        if self.inner.session.is_drained() {
            return Ok(());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let Some(_closing) = self.inner.close.try_lock() else {
            return Err(CloseDeliveryError::Timeout("concurrent close"));
        };
        if self.inner.session.is_drained() {
            return Ok(());
        }
        if cef::currently_on(ThreadId::UI) != 0 {
            return Err(CloseDeliveryError::WrongThread);
        }
        self.inner.session.revoke_until(deadline)?;
        drain_and_release(&self.inner.session, &self.inner.platform, || {
            crate::ready::stop();
            self.inner.window_subscription.lock().take();
            self.inner.subscription.lock().take();
            let (tx, rx) = std::sync::mpsc::channel();
            let mut task = DrainBarrierTask::new(self.clone(), Arc::new(Mutex::new(Some(tx))));
            if post_task(ThreadId::UI, Some(&mut task)) != 1 {
                return Err(CloseDeliveryError::PostRejected);
            }
            drop(task);
            let client = rx
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Timeout => {
                        CloseDeliveryError::Timeout("drain barrier")
                    }
                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                        CloseDeliveryError::TaskCanceled
                    }
                })?;
            if let Some(client) = client {
                post_close_and_wait(client, deadline)?;
            }
            self.inner.session.wait_for_producers(deadline)?;
            let application_menu = self.inner.application_menu.lock().take();
            drop(application_menu);
            Ok(())
        })?;
        *STARTED.lock() = Weak::new();
        Ok(())
    }
}

fn drain_and_release<T, E>(
    session: &crate::runtime::Session,
    dependency: &Mutex<Option<T>>,
    close: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    session.drain(|| {
        close()?;
        let released = dependency.lock().take();
        drop(released);
        Ok(())
    })
}

pub(crate) struct WebOverlaySurface {
    platform: jfn_platform_abi::PlatformLease,
    handle: jfn_platform_abi::SurfaceHandle,
}

impl WebOverlaySurface {
    pub(crate) fn platform(&self) -> &dyn jfn_platform_abi::Platform {
        self.platform.platform()
    }
    pub(crate) fn allocate(platform: jfn_platform_abi::PlatformLease) -> Arc<WebOverlaySurface> {
        let surface = Arc::new(Self {
            handle: platform.platform().alloc_surface(Visibility::Hidden),
            platform,
        });
        let stacker: Arc<dyn jfn_platform_abi::stack::WebOverlayStacker> = surface.clone();
        jfn_platform_abi::stack::install_web_overlay_stacker(Arc::downgrade(&stacker));
        surface
    }

    pub(crate) fn set_visibility(&self, visibility: Visibility) -> Visibility {
        self.platform
            .platform()
            .set_surface_visibility(self.handle, visibility)
            .acknowledged()
    }

    pub(crate) fn resize(&self, size: SurfaceSize) {
        self.platform.platform().surface_resize(self.handle, size);
    }

    pub(crate) fn present<'a>(&self, frame: PaintFrame<'a>) -> Result<Presented, PaintFrame<'a>> {
        self.platform.platform().surface_present(self.handle, frame)
    }

    pub(crate) fn popup_show(&self, x: c_int, y: c_int, width: c_int, height: c_int) {
        self.platform
            .platform()
            .osr_popup_surface()
            .show(self.handle, x, y, width, height);
    }

    pub(crate) fn popup_hide(&self) {
        self.platform
            .platform()
            .osr_popup_surface()
            .hide(self.handle);
    }

    pub(crate) fn popup_present<'a>(
        &self,
        frame: PaintFrame<'a>,
        width: c_int,
        height: c_int,
    ) -> Result<Presented, PaintFrame<'a>> {
        self.platform
            .platform()
            .osr_popup_surface()
            .present(self.handle, frame, width, height)
    }

    #[expect(dead_code, reason = "reserved for external accelerated-paint targets")]
    pub(crate) fn window_target(&self) -> Option<WindowTarget> {
        self.platform.platform().surface_window_target(self.handle)
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
        self.platform.platform().apply_stack(&ordered);
    }
}

impl Drop for WebOverlaySurface {
    fn drop(&mut self) {
        jfn_platform_abi::stack::remove_web_overlay_stacker();
        if !self.handle.is_none() {
            self.platform.platform().free_surface(self.handle);
        }
    }
}

pub(crate) fn with_current_client<R>(f: impl FnOnce(&Inner) -> R) -> Option<R> {
    let overlay = STARTED.lock().upgrade()?;
    overlay
        .session
        .dispatch(|| {
            let client = overlay.client.get().and_then(Weak::upgrade)?;
            Some(f(&client))
        })
        .flatten()
}

fn sync_started() {
    if let Some(inner) = STARTED.lock().upgrade() {
        WebOverlay { inner }.sync();
    }
}

enum Operation {
    Probe {
        cycle: u64,
        url: String,
    },
    CancelProbe,
    Navigate {
        navigation: crate::Navigation,
        url: String,
    },
    Abandon {
        navigation: crate::Navigation,
    },
}

fn cancel_probe_on_ui() {
    let previous = PROBE.lock().take();
    if let Some(previous) = previous {
        previous.cancel_on_ui();
    }
}

fn probe(overlay: &WebOverlay, cycle: u64, url: &str) {
    cancel_probe_on_ui();
    let Some(handler) = overlay.inner.deferred_navigation.on_event.get().cloned() else {
        return;
    };
    let probe = crate::server_probe::Probe::start(
        Arc::clone(&overlay.inner.session),
        url,
        Box::new(move |base| handler(crate::WebEvent::ProbeFinished { cycle, base })),
    );
    *PROBE.lock() = Some(probe);
}

static PROBE: Mutex<Option<crate::server_probe::Probe>> = Mutex::new(None);

type DrainSender = Arc<Mutex<Option<std::sync::mpsc::Sender<Option<Arc<Inner>>>>>>;

wrap_task! {
    struct DrainBarrierTask {
        overlay: WebOverlay,
        sender: DrainSender,
    }
    impl Task {
        fn execute(&self) {
            if let Some(probe) = PROBE.lock().take() { probe.cancel_on_ui(); }
            if let Some(sender) = self.sender.lock().take() {
                let _ = sender.send(self.overlay.client());
            }
        }
    }
}

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
            if !overlay.session.is_active() { return; }
            overlay.frame_rate.store(Some(self.frame_rate));
            if let Some(client) = overlay.client.get().and_then(Weak::upgrade) {
                client.set_refresh_rate(self.frame_rate);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_closed_handle_releases_its_platform_dependency() {
        let session = crate::runtime::Session::new();
        assert!(session.register_overlay());
        let platform = Arc::new(());
        let observer = Arc::downgrade(&platform);
        let retained_handle = Arc::new(Mutex::new(Some(platform)));
        let closer = Arc::clone(&retained_handle);
        assert!(
            drain_and_release(&session, &closer, || {
                assert!(observer.upgrade().is_some());
                assert!(!session.is_drained());
                Ok::<(), ()>(())
            })
            .is_ok()
        );
        assert!(session.is_drained());
        assert!(retained_handle.lock().is_none());
        assert!(observer.upgrade().is_none());
    }

    #[test]
    fn failed_drain_keeps_the_platform_dependency_for_retry() {
        let session = crate::runtime::Session::new();
        assert!(session.register_overlay());
        let platform = Arc::new(());
        let observer = Arc::downgrade(&platform);
        let retained_handle = Mutex::new(Some(platform));
        assert_eq!(
            drain_and_release(&session, &retained_handle, || Err("close rejected")),
            Err("close rejected")
        );
        assert!(!session.is_drained());
        assert!(observer.upgrade().is_some());
        assert!(drain_and_release(&session, &retained_handle, || Ok::<(), ()>(())).is_ok());
        assert!(observer.upgrade().is_none());
    }
}
