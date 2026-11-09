//! Process entry point. [`jfn_app_main`] owns the full main loop and
//! returns the exit code.

use std::ffi::{CStr, CString, c_char, c_int};
use std::path::{Path, PathBuf};
use std::ptr;
use std::thread::JoinHandle;

use crate::shell::metadata::ApplicationMetadata;
use clap::Parser;
use jfn_cef::version::CefVersion;
use jfn_cef::{APP_VERSION_FULL, BrowserCef, InitializedCef, LoadedCef, ProcessDispatch};
use jfn_instance_ipc::jfn::{Request, Response};
use jfn_instance_ipc::{Listener, Start, Stream};
use jfn_logging::{Category, Level};
use jfn_platform_abi::{IdleInhibitLevel, Instance, Platform, WindowGeometry};

use crate::cli;

// Shorthand for the installed Platform backend. `install()` happens before
// any of the call sites here run.
fn plat() -> &'static dyn Platform {
    // SAFETY: process wiring owns the prepared/runtime native phase until all producers stop.
    unsafe { jfn_platform_abi::get() }
}

/// The started web overlay, for the C handler thunks the playback coordinator
/// still calls through.
static WEB_OVERLAY: parking_lot::Mutex<Option<jfn_cef::WebOverlay>> = parking_lot::Mutex::new(None);

#[derive(Debug, thiserror::Error)]
enum RuntimeShutdownError {
    #[error(transparent)]
    Manager(#[from] crate::manager::ManagerError),
    #[error("the shutdown manager thread could not be joined")]
    ManagerJoinFailed,
}

/// mpv background applied over the user's mpv.conf color for the app's
/// lifetime before the theme rotator takes over.
const STARTUP_BG_HEX: &str = "#101010";

enum BackgroundCapture {
    Pending,
    Applied { user_video_bg: u32 },
}

struct ReadyBoot {
    user_video_bg: u32,
}

#[derive(Debug, thiserror::Error)]
enum BootError {
    #[error("startup shutdown-wake thread: {0}")]
    Wake(#[from] std::io::Error),
    #[error("startup canceled by a shutdown request")]
    Canceled,
    #[error("mpv terminated before its window became ready")]
    MpvTerminated,
}

impl BootError {
    fn exit_code(&self) -> c_int {
        if matches!(self, Self::Canceled) { 0 } else { 1 }
    }
}

/// What `CefInitialize` was given.
struct CefInit {
    runtime: InitializedCef,
    /// Whether CEF composites through shared textures.
    shared_textures: bool,
}

pub(crate) const DEFAULT_LOG_FILTER: &str = "info";

fn cs(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

/// Normalize the audio-passthrough list: if `dts-hd` is present, drop
/// bare `dts` (the HD variant subsumes it).
fn normalize_passthrough(s: &str) -> String {
    if !s.contains("dts-hd") {
        return s.to_string();
    }
    s.split(',')
        .filter(|c| *c != "dts")
        .collect::<Vec<_>>()
        .join(",")
}

fn print_version(version: &CefVersion) {
    println!("jellium-desktop {}\n\nCEF {}\n", APP_VERSION_FULL, version);
    use std::io::Write;
    let _ = std::io::stdout().flush();
    jfn_mpv::probe::jfn_mpv_print_version_info();
}

fn init_logging(log_file: Option<&Path>, log_level: &str, version: &CefVersion) {
    let log_path = log_file
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let filter = if log_level.is_empty() {
        DEFAULT_LOG_FILTER.to_string()
    } else {
        log_level.to_string()
    };
    jfn_logging::jfn_log_init(&log_path, &filter);

    tracing::info!(target: "Main", "jellium-desktop {APP_VERSION_FULL}");
    tracing::info!(target: "Main", "CEF {}", version);
    if !log_path.is_empty() {
        tracing::info!(target: "Main", "Log file: {log_path}");
    }
}

fn log_mpv_versions() {
    for prop in ["mpv-version", "ffmpeg-version"] {
        let pc = cs(prop);
        let v = unsafe { jfn_mpv::api::jfn_mpv_get_property_string(pc.as_ptr()) };
        let s = if v.is_null() {
            String::new()
        } else {
            let s = unsafe { CStr::from_ptr(v) }.to_string_lossy().into_owned();
            unsafe { jfn_mpv::api::jfn_mpv_free_string(v) };
            s
        };
        tracing::info!(target: "Main", "{prop} {s}");
    }
}

/// Restores the builtin CLOSE_WIN -> quit binding that
/// `input-default-bindings=no` drops. Async: the boot path never parks on
/// mpv's core.
fn install_mpv_close_binding() {
    let kb = cs("keybind");
    let name = cs("CLOSE_WIN");
    let action = cs("quit");
    let argv = [kb.as_ptr(), name.as_ptr(), action.as_ptr()];
    unsafe { jfn_mpv::api::jfn_mpv_command_async(argv.as_ptr(), argv.len()) };
}

/// Wake any thread parked in `mpv_wait_event` whenever a host publishes a
/// window change, so the VO wait re-reads the readiness inputs mpv never
/// reports: `MpvHost::host_ready` and the host-owned extent on backends that
/// own their toplevel.
fn wake_mpv_on_window_change() -> jfn_platform_abi::WindowSubscription {
    jfn_platform_abi::subscribe_window_changed(jfn_mpv::api::jfn_mpv_wakeup)
}

fn setup_mpv_environment() {
    let mpv_home = jfn_paths::mpv_home();
    unsafe {
        std::env::set_var("MPV_HOME", &mpv_home);
    }

    plat()
        .mpv_host()
        .prepare(jfn_config::configured_window_decorations());
}

struct StartupOptions {
    hwdec: jfn_config::Hwdec,
    audio_passthrough: String,
    audio_exclusive: bool,
    audio_channels: String,
    log_level: String,
    /// The file logs are written to; `None` disables file logging.
    log_file: Option<PathBuf>,
    disable_gpu_compositing: bool,
    remote_debugging_port: jfn_cef::DebuggingPort,
}

fn resolve_startup_options(cli: &cli::Cli) -> StartupOptions {
    let saved_hwdec = jfn_config::hwdec();
    let saved_pass = jfn_config::audio_passthrough();
    let saved_chans = jfn_config::audio_channels();
    let saved_log_level = jfn_config::log_level();
    let saved_audio_exclusive = jfn_config::audio_exclusive();

    // An unknown CLI value falls back to the mpv default.
    let hwdec = match cli.hwdec.as_deref() {
        Some(value) => value.parse::<jfn_config::Hwdec>().unwrap_or_default(),
        None => saved_hwdec,
    };
    let mut audio_passthrough = saved_pass;
    let mut audio_exclusive = saved_audio_exclusive;
    let mut audio_channels = saved_chans;
    let mut log_level = saved_log_level;

    let log_file = match cli.log_file.as_deref() {
        Some("") => None,
        Some(path) => Some(PathBuf::from(path)),
        None => jfn_paths::default_log_file(),
    };
    let mut disable_gpu_compositing = false;
    let remote_debugging_port = cli.remote_debug_port.unwrap_or_default();

    if let Some(v) = cli.audio_passthrough.clone() {
        audio_passthrough = v;
    }
    if let Some(v) = cli.audio_channels.clone() {
        audio_channels = v;
    }
    if let Some(v) = cli.log_level.clone() {
        log_level = v;
    }
    if cli.audio_exclusive {
        audio_exclusive = true;
    }
    if cli.disable_gpu_compositing {
        disable_gpu_compositing = true;
    }

    if !audio_passthrough.is_empty() {
        audio_passthrough = normalize_passthrough(&audio_passthrough);
    }

    StartupOptions {
        hwdec,
        audio_passthrough,
        audio_exclusive,
        audio_channels,
        log_level,
        log_file,
        disable_gpu_compositing,
        remote_debugging_port,
    }
}

struct MpvInitOptions<'a> {
    backend_byte: u8,
    boot_geometry: Option<&'a str>,
    boot_force_position: bool,
    boot_window_max: bool,
    embed_wid: Option<i64>,
    hwdec: jfn_config::Hwdec,
    audio_passthrough: &'a str,
    audio_exclusive: bool,
    audio_channels: &'a str,
    mpv_log_level: &'a str,
}

fn init_mpv_handle(opts: MpvInitOptions<'_>) -> *mut jfn_mpv::sys::mpv_handle {
    let geometry_c = opts.boot_geometry.map(cs);
    let hwdec_c = cs(opts.hwdec.as_str());
    let user_agent_c = cs(&format!("JelliumDesktop/{}", APP_VERSION_FULL));
    let passthrough_c = cs(opts.audio_passthrough);
    let channels_c = cs(opts.audio_channels);
    let mpv_log_level_c = cs(opts.mpv_log_level);
    let boot = jfn_mpv::boot::JfnMpvBoot {
        display_backend: opts.backend_byte,
        hwdec: hwdec_c.as_ptr(),
        user_agent: user_agent_c.as_ptr(),
        audio_passthrough: if opts.audio_passthrough.is_empty() {
            ptr::null()
        } else {
            passthrough_c.as_ptr()
        },
        audio_exclusive: opts.audio_exclusive,
        audio_channels: if opts.audio_channels.is_empty() {
            ptr::null()
        } else {
            channels_c.as_ptr()
        },
        geometry: geometry_c.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
        wid: opts.embed_wid.unwrap_or(0),
        force_window_position: opts.boot_force_position,
        window_maximized_at_boot: opts.boot_window_max,
        mpv_log_level: mpv_log_level_c.as_ptr(),
        client_side_decorations: jfn_config::client_side_decorations(),
    };
    unsafe { jfn_mpv::boot::jfn_mpv_handle_init(&boot as *const _) }
}

/// Returns the completed boot state or the reason startup stopped.
fn wait_for_vo_window(mut boot: BackgroundCapture) -> Result<ReadyBoot, BootError> {
    use std::ops::ControlFlow::{Break, Continue};
    let _shutdown_wake = crate::manager::BootShutdownWake::start()?;
    tracing::info!(target: "Main", "Waiting for mpv window...");
    let started = std::time::Instant::now();
    let mut outcome = Err(BootError::MpvTerminated);
    plat().mpv_host().run_vo_wait(&mut |wait| {
        if jfn_playback::shutdown::jfn_shutting_down() {
            outcome = Err(BootError::Canceled);
            return Break(());
        }
        loop {
            match consume_boot_event(&mut boot, jfn_mpv::api::wait_event_owned(0.0)) {
                BootEvent::Idle => break,
                BootEvent::Fatal => return Break(()),
                BootEvent::Consumed => {}
            }
        }
        if let Some(ready) = boot_progress(&boot) {
            outcome = Ok(ready);
            return Break(());
        }
        if wait == jfn_platform_abi::VoWait::Event
            && matches!(
                consume_boot_event(&mut boot, jfn_mpv::api::wait_event_owned(-1.0)),
                BootEvent::Fatal
            )
        {
            return Break(());
        }
        Continue(())
    });
    if outcome.is_ok() {
        tracing::info!(target: "Main", "mpv window ready in {} ms", started.elapsed().as_millis());
    }
    outcome
}

fn publish_device_profile(mpv_raw: *mut jfn_mpv::sys::mpv_handle) {
    let caps = unsafe { jfn_mpv::capabilities::query_raw(mpv_raw) };
    let decoders: Vec<jfn_jellyfin::Codec> = caps
        .decoders
        .into_iter()
        .map(|c| jfn_jellyfin::Codec {
            name: c.name,
            kind: match c.kind {
                jfn_mpv::capabilities::MediaKind::Video => jfn_jellyfin::MediaKind::Video,
                jfn_mpv::capabilities::MediaKind::Audio => jfn_jellyfin::MediaKind::Audio,
                jfn_mpv::capabilities::MediaKind::Subtitle => jfn_jellyfin::MediaKind::Subtitle,
            },
        })
        .collect();
    let force = jfn_config::force_transcoding();
    let profile = jfn_jellyfin::build_device_profile(
        &decoders,
        &caps.demuxers,
        "Jellium Desktop",
        APP_VERSION_FULL,
        force,
    );
    tracing::info!(target: "Main", "Device profile: {profile}");
    unsafe {
        jfn_cef::injection::jfn_cef_set_device_profile_json(
            profile.as_ptr() as *const _,
            profile.len(),
        );
    }
}

/// Consumes the browser bootstrap and returns its initialized session.
fn initialize_cef(
    runtime: BrowserCef,
    platform: &jfn_platform_abi::PlatformRuntime,
    opts: &StartupOptions,
) -> Result<CefInit, jfn_cef::InitError> {
    let shared_textures = plat().shared_texture_supported() && !opts.disable_gpu_compositing;
    tracing::info!(target: "Main", "[FLOW] calling CefInitialize...");
    let started = std::time::Instant::now();
    let runtime = runtime.initialize(
        platform,
        jfn_cef::InitOptions {
            log_severity: cef_severity_for_cef_filter(),
            remote_debugging_port: opts.remote_debugging_port,
            disable_gpu_compositing: !shared_textures,
        },
    )?;
    tracing::info!(target: "Main",
        "[FLOW] CefInitialize returned ok in {} ms", started.elapsed().as_millis());
    Ok(CefInit {
        runtime,
        shared_textures,
    })
}

#[derive(Debug, thiserror::Error)]
#[error("failed to start mpv event thread")]
struct PlaybackStartError;

struct PlaybackCoordination {
    stopped: bool,
}

struct StoppedPlayback;

impl PlaybackCoordination {
    fn stop(mut self) -> StoppedPlayback {
        let stopped = StoppedPlayback;
        self.stopped = true;
        plat().media_session().stop();
        jfn_playback::ingest_driver::jfn_playback_stop_mpv_event_thread();
        stopped
    }
}

impl Drop for PlaybackCoordination {
    fn drop(&mut self) {
        if !self.stopped {
            let _shutdown = StoppedPlayback;
            plat().media_session().stop();
            jfn_playback::ingest_driver::jfn_playback_stop_mpv_event_thread();
        }
    }
}
impl StoppedPlayback {
    fn shutdown(self) {
        drop(self);
    }
}
impl Drop for StoppedPlayback {
    fn drop(&mut self) {
        jfn_playback::ffi::jfn_playback_shutdown();
    }
}

fn initialize_playback_coordination() -> PlaybackCoordination {
    jfn_playback::idle_inhibit_sink::jfn_playback_set_idle_inhibit_handler(Some(h_idle_inhibit));
    jfn_playback::theme_color_sink::jfn_playback_set_theme_video_mode_handler(Some(
        h_theme_video_mode,
    ));
    jfn_playback::exec_js::jfn_playback_set_web_exec_js_handler(Some(h_web_exec_js));
    jfn_playback::browser_sink::jfn_playback_set_browsers_refresh_rate_handler(Some(
        h_browsers_set_refresh_rate,
    ));

    jfn_playback::ingest_driver::jfn_playback_set_fullscreen_handler(|fs| {
        plat().set_fullscreen(fs)
    });
    jfn_playback::ingest_driver::jfn_playback_set_shutdown_handler(|| {
        tracing::info!(target: "Main", "MPV_EVENT_SHUTDOWN received");
        jfn_playback::jfn_shutdown_initiate();
    });

    // The coordinator reconciles immediately, so install every sink first.
    jfn_playback::ffi::jfn_playback_init();
    PlaybackCoordination { stopped: false }
}

#[derive(Debug, thiserror::Error)]
enum CleanupError {
    #[error("mpv termination unconfirmed: {0}")]
    Termination(String),
    #[error("shell shutdown: {0:?}")]
    Shell(crate::shell::ShutdownOutcome),
    #[error(transparent)]
    Cef(#[from] jfn_cef::ShutdownError),
    #[error(transparent)]
    Platform(#[from] jfn_platform_abi::PlatformBusy),
}
#[derive(Debug)]
struct CleanupErrors(Vec<CleanupError>);
impl std::fmt::Display for CleanupErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index != 0 {
                f.write_str("; ")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}
impl std::error::Error for CleanupErrors {}

/// Owns the process mpv handle and its ingestion dependency. A runtime can only
/// be terminated after ingestion has stopped, including startup rollback.
struct MpvRuntime {
    raw: std::ptr::NonNull<jfn_mpv::sys::mpv_handle>,
    window_wake: Option<jfn_platform_abi::WindowSubscription>,
    armed: bool,
}
impl MpvRuntime {
    fn new(raw: *mut jfn_mpv::sys::mpv_handle) -> Option<Self> {
        Some(Self {
            raw: std::ptr::NonNull::new(raw)?,
            window_wake: None,
            armed: true,
        })
    }
    fn raw(&self) -> *mut jfn_mpv::sys::mpv_handle {
        self.raw.as_ptr()
    }
    fn terminate(mut self) -> Result<(), jfn_platform_abi::BlockingError> {
        self.stop()
    }
    fn stop(&mut self) -> Result<(), jfn_platform_abi::BlockingError> {
        if !std::mem::replace(&mut self.armed, false) {
            return Ok(());
        }
        self.window_wake.take();
        jfn_playback::ingest_driver::jfn_playback_stop_mpv_event_thread();
        // On rejection the returned work retains termination authority; it may
        // be retried before the retained PostWindowCleanup is completed.
        plat().run_blocking(Box::new(jfn_mpv::boot::jfn_mpv_handle_terminate))
    }
}
impl Drop for MpvRuntime {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::error!(target: "Main", "mpv termination retained: {error}");
            error.abandon();
        }
    }
}

/// A prepared host may precede mpv acquisition; an initialized platform always
/// owns mpv. Each phase carries exactly the native resources it can own.
struct PreparedNative {
    prepared: jfn_platform_abi::PreparedPlatform,
}
struct MpvNative {
    prepared: jfn_platform_abi::PreparedPlatform,
    mpv: MpvRuntime,
}
struct InitializedNative {
    platform: jfn_platform_abi::PlatformRuntime,
    mpv: MpvRuntime,
}
trait NativePhase {
    fn cleanup(self) -> Result<(), CleanupError>;
}
impl NativePhase for PreparedNative {
    fn cleanup(self) -> Result<(), CleanupError> {
        match self
            .prepared
            .cleanup(|| Ok::<(), std::convert::Infallible>(()))
        {
            Ok(()) => Ok(()),
            Err((never, _)) => match never {},
        }
    }
}
impl NativePhase for MpvNative {
    fn cleanup(self) -> Result<(), CleanupError> {
        match self.prepared.cleanup(|| self.mpv.terminate()) {
            Ok(()) => Ok(()),
            Err((error, post_window)) => {
                let detail = error.to_string();
                error.abandon();
                post_window.abandon();
                Err(CleanupError::Termination(detail))
            }
        }
    }
}
impl NativePhase for InitializedNative {
    fn cleanup(self) -> Result<(), CleanupError> {
        self.platform
            .platform()
            .set_idle_inhibit(IdleInhibitLevel::None);
        match self.platform.cleanup(|| self.mpv.terminate()) {
            Ok(()) => Ok(()),
            Err(jfn_platform_abi::PlatformCleanupError::Busy { runtime, terminate }) => {
                let _ = Box::leak(Box::new((runtime, terminate)));
                Err(jfn_platform_abi::PlatformBusy.into())
            }
            Err(jfn_platform_abi::PlatformCleanupError::Termination { error, post_window }) => {
                let detail = error.to_string();
                error.abandon();
                post_window.abandon();
                Err(CleanupError::Termination(detail))
            }
        }
    }
}
struct NativeOwner<P: NativePhase>(Option<P>);
// The only optional phase is the consumed teardown slot. Callers cannot form
// an initialized startup phase without its concrete platform and mpv owners.
#[allow(clippy::expect_used)]
impl<P: NativePhase> NativeOwner<P> {
    fn get(&self) -> &P {
        self.0
            .as_ref()
            .expect("native phase already consumed by teardown")
    }
    fn get_mut(&mut self) -> &mut P {
        self.0
            .as_mut()
            .expect("native phase already consumed by teardown")
    }
    fn take(&mut self) -> P {
        self.0
            .take()
            .expect("native phase already consumed by teardown")
    }

    fn cleanup(&mut self) -> Result<(), CleanupError> {
        self.0.take().map_or(Ok(()), NativePhase::cleanup)
    }
}
impl<P: NativePhase> Drop for NativeOwner<P> {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            tracing::error!(target: "Main", "native cleanup: {error}");
        }
    }
}

/// Field drop order is intentional: producers and CEF drain before native
/// backend cleanup, mpv termination, and post-window cleanup.
struct StartupResources<P: NativePhase> {
    services: Services,
    native: NativeOwner<P>,
}
impl StartupResources<PreparedNative> {
    fn with_mpv(mut self, mpv: MpvRuntime) -> StartupResources<MpvNative> {
        let PreparedNative { prepared } = self.native.take();
        StartupResources {
            services: self.services,
            native: NativeOwner(Some(MpvNative { prepared, mpv })),
        }
    }
}
impl StartupResources<MpvNative> {
    fn initialize(
        mut self,
    ) -> Result<StartupResources<InitializedNative>, jfn_platform_abi::PlatformInitError> {
        let MpvNative { prepared, mpv } = self.native.take();
        match prepared.initialize(mpv.raw().cast()) {
            Ok(platform) => Ok(StartupResources {
                services: self.services,
                native: NativeOwner(Some(InitializedNative { platform, mpv })),
            }),
            Err((error, prepared)) => {
                self.native.0 = Some(MpvNative { prepared, mpv });
                Err(error)
            }
        }
    }
}
#[derive(Default)]
struct Services {
    cleaned: bool,
    fonts: Option<crate::shell::FontWarmup>,
    cef: Option<CefInit>,
    shell: Option<crate::shell::Shell>,
    playback: Option<PlaybackCoordination>,
}
impl Services {
    fn cleanup(&mut self) -> Result<(), CleanupErrors> {
        if std::mem::replace(&mut self.cleaned, true) {
            return Ok(());
        }
        if let Some(fonts) = self.fonts.take() {
            let _ = fonts.join();
        }
        plat().mpv_host().detach();
        if let Some(host) = plat().cef_host() {
            host.stop_frame_driver();
        }
        jfn_color::theme::jfn_theme_color_shutdown();
        let playback = self.playback.take().map(PlaybackCoordination::stop);
        jfn_config::settings_shutdown_save_worker();
        let mut failures = Vec::new();
        if let Some(shell) = self.shell.take() {
            let result = shell.shutdown();
            if result != crate::shell::ShutdownOutcome::Terminated {
                failures.push(CleanupError::Shell(result));
            }
        }
        WEB_OVERLAY.lock().take();
        if let Some(mut cef) = self.cef.take()
            && let Err(error) = cef.runtime.try_shutdown()
        {
            failures.push(error.into());
            cef.runtime.abandon();
        }
        if let Some(playback) = playback {
            playback.shutdown();
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(CleanupErrors(failures))
        }
    }
}
impl Drop for Services {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            tracing::error!(target: "Main", "startup/runtime cleanup: {error}");
        }
    }
}
impl<P: NativePhase> StartupResources<P> {
    fn cleanup(&mut self) -> Result<(), CleanupErrors> {
        let mut failures = self.services.cleanup().err().map_or_else(Vec::new, |e| e.0);
        if let Err(error) = self.native.cleanup() {
            failures.push(error);
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(CleanupErrors(failures))
        }
    }
}

type ManagerThread = JoinHandle<Result<(), crate::manager::ManagerError>>;

#[derive(Debug, thiserror::Error)]
enum OverlayStartupError {
    #[error(transparent)]
    Registration(#[from] jfn_cef::web_overlay::OverlayStartError),
    #[error("shutdown manager thread spawn: {0}")]
    ManagerSpawn(#[from] std::io::Error),
}

struct StartedOverlay {
    manager_thread: ManagerThread,
    overlay: jfn_cef::WebOverlay,
}

struct RunningRuntime {
    resources: StartupResources<InitializedNative>,
    manager_thread: ManagerThread,
    _overlay: jfn_cef::WebOverlay,
}
impl RunningRuntime {
    /// Enable external producers only after all overlay/manager callbacks exist.
    fn start_ingestion(&self, instance: &Instance) -> Result<(), PlaybackStartError> {
        plat().media_session().start(instance);
        if !jfn_playback::ingest_driver::jfn_playback_start_mpv_event_thread() {
            return Err(PlaybackStartError);
        }
        Ok(())
    }

    fn run(mut self, instance: &Instance) -> c_int {
        let ingestion = if jfn_playback::shutdown::jfn_shutting_down() {
            Ok(())
        } else {
            self.start_ingestion(instance)
        };
        if let Err(error) = &ingestion {
            tracing::error!(target: "Main", "{error}");
            // The manager already exists, so startup failure can drain browsers
            // through the normal main-loop path instead of abandoning them.
            jfn_playback::jfn_shutdown_initiate();
        }
        plat().run_main_loop();
        crate::window_geometry::controller().persist();
        jfn_config::settings_save();
        let manager = self
            .manager_thread
            .join()
            .map_err(|_| RuntimeShutdownError::ManagerJoinFailed)
            .and_then(|r| r.map_err(RuntimeShutdownError::from));
        if let Err(error) = &manager {
            tracing::error!(target: "Main", "shutdown manager: {error}");
        }
        // InitializedCef independently requires confirmed drain; manager failure
        // never becomes permission to shut down still-live browsers.
        let cleanup = self.resources.cleanup();
        if let Err(error) = &cleanup {
            tracing::error!(target: "Main", "shutdown: {error}");
        }
        i32::from(ingestion.is_err() || manager.is_err() || cleanup.is_err())
    }
}

/// Boot-time mpv size reconcile (saved scale vs the scale the platform
/// reports); seeds the display-hz cache and returns it for browser init.
fn boot_mpv_reconcile() -> Option<jfn_gpu_paint::RefreshRate> {
    jfn_playback::ingest_driver::jfn_playback_seed_display_hz_sync();
    let hz = jfn_playback::ingest_driver::jfn_playback_display_hz();
    let rate = jfn_gpu_paint::RefreshRate::from_hz(hz);
    if let Some(rate) = rate {
        jfn_gpu_paint::report_refresh(jfn_gpu_paint::RefreshSource::MpvDisplayFps, rate);
    }
    let saved = jfn_config::window_geometry();
    let snap = crate::window_geometry::controller().source().snapshot();
    tracing::info!(target: "Main",
        "[FLOW] scale={} fullscreen={} display-hz={hz}",
        plat().scale(), snap.fullscreen
    );

    // Saved intent, not an observation: the OS may still be applying the
    // maximize, and a set_geometry landing mid-flight leaves mpv's stored
    // window size disagreeing with the visible window.
    let locked = saved.maximized || snap.fullscreen || snap.maximized;
    let reconciled = crate::window_geometry::saved_sizes(&saved).and_then(|(logical, physical)| {
        plat()
            .window_owner()
            .reconcile_mpv_size(plat().scale(), logical, physical, locked)
    });
    if let Some(physical) = reconciled {
        let clamped = plat().clamp_window_geometry(WindowGeometry {
            w: physical.w,
            h: physical.h,
            position: None,
        });
        let (new_pw, new_ph) = (clamped.w, clamped.h);
        let geom_str = format!("{new_pw}x{new_ph}");
        tracing::info!(target: "Main",
            "[FLOW] scale {}, saved {}x{} logical at {}x{} physical, resize to {}",
            plat().scale(), saved.logical_width, saved.logical_height,
            saved.width, saved.height, geom_str);
        let g_c = cs(&geom_str);
        unsafe { jfn_mpv::api::jfn_mpv_set_geometry(g_c.as_ptr()) };
    }

    rate
}

fn start_web_overlay(
    rate: Option<jfn_gpu_paint::RefreshRate>,
    cef: &CefInit,
    user_video_bg: u32,
) -> Result<StartedOverlay, OverlayStartupError> {
    // Must run before the browser is created: the pre-loaded page fires its
    // initial theme-color IPC at DOMContentLoaded.
    let titlebar_themed = jfn_config::titlebar_theme_color();
    unsafe {
        jfn_color::theme::jfn_theme_color_init(
            if titlebar_themed {
                Some(h_theme_set_titlebar)
            } else {
                None
            },
            Some(h_theme_set_mpv_bg),
        );
    }
    jfn_color::theme::jfn_theme_color_set_video_bg(user_video_bg);

    // The overlay creates its browser itself, at the first size the window
    // snapshot and the shell overlay's reserved strip yield.
    let manager = crate::manager::jfn_manager_prepare()?;
    let overlay = jfn_cef::WebOverlay::start(
        &cef.runtime,
        jfn_cef::WebOverlayConfig {
            on_event: std::sync::Arc::new(|event| {
                crate::shell::post(crate::shell::actor::Work::WebEvent(event))
            }),
            frame_rate: rate,
            shared_textures: cef.shared_textures,
            application_menu: crate::app_menu::cef_menu(),
        },
    )?;
    crate::shell::post(crate::shell::actor::Work::WebAttached(overlay.clone()));
    let manager_thread = manager.activate(overlay.clone());

    if let Some(host) = plat().cef_host() {
        host.start_frame_driver(std::sync::Arc::new({
            let overlay = overlay.clone();
            move || overlay.send_external_begin_frame()
        }));
    }

    Ok(StartedOverlay {
        manager_thread,
        overlay,
    })
}

fn application_metadata(cef_version: CefVersion) -> std::io::Result<ApplicationMetadata> {
    let log_path = jfn_logging::active_path();
    Ok(ApplicationMetadata {
        app_version: APP_VERSION_FULL.to_owned(),
        cef_version: cef_version.to_string(),
        config_dir: std::path::absolute(jfn_paths::config_dir())?,
        log_file: if log_path.is_empty() {
            None
        } else {
            Some(std::path::absolute(log_path)?)
        },
    })
}

pub fn jfn_app_main() -> c_int {
    crate::platform_install::install_early();

    let cef_runtime = match LoadedCef::load() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("CEF startup: {error}");
            return 1;
        }
    };
    let cef_runtime = match cef_runtime.dispatch() {
        ProcessDispatch::Browser(runtime) => runtime,
        ProcessDispatch::SubprocessExit(code) => return code,
    };

    let version = cef_runtime.version().clone();

    // Path overrides must be applied before settings load and CEF
    // root_cache_path construction below.
    let cli = cli::Cli::parse();
    if cli.version {
        print_version(&version);
        return 0;
    }
    if let Some(path) = &cli.config_dir {
        jfn_paths::set_config_dir_override(path.into());
    }
    if let Some(path) = &cli.cache_dir {
        jfn_paths::set_cache_dir_override(path.into());
    }

    let settings_path = jfn_paths::config_dir().join("settings.json");
    jfn_config::settings_init(&settings_path);
    jfn_config::settings_load();

    let opts = resolve_startup_options(&cli);

    init_logging(opts.log_file.as_deref(), &opts.log_level, &version);
    let metadata = match application_metadata(version) {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::error!(target: "Main", "application metadata: {error}");
            return 1;
        }
    };

    crate::platform_install::install_from_cli(&cli);

    let _ = crate::window_geometry::controller();

    crate::manager::prepare_shutdown();
    plat().install_shutdown_handler(jfn_playback::jfn_shutdown_initiate);

    let instance = match Instance::for_config_dir(&jfn_paths::config_dir()) {
        Ok(instance) => instance,
        Err(e) => {
            tracing::error!(target: "Main", "establishing instance identity: {e}");
            return 1;
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(target: "Main", "tokio runtime: {e}");
            return 1;
        }
    };
    // The accept loop lives on `runtime`'s workers while `run_app` blocks the
    // main thread on the native loop, so `runtime` must outlive `run_app`.
    match runtime.block_on(Listener::try_start(
        &instance,
        jfn_instance_ipc::jfn::handle,
    )) {
        Start::Started(_listener) => run_app(&instance, opts, cef_runtime, metadata),
        Start::AlreadyRunning => runtime.block_on(notify_running(&instance)),
        Start::Failed(e) => {
            tracing::error!(target: "Main", "could not start instance IPC: {e}");
            1
        }
    }
}

async fn notify_running(instance: &Instance) -> c_int {
    let acked = async {
        let mut stream = Stream::connect(instance).await?;
        stream.send(&Request::Ping).await?;
        stream.recv::<Response>().await
    }
    .await;
    match acked {
        Ok(Some(_)) => tracing::info!(target: "Main", "Signaled existing instance, exiting"),
        Ok(None) => tracing::warn!(target: "Main", "existing instance closed without ack"),
        Err(e) => tracing::warn!(target: "Main", "could not signal existing instance: {e}"),
    }
    0
}

fn run_app(
    instance: &Instance,
    opts: StartupOptions,
    runtime: BrowserCef,
    metadata: ApplicationMetadata,
) -> c_int {
    let prepared = match jfn_platform_abi::PreparedPlatform::claim(plat()) {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::error!(target: "Main", "{error}");
            return 1;
        }
    };
    let resources = StartupResources {
        services: Services {
            fonts: Some(crate::shell::shell_warm_fonts()),
            cleaned: false,
            cef: None,
            shell: None,
            playback: None,
        },
        native: NativeOwner(Some(PreparedNative { prepared })),
    };

    // Boot geometry resolves before the host prepare so its display probes
    // hit the real server, not the mpv proxy the prepare may install.
    let Some(boot) = crate::window_geometry::controller().boot() else {
        tracing::error!(target: "Main", "boot geometry unrepresentable at the reported scale");
        return 1;
    };
    let mpv_boot = plat().window_owner().apply_boot_geometry(&boot);

    setup_mpv_environment();

    // Hosts that own their toplevel create it here, before mpv init, so its
    // window ID can be handed to mpv as `wid`.
    plat().mpv_host().ensure_host_window();

    let mpv_log_level = mpv_log_level_from_filter();

    // mpv's --geometry takes physical pixels (see m_geometry_apply in
    // third_party/mpv/options/m_option.c). Window boot options only apply
    // when mpv owns the window; toplevel-owning backends size and
    // position/maximize the host window themselves.
    let backend_byte: u8 = plat().display() as u8;
    let mpv_started = std::time::Instant::now();
    let raw = init_mpv_handle(MpvInitOptions {
        backend_byte,
        boot_geometry: mpv_boot.as_ref().map(|w| w.geometry.as_str()),
        boot_force_position: mpv_boot.as_ref().is_some_and(|w| w.force_position),
        boot_window_max: mpv_boot.as_ref().is_some_and(|w| w.maximized),
        embed_wid: plat().mpv_host().embed_wid(),
        hwdec: opts.hwdec,
        audio_passthrough: &opts.audio_passthrough,
        audio_exclusive: opts.audio_exclusive,
        audio_channels: &opts.audio_channels,
        mpv_log_level,
    });
    let Some(mpv) = MpvRuntime::new(raw) else {
        tracing::error!(target: "Main", "mpv handle init failed");
        return 1;
    };
    let mut resources = resources.with_mpv(mpv);
    tracing::info!(target: "Main",
        "[FLOW] mpv handle initialized in {} ms", mpv_started.elapsed().as_millis());

    if !jfn_playback::ingest_driver::jfn_playback_observe_mpv_properties(backend_byte) {
        tracing::error!(target: "Main", "observe_mpv_properties failed");
        return 1;
    }

    // force-window=yes keeps VO creation on mpv's core thread. The user's
    // mpv.conf color is only known after mpv_initialize parsed the config, so
    // the capture is async: the reply lands in the boot pump, which writes the
    // override and gates boot readiness on it.
    jfn_mpv::api::jfn_mpv_request_background_color();

    // input-default-bindings=no drops the builtin CLOSE_WIN -> quit binding;
    // the WM close button needs it back.
    install_mpv_close_binding();

    resources.native.get_mut().mpv.window_wake = Some(wake_mpv_on_window_change());

    let boot = BackgroundCapture::Pending;

    // Platform init precedes both the shell overlay and `CefInitialize`: the
    // overlay's surface needs the backend's compositor devices, and the
    // connect screen must be on screen while CEF is still starting.
    let mut resources = match resources.initialize() {
        Ok(resources) => resources,
        Err(error) => {
            tracing::error!(target: "Main", "{error}");
            return 1;
        }
    };
    let platform = &resources.native.get().platform;
    tracing::info!(target: "Main", "Platform init ok");
    jfn_platform_abi::set_about_handler(crate::shell::shell_open_about);
    jfn_platform_abi::set_client_settings_handler(crate::shell::shell_open_client_settings);
    let shell_readiness =
        match crate::shell::shell_start(platform, metadata, crate::app_menu::shell_actions()) {
            Ok((shell, readiness)) => {
                resources.services.shell = Some(shell);
                Some(readiness)
            }
            Err(error) => {
                tracing::error!(target: "Main", "shell overlay unavailable: {error}");
                return 1;
            }
        };
    // fontdb's directory walk must not run while Chromium is manipulating
    // process file descriptors.
    if let Some(fonts) = resources.services.fonts.take()
        && let Err(error) = fonts.join()
    {
        tracing::error!(target: "Main", "{error}");
        return 1;
    }

    // CEF's process bring-up needs nothing mpv owns; where the platform
    // allows it, it runs while the core thread builds the VO and its GPU
    // context instead of after.
    let deferred = if plat().cef_init_precedes_mpv_window() {
        match initialize_cef(runtime, platform, &opts) {
            Ok(cef) => {
                resources.services.cef = Some(cef);
                None
            }
            Err(error) => {
                tracing::error!(target: "Main", "{error}");
                return 1;
            }
        }
    } else {
        Some(runtime)
    };
    let boot = match wait_for_vo_window(boot) {
        Ok(boot) => boot,
        Err(error) => {
            let code = error.exit_code();
            tracing::info!(target: "Main", "{error}");
            return code;
        }
    };
    if let Some(readiness) = shell_readiness {
        match readiness.wait(platform, std::time::Duration::from_secs(5)) {
            Ok(()) => tracing::debug!(target: "Main", "shell renderer ready"),
            Err(error) => {
                // Product policy: failure of the connect/settings UI is fatal.
                tracing::error!(target: "Main", "shell startup: {error}");
                return 1;
            }
        }
    }
    log_mpv_versions();
    run_with_cef(boot, deferred, resources, &opts, instance)
}

// =====================================================================
// mpv boot helpers + VO wait loop
// =====================================================================

fn mpv_log_level_from_filter() -> &'static str {
    let e = |level| jfn_logging::log_enabled(Category::Mpv, level);
    if e(Level::Trace) {
        "debug"
    } else if e(Level::Debug) {
        "v"
    } else if e(Level::Info) {
        "info"
    } else if e(Level::Warn) {
        "warn"
    } else if e(Level::Error) {
        "error"
    } else {
        "no"
    }
}

/// What one drained libmpv event means for the boot wait.
enum BootEvent {
    /// The queue was empty, or the parked wait timed out.
    Idle,
    /// mpv is going away before its window came up.
    Fatal,
    /// Folded into boot state.
    Consumed,
}

/// Log messages reach tracing, the background-color reply applies the startup
/// override, every other event reaches the ingest layer.
fn consume_boot_event(boot: &mut BackgroundCapture, event: jfn_mpv::api::WaitEvent) -> BootEvent {
    match event {
        jfn_mpv::api::WaitEvent::None => BootEvent::Idle,
        jfn_mpv::api::WaitEvent::LogMessage(m) => {
            jfn_mpv::forward_log_to_tracing(&m);
            BootEvent::Consumed
        }
        jfn_mpv::api::WaitEvent::Event(jfn_mpv::Event::Shutdown | jfn_mpv::Event::EndFile(_)) => {
            BootEvent::Fatal
        }
        jfn_mpv::api::WaitEvent::Event(jfn_mpv::Event::GetPropertyReply {
            reply: jfn_mpv::api::BACKGROUND_COLOR_REPLY,
            ref value,
            ..
        }) => {
            apply_startup_background(boot, value);
            BootEvent::Consumed
        }
        jfn_mpv::api::WaitEvent::Event(event) => {
            jfn_playback::ingest_driver::jfn_playback_ingest_mpv_event_owned(&event, plat());
            BootEvent::Consumed
        }
    }
}

/// Applies the startup override and records the captured color as one state.
fn apply_startup_background(boot: &mut BackgroundCapture, value: &jfn_mpv::PropertyValue) {
    let user_video_bg = jfn_mpv::api::background_color_from_reply(value).unwrap_or(0);
    let startup_bg = cs(STARTUP_BG_HEX);
    unsafe { jfn_mpv::api::jfn_mpv_set_background_color_hex(startup_bg.as_ptr()) };
    *boot = BackgroundCapture::Applied { user_video_bg };
}

/// Produces startup evidence from the current window and captured background.
fn boot_progress(boot: &BackgroundCapture) -> Option<ReadyBoot> {
    let BackgroundCapture::Applied { user_video_bg } = boot else {
        return None;
    };
    crate::window_geometry::controller()
        .source()
        .snapshot()
        .extent?;
    if !plat().mpv_host().host_ready() {
        return None;
    }
    Some(ReadyBoot {
        user_video_bg: *user_video_bg,
    })
}

// =====================================================================
// run_with_cef body — Rust port
// =====================================================================

fn cef_severity_for_cef_filter() -> jfn_cef::LogSeverity {
    let enabled = |level| jfn_logging::log_enabled(Category::Cef, level);
    if enabled(Level::Trace) || enabled(Level::Debug) {
        jfn_cef::LogSeverity::Verbose
    } else if enabled(Level::Info) {
        jfn_cef::LogSeverity::Info
    } else if enabled(Level::Warn) {
        jfn_cef::LogSeverity::Warning
    } else {
        jfn_cef::LogSeverity::Error
    }
}

// Handler thunks installed via jfn_playback_set_*_handler. They capture
// nothing (Rust function items are 'static) and forward to the platform
// backend / jfn-cef.

extern "C" fn h_idle_inhibit(level: u32) {
    let lvl = match level {
        1 => IdleInhibitLevel::System,
        2 => IdleInhibitLevel::Display,
        _ => IdleInhibitLevel::None,
    };
    plat().set_idle_inhibit(lvl);
}
extern "C" fn h_theme_video_mode(active: bool) {
    jfn_color::theme::jfn_theme_color_set_video_mode(active);
}
extern "C" fn h_web_exec_js(js: *const c_char) {
    if js.is_null() {
        return;
    }
    let Some(overlay) = WEB_OVERLAY.lock().clone() else {
        return;
    };
    let js = unsafe { CStr::from_ptr(js) }.to_string_lossy();
    overlay.exec_js(&js);
}
extern "C" fn h_browsers_set_refresh_rate(hz: f64) {
    tracing::info!(target: "Main", "Display refresh rate changed: {hz} Hz");
    let Some(rate) = jfn_gpu_paint::RefreshRate::from_hz(hz) else {
        return;
    };
    jfn_gpu_paint::report_refresh(jfn_gpu_paint::RefreshSource::MpvDisplayFps, rate);
    if let Some(overlay) = WEB_OVERLAY.lock().clone() {
        overlay.set_refresh_rate(rate);
    }
}
extern "C" fn h_theme_set_titlebar(rgb: u32) {
    plat().set_theme_color(rgb);
}
extern "C" fn h_theme_set_mpv_bg(hex: *const c_char) {
    unsafe { jfn_mpv::api::jfn_mpv_set_background_color_hex(hex) };
}

/// Owns the run_with_cef body — invoked once by `jfn_app_main`.
fn run_with_cef(
    boot: ReadyBoot,
    deferred: Option<BrowserCef>,
    mut resources: StartupResources<InitializedNative>,
    opts: &StartupOptions,
    instance: &Instance,
) -> c_int {
    let mpv_raw = resources.native.get().mpv.raw();
    if jfn_config::titlebar_theme_color() {
        plat().set_theme_color(0x101010);
    }
    publish_device_profile(mpv_raw);
    if let Some(runtime) = deferred {
        let platform = &resources.native.get().platform;
        match initialize_cef(runtime, platform, opts) {
            Ok(cef) => resources.services.cef = Some(cef),
            Err(error) => {
                tracing::error!(target: "Main", "{error}");
                return 1;
            }
        }
    }
    let rate = boot_mpv_reconcile();
    resources.services.playback = Some(initialize_playback_coordination());
    let Some(cef) = resources.services.cef.as_ref() else {
        tracing::error!(target: "Main", "startup invariant violated: overlay acquisition requires initialized CEF");
        return 1;
    };
    let StartedOverlay {
        manager_thread,
        overlay,
    } = match start_web_overlay(rate, cef, boot.user_video_bg) {
        Ok(started) => started,
        Err(error) => {
            tracing::error!(target: "Main", "overlay startup: {error}");
            return 1;
        }
    };
    WEB_OVERLAY.lock().replace(overlay.clone());
    RunningRuntime {
        resources,
        manager_thread,
        _overlay: overlay,
    }
    .run(instance)
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn fatal_boot_event_is_failure_and_only_requested_cancellation_succeeds() {
        let mut boot = BackgroundCapture::Pending;
        assert!(matches!(
            consume_boot_event(
                &mut boot,
                jfn_mpv::api::WaitEvent::Event(jfn_mpv::Event::Shutdown)
            ),
            BootEvent::Fatal
        ));
        assert_eq!(BootError::MpvTerminated.exit_code(), 1);
        assert_eq!(BootError::Canceled.exit_code(), 0);
        assert_eq!(
            BootError::Wake(std::io::Error::other("injected wake failure")).exit_code(),
            1
        );
    }
    #[test]
    fn native_owner_cleanup_runs_exactly_once_after_explicit_teardown() {
        struct Fake(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl NativePhase for Fake {
            fn cleanup(self) -> Result<(), CleanupError> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
        }
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut owner = NativeOwner(Some(Fake(calls.clone())));
        assert!(owner.cleanup().is_ok());
        drop(owner);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
    #[test]
    fn native_owner_rolls_back_during_unwind() {
        struct Fake(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl NativePhase for Fake {
            fn cleanup(self) -> Result<(), CleanupError> {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
        }
        let cleaned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let captured = cleaned.clone();
        assert!(
            std::panic::catch_unwind(|| {
                let _owner = NativeOwner(Some(Fake(captured)));
                std::panic::resume_unwind(Box::new("injected startup panic"));
            })
            .is_err()
        );
        assert!(cleaned.load(std::sync::atomic::Ordering::Relaxed));
    }
}
