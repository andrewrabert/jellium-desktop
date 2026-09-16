#![deny(clippy::let_underscore_must_use)]

pub mod about;
pub mod actor;
pub mod chrome;
pub mod connect;
mod controls;
pub mod field;
pub mod fields;
mod fonts;
pub mod key;
pub mod lang;
pub mod logo;
pub mod menu;
pub mod metadata;
pub mod modal;
pub mod paint;
pub mod router_sink;
pub mod settings;
pub mod settings_overlay;
pub mod spinner;
pub mod state;
pub mod theme;

use std::sync::OnceLock;

use parking_lot::{Condvar, Mutex};

use actor::{Actor, Channel, Work};
use jfn_platform_abi::{Plane, SurfaceHandle, SurfaceSize, Visibility};

#[derive(Clone, Copy)]
pub struct ApplicationActions {
    pub open_menu: fn(jfn_platform_abi::LogicalPoint, bool),
}

#[must_use = "the shell owns a render thread and native surface"]
pub struct Shell {
    actor: Option<Actor>,
    surface: SurfaceHandle,
    platform: Option<jfn_platform_abi::PlatformLease>,
    subscriptions: Option<Subscriptions>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
struct Subscriptions {
    _window: jfn_platform_abi::WindowSubscription,
    _refresh: jfn_gpu_paint::refresh::Subscription,
    _decorations: jfn_platform_abi::DecorationsSubscription,
}
pub use actor::{JoinOutcome as ShutdownOutcome, LoopStartError};

#[must_use = "the application must choose how to handle renderer readiness"]
pub struct Readiness(std::sync::mpsc::Receiver<Result<(), LoopStartError>>);
impl Readiness {
    pub fn wait(
        self,
        platform: &jfn_platform_abi::PlatformRuntime,
        timeout: std::time::Duration,
    ) -> Result<(), ReadinessError> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        platform
            .platform()
            .run_blocking(Box::new(move || {
                let result = self
                    .0
                    .recv_timeout(timeout)
                    .map_err(ReadinessError::Receive)
                    .and_then(|result| result.map_err(ReadinessError::Renderer));
                drop(sender.send(result));
            }))
            .map_err(ReadinessError::Worker)?;
        receiver
            .recv_timeout(std::time::Duration::ZERO)
            .map_err(ReadinessError::Receive)?
    }
}
#[derive(Debug, thiserror::Error)]
pub enum ReadinessError {
    #[error(transparent)]
    Worker(jfn_platform_abi::BlockingError),
    #[error("shell renderer: {0}")]
    Renderer(LoopStartError),
    #[error("shell readiness: {0}")]
    Receive(std::sync::mpsc::RecvTimeoutError),
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("shell has no usable GPU adapter")]
    NoAdapter,
    #[error("shell surface allocation failed")]
    SurfaceAllocation,
    #[error("shell receiver has already been claimed")]
    ReceiverUnavailable,
    #[error("shell startup has already been claimed")]
    StartupAlreadyClaimed,
    #[error("shell thread could not start: {0}")]
    Thread(#[from] std::io::Error),
}
static CHANNEL: OnceLock<Channel> = OnceLock::new();
static SURFACE: Mutex<Option<SurfaceHandle>> = Mutex::new(None);
static FONTS_READY: (Mutex<Option<Result<(), FontWarmupError>>>, Condvar) =
    (Mutex::new(None), Condvar::new());

pub(crate) fn publish_no_overlay() {
    let Some(lease) = jfn_platform_abi::try_lease() else {
        return;
    };
    let plat = lease.platform();
    jfn_input::publish_shell_state(crate::shell::state::shell_state(
        plat.window_owner().source().snapshot().extent,
        crate::shell::state::ChromeInputs::default(),
        false,
    ));
}

pub fn shell_start(
    platform: &jfn_platform_abi::PlatformRuntime,
    metadata: metadata::ApplicationMetadata,
    actions: ApplicationActions,
) -> Result<(Shell, Readiness), StartError> {
    static CLAIMED: OnceLock<()> = OnceLock::new();
    CLAIMED
        .set(())
        .map_err(|()| StartError::StartupAlreadyClaimed)?;
    if jfn_gpu_paint::Surfaces::init(None).is_none() {
        publish_no_overlay();
        return Err(StartError::NoAdapter);
    }

    let plat = platform.platform();
    let surface = plat.alloc_surface(Visibility::Hidden);
    if surface == SurfaceHandle::NONE {
        publish_no_overlay();
        return Err(StartError::SurfaceAllocation);
    }
    let mut shell = Shell {
        actor: None,
        surface,
        platform: Some(platform.lease()),
        subscriptions: None,
        _thread: std::marker::PhantomData,
    };
    jfn_platform_abi::stack::occupy(Plane::ShellOverlay, surface);
    *SURFACE.lock() = Some(surface);

    let _claimed = plat.surface_window_target(surface);

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    shell.actor = Some(Actor::spawn(
        surface,
        CHANNEL.get_or_init(Channel::new),
        metadata,
        actions,
        platform.lease(),
        ready_tx,
    )?);

    let refresh = jfn_gpu_paint::refresh::subscribe(refresh_changed);
    jfn_input::install_shell(Box::new(router_sink::ShellSink));
    chrome::set_listener(Box::new(|inputs| post(Work::Chrome(inputs))));
    jfn_playback::chrome::subscribe_chrome(push_playback_chrome);
    jfn_color::theme::jfn_theme_color_subscribe(|rgb| {
        post(Work::ChromeBackground(theme::from_rgb(rgb)));
    });
    let decorations = jfn_platform_abi::set_decorations_listener(push_decorations);
    let window = jfn_platform_abi::subscribe_window_changed(push_window_state);
    push_decorations();
    push_window_state();
    push_playback_chrome();

    shell.subscriptions = Some(Subscriptions {
        _window: window,
        _refresh: refresh,
        _decorations: decorations,
    });
    Ok((shell, Readiness(ready_rx)))
}

impl Shell {
    pub fn shutdown(mut self) -> ShutdownOutcome {
        self.stop()
    }

    fn stop(&mut self) -> ShutdownOutcome {
        let Some(platform) = self.platform.take() else {
            return ShutdownOutcome::Terminated;
        };
        CLOSED.store(true, std::sync::atomic::Ordering::Release);
        self.subscriptions.take();
        *SURFACE.lock() = None;
        let result = if let Some(actor) = self.actor.take() {
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            if let Err(error) = platform.platform().run_blocking(Box::new(move || {
                let _delivery = sender.send(actor.join());
            })) {
                tracing::error!("shell shutdown: {error}");
                error.abandon();
            }
            receiver
                .try_recv()
                .unwrap_or(ShutdownOutcome::JoinerUnavailable)
        } else {
            ShutdownOutcome::Terminated
        };
        {
            if result.terminated() {
                jfn_platform_abi::stack::vacate(Plane::ShellOverlay);
                platform.platform().free_surface(self.surface);
            } else {
                let _ = Box::leak(Box::new(platform));
            }
        }
        result
    }
}
impl Drop for Shell {
    fn drop(&mut self) {
        let result = self.stop();
        if !result.terminated() {
            tracing::error!(?result, "shell termination unconfirmed; retaining platform");
        }
    }
}

static CLOSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn shell_surface() -> SurfaceHandle {
    SURFACE.lock().unwrap_or(SurfaceHandle::NONE)
}

#[must_use = "font warmup must finish before CEF initialization"]
pub struct FontWarmup {
    worker: Option<std::thread::JoinHandle<()>>,
    spawn_failed: bool,
}
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum FontWarmupError {
    #[error("font warmup worker could not start")]
    Spawn,
    #[error("font warmup worker panicked")]
    Panicked,
}
impl FontWarmup {
    pub fn join(mut self) -> Result<(), FontWarmupError> {
        self.finish()
    }
    fn finish(&mut self) -> Result<(), FontWarmupError> {
        if self.spawn_failed {
            return Err(FontWarmupError::Spawn);
        }
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| FontWarmupError::Panicked)?;
        }
        Ok(())
    }
}
impl Drop for FontWarmup {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            tracing::error!("{error}");
        }
    }
}

fn warm_fonts_with(warm: impl FnOnce(), complete: impl FnOnce(Result<(), FontWarmupError>)) {
    struct Completion<F: FnOnce(Result<(), FontWarmupError>)> {
        callback: Option<F>,
        success: bool,
    }
    impl<F: FnOnce(Result<(), FontWarmupError>)> Drop for Completion<F> {
        fn drop(&mut self) {
            if let Some(callback) = self.callback.take() {
                callback(if self.success {
                    Ok(())
                } else {
                    Err(FontWarmupError::Panicked)
                });
            }
        }
    }
    let mut completion = Completion {
        callback: Some(complete),
        success: false,
    };
    warm();
    completion.success = true;
}

pub fn shell_warm_fonts() -> FontWarmup {
    match std::thread::Builder::new()
        .name("jfn-shell-fonts".to_owned())
        .spawn(|| warm_fonts_with(|| fonts::warm(FONT), signal_fonts_ready))
    {
        Ok(worker) => FontWarmup {
            worker: Some(worker),
            spawn_failed: false,
        },
        Err(error) => {
            tracing::error!("font warmup spawn: {error}");
            signal_fonts_ready(Err(FontWarmupError::Spawn));
            FontWarmup {
                worker: None,
                spawn_failed: true,
            }
        }
    }
}

pub fn wait_fonts_ready() -> Result<(), FontWarmupError> {
    let (lock, ready) = &FONTS_READY;
    let mut result = lock.lock();
    while result.is_none() {
        ready.wait(&mut result);
    }
    result.unwrap_or(Err(FontWarmupError::Panicked))
}
fn signal_fonts_ready(result: Result<(), FontWarmupError>) {
    let (lock, ready) = &FONTS_READY;
    *lock.lock() = Some(result);
    ready.notify_all();
}

pub fn shell_open_about() {
    post(Work::OpenAbout);
}

pub fn shell_open_client_settings() {
    post(Work::OpenClientSettings);
}

const FONT: &[u8] = include_bytes!("assets/NotoSans-Regular.ttf");

pub(crate) fn post(work: Work) {
    if !CLOSED.load(std::sync::atomic::Ordering::Acquire) {
        CHANNEL.get_or_init(Channel::new).post(work);
    }
}

fn refresh_changed() {
    post(Work::Redraw);
}

fn push_playback_chrome() {
    let state = jfn_playback::chrome::chrome_state();
    chrome::set_video_active(state.video_active);
    chrome::set_osd_visible(state.osd_visible);
}

fn push_decorations() {
    let _surface_guard = SURFACE.lock();
    if CLOSED.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let Some(lease) = jfn_platform_abi::try_lease() else {
        return;
    };
    let client_side = matches!(
        lease.platform().effective_decorations(),
        jfn_platform_abi::EffectiveDecorations::ClientSide
    );
    chrome::set_client_side_decorations(client_side);
}

fn push_window_state() {
    let surface_guard = SURFACE.lock();
    if CLOSED.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let Some(lease) = jfn_platform_abi::try_lease() else {
        return;
    };
    let plat = lease.platform();
    let snap = plat.window_owner().source().snapshot();
    chrome::set_fullscreen(snap.fullscreen);
    let Some(extent) = snap.extent else { return };
    post(Work::Resize { extent });
    let surface = surface_guard.unwrap_or(SurfaceHandle::NONE);
    if surface != SurfaceHandle::NONE {
        plat.surface_resize(
            surface,
            SurfaceSize {
                extent,
                logical_top: 0,
                physical_top: 0,
            },
        );
    }
}

#[cfg(test)]
mod font_tests {
    use super::*;
    #[test]
    fn warmup_unwind_releases_waiter_with_failure() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let result = std::panic::catch_unwind(|| {
            warm_fonts_with(
                || std::panic::resume_unwind(Box::new("injected font failure")),
                |result| {
                    assert!(sender.send(result).is_ok());
                },
            )
        });
        assert!(result.is_err());
        assert!(matches!(
            receiver.try_recv(),
            Ok(Err(FontWarmupError::Panicked))
        ));
    }
    #[test]
    fn warmup_success_publishes_readiness_once() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        warm_fonts_with(
            || {},
            |result| {
                assert!(sender.send(result).is_ok());
            },
        );
        assert!(matches!(receiver.try_recv(), Ok(Ok(()))));
        assert!(receiver.try_recv().is_err());
    }
}
