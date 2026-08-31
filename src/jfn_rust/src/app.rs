//! Process entry point. [`jfn_app_main`] owns the full main loop and
//! returns the exit code.

use std::ffi::{CStr, CString, c_char, c_int};
use std::path::{Path, PathBuf};
use std::ptr;
use std::thread::JoinHandle;
use std::time::Duration;

use clap::Parser;
use jfn_cef::{APP_VERSION_FULL, cef_version};
use jfn_instance_ipc::jfn::{Request, Response};
use jfn_instance_ipc::{Listener, Start, Stream};
use jfn_logging::{Category, Level};
use jfn_platform_abi::{IdleInhibitLevel, Instance, Platform, WindowGeometry};

use crate::cli;

// Shorthand for the installed Platform backend. `install()` happens before
// any of the call sites here run.
fn plat() -> &'static dyn Platform {
    jfn_platform_abi::get()
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

// Read once by `jfn_app_main` after CEF boot to seed the theme rotator.
static VIDEO_BG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn video_bg_set(rgb: u32) {
    VIDEO_BG.store(rgb, std::sync::atomic::Ordering::Release);
}

fn video_bg_get() -> u32 {
    VIDEO_BG.load(std::sync::atomic::Ordering::Acquire)
}

/// mpv background applied over the user's mpv.conf color for the app's
/// lifetime before the theme rotator takes over.
const STARTUP_BG_HEX: &str = "#101010";

/// Set once the startup background override has replaced the user's color.
static STARTUP_BG_APPLIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Shared-texture decision `CefInitialize` was given.
static SHARED_TEXTURES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether CEF was initialized with shared-texture compositing.
fn shared_textures() -> bool {
    SHARED_TEXTURES.load(std::sync::atomic::Ordering::Acquire)
}

pub(crate) const DEFAULT_LOG_FILTER: &str = "info";

struct BootArgs {
    disable_gpu_compositing: bool,
    remote_debugging_port: c_int,
}

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

fn print_version() {
    println!(
        "jellium-desktop {}\n\nCEF {}\n",
        APP_VERSION_FULL,
        cef_version()
    );
    use std::io::Write;
    let _ = std::io::stdout().flush();
    jfn_mpv::probe::jfn_mpv_print_version_info();
}

fn init_logging(log_file: Option<&Path>, log_level: &str) {
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
    tracing::info!(target: "Main", "CEF {}", cef_version());
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
fn wake_mpv_on_window_change() {
    jfn_platform_abi::subscribe_window_changed(jfn_mpv::api::jfn_mpv_wakeup);
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
    hwdec: String,
    audio_passthrough: String,
    audio_exclusive: bool,
    audio_channels: String,
    log_level: String,
    /// The file logs are written to; `None` disables file logging.
    log_file: Option<PathBuf>,
    disable_gpu_compositing: bool,
    remote_debugging_port: c_int,
}

fn resolve_startup_options(cli: &cli::Cli) -> StartupOptions {
    let saved_hwdec = jfn_config::hwdec();
    let saved_pass = jfn_config::audio_passthrough();
    let saved_chans = jfn_config::audio_channels();
    let saved_log_level = jfn_config::log_level();
    let saved_audio_exclusive = jfn_config::audio_exclusive();

    let mpv_hwdec_default = jfn_mpv::HWDEC_DEFAULT.to_string();

    let mut hwdec = if saved_hwdec.is_empty() {
        mpv_hwdec_default.clone()
    } else {
        saved_hwdec
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
    let mut remote_debugging_port: c_int = 0;

    if let Some(v) = cli.hwdec.clone() {
        hwdec = v;
    }
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
    if let Some(p) = cli.remote_debug_port {
        remote_debugging_port = p;
    }

    if !jfn_mpv::is_valid_hwdec(&hwdec) {
        hwdec = mpv_hwdec_default;
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
    hwdec: &'a str,
    audio_passthrough: &'a str,
    audio_exclusive: bool,
    audio_channels: &'a str,
    mpv_log_level: &'a str,
}

fn init_mpv_handle(opts: MpvInitOptions<'_>) -> *mut jfn_mpv::sys::mpv_handle {
    let geometry_c = opts.boot_geometry.map(cs);
    let hwdec_c = cs(opts.hwdec);
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

/// Blocks until the window source has a usable extent. Returns false on
/// a fatal mpv event (shutdown before the VO came up).
fn wait_for_vo_window() -> bool {
    tracing::info!(target: "Main", "Waiting for mpv window...");
    let started = std::time::Instant::now();

    let mut fatal = false;

    // The platform owns the wait strategy; this pump owns all mpv event
    // handling. It drains everything mpv has queued without blocking, then,
    // when the platform's strategy grants a block budget, parks in mpv for at
    // most that long.
    plat().mpv_host().run_vo_wait(&mut |budget: Duration| {
        loop {
            match consume_boot_event(jfn_mpv::api::wait_event_owned(0.0)) {
                BootEvent::Idle => break,
                BootEvent::Fatal => {
                    fatal = true;
                    return false;
                }
                BootEvent::Consumed => {}
            }
        }
        if boot_ready() {
            return false;
        }
        if !budget.is_zero()
            && matches!(
                consume_boot_event(jfn_mpv::api::wait_event_owned(budget.as_secs_f64())),
                BootEvent::Fatal
            )
        {
            fatal = true;
            return false;
        }
        true
    });

    if fatal {
        return false;
    }
    tracing::info!(target: "Main",
        "mpv window ready in {} ms", started.elapsed().as_millis());
    true
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

/// `CefInitialize` with the flags this boot resolved, recording the
/// shared-texture decision for [`shared_textures`]. A call after a successful
/// one returns true without re-entering CEF.
fn ensure_cef_initialized(ba: &BootArgs) -> bool {
    if CEF_INITED.load(std::sync::atomic::Ordering::Acquire) {
        return true;
    }
    let use_shared_textures = plat().shared_texture_supported() && !ba.disable_gpu_compositing;
    SHARED_TEXTURES.store(use_shared_textures, std::sync::atomic::Ordering::Release);
    jfn_cef::ffi::jfn_cef_set_log_severity(cef_severity_for_cef_filter());
    jfn_cef::ffi::jfn_cef_set_remote_debugging_port(ba.remote_debugging_port);
    jfn_cef::ffi::jfn_cef_set_disable_gpu_compositing(!use_shared_textures);
    jfn_cef::ffi::jfn_cef_set_platform_switches(plat().display());
    tracing::info!(target: "Main", "[FLOW] calling CefInitialize...");
    let started = std::time::Instant::now();
    if !jfn_cef::ffi::jfn_cef_initialize() {
        tracing::error!(target: "Main", "CefInitialize failed");
        return false;
    }
    CEF_INITED.store(true, std::sync::atomic::Ordering::Release);
    tracing::info!(target: "Main",
        "[FLOW] CefInitialize returned ok in {} ms", started.elapsed().as_millis());
    true
}

fn start_playback_coordination(instance: &Instance) -> bool {
    jfn_playback::ffi::jfn_playback_init();
    COORD_INITED.store(true, std::sync::atomic::Ordering::Release);

    jfn_playback::idle_inhibit_sink::jfn_playback_set_idle_inhibit_handler(Some(h_idle_inhibit));
    jfn_playback::theme_color_sink::jfn_playback_set_theme_video_mode_handler(Some(
        h_theme_video_mode,
    ));
    jfn_playback::exec_js::jfn_playback_set_web_exec_js_handler(Some(h_web_exec_js));
    jfn_playback::browser_sink::jfn_playback_set_browsers_refresh_rate_handler(Some(
        h_browsers_set_refresh_rate,
    ));

    plat().media_session().start(instance);

    jfn_playback::ingest_driver::jfn_playback_set_fullscreen_handler(|fs| {
        plat().set_fullscreen(fs)
    });
    jfn_playback::ingest_driver::jfn_playback_set_shutdown_handler(|| {
        tracing::info!(target: "Main", "MPV_EVENT_SHUTDOWN received");
        jfn_playback::jfn_shutdown_initiate();
    });

    tracing::info!(target: "Main", "[FLOW] starting Rust-owned mpv event thread");
    if !jfn_playback::ingest_driver::jfn_playback_start_mpv_event_thread() {
        tracing::error!(target: "Main", "failed to start mpv event thread");
        return false;
    }

    true
}

fn shutdown_runtime(
    manager_thread: JoinHandle<Result<(), crate::manager::ManagerError>>,
    overlay: jfn_cef::WebOverlay,
) -> Result<(), RuntimeShutdownError> {
    // Persist before the join below: it can block on a VO-teardown
    // roundtrip, and a hang there must not cost the window geometry.
    crate::window_geometry::controller().persist();
    jfn_config::settings_save();

    // Join before any teardown so no posted task outlives the layer free below.
    let manager = manager_thread
        .join()
        .map_err(|_| RuntimeShutdownError::ManagerJoinFailed)
        .and_then(|r| r.map_err(RuntimeShutdownError::from));

    // Sever host↔mpv links that could deadlock the teardown below once
    // CEF threads start dying.
    plat().mpv_host().detach();

    jfn_color::theme::jfn_theme_color_shutdown();
    plat().media_session().stop();

    jfn_playback::ingest_driver::jfn_playback_stop_mpv_event_thread();

    jfn_config::settings_shutdown_save_worker();

    jfn_shell::shell_shutdown();
    WEB_OVERLAY.lock().take();
    drop(overlay);
    jfn_cef::ffi::jfn_cef_shutdown();
    CEF_INITED.store(false, std::sync::atomic::Ordering::Release);

    plat().set_idle_inhibit(IdleInhibitLevel::None);

    plat().cleanup();
    PLATFORM_INITED.store(false, std::sync::atomic::Ordering::Release);

    jfn_playback::ffi::jfn_playback_shutdown();
    COORD_INITED.store(false, std::sync::atomic::Ordering::Release);

    manager
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
    use_shared_textures: bool,
) -> (
    JoinHandle<Result<(), crate::manager::ManagerError>>,
    jfn_cef::WebOverlay,
) {
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
    jfn_color::theme::jfn_theme_color_set_video_bg(video_bg_get());

    // The overlay creates its browser itself, at the first size the window
    // snapshot and the shell overlay's reserved strip yield.
    let overlay = jfn_cef::WebOverlay::start(jfn_cef::WebOverlayConfig {
        frame_rate: rate,
        shared_textures: use_shared_textures,
    });
    let manager_thread = crate::manager::jfn_manager_start(overlay.clone());
    jfn_playback::jfn_shutdown_set_handler(Some(h_shutdown_wake_manager));

    if let Some(host) = plat().cef_host() {
        host.start_frame_driver(std::sync::Arc::new({
            let overlay = overlay.clone();
            move || overlay.send_external_begin_frame()
        }));
    }

    (manager_thread, overlay)
}

pub fn jfn_app_main() -> c_int {
    crate::platform_install::install_early();

    let rc = jfn_cef::ffi::jfn_cef_start();
    if rc >= 0 {
        return rc;
    }

    // Path overrides must be applied before settings load and CEF
    // root_cache_path construction below.
    let cli = cli::Cli::parse();
    if cli.version {
        print_version();
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

    init_logging(opts.log_file.as_deref(), &opts.log_level);

    crate::platform_install::install_from_cli(&cli);

    let _ = crate::window_geometry::controller();

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
        Start::Started(_listener) => run_app(&instance, opts),
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

fn run_app(instance: &Instance, opts: StartupOptions) -> c_int {
    let fonts = jfn_shell::shell_warm_fonts();

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
        hwdec: &opts.hwdec,
        audio_passthrough: &opts.audio_passthrough,
        audio_exclusive: opts.audio_exclusive,
        audio_channels: &opts.audio_channels,
        mpv_log_level,
    });
    if raw.is_null() {
        tracing::error!(target: "Main", "mpv handle init failed");
        return 1;
    }
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

    wake_mpv_on_window_change();

    let boot_args = BootArgs {
        disable_gpu_compositing: opts.disable_gpu_compositing,
        remote_debugging_port: opts.remote_debugging_port,
    };

    // Platform init precedes both the shell overlay and `CefInitialize`: the
    // overlay's surface needs the backend's compositor devices, and the
    // connect screen must be on screen while CEF is still starting.
    if !plat().init(raw as *mut std::ffi::c_void) {
        tracing::error!(target: "Main", "Platform init failed");
        return 1;
    }
    tracing::info!(target: "Main", "Platform init ok");
    PLATFORM_INITED.store(true, std::sync::atomic::Ordering::Release);

    jfn_platform_abi::set_about_handler(jfn_shell::shell_open_about);
    jfn_platform_abi::set_client_settings_handler(jfn_shell::shell_open_client_settings);
    if jfn_shell::shell_start().is_none() {
        tracing::warn!(target: "Main", "shell overlay unavailable; no connect screen");
    }
    // fontdb's directory walk must not run while Chromium is manipulating
    // process file descriptors.
    let _ = fonts.join();

    // CEF's process bring-up needs nothing mpv owns; where the platform
    // allows it, it runs while the core thread builds the VO and its GPU
    // context instead of after.
    if plat().cef_init_precedes_mpv_window() && !ensure_cef_initialized(&boot_args) {
        return 1;
    }

    if !wait_for_vo_window() {
        return 0;
    }

    log_mpv_versions();

    let rc = unsafe { run_with_cef(&boot_args, instance) };
    if rc != 0 {
        return rc;
    }

    // macOS must run TerminateDestroy off the main thread (mpv's VO uninit
    // does DispatchQueue.main.sync); run_blocking keeps main pumping.
    plat().run_blocking(Box::new(jfn_mpv::boot::jfn_mpv_handle_terminate));

    plat().post_window_cleanup();

    0
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
fn consume_boot_event(event: jfn_mpv::api::WaitEvent) -> BootEvent {
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
            apply_startup_background(value);
            BootEvent::Consumed
        }
        jfn_mpv::api::WaitEvent::Event(event) => {
            jfn_playback::ingest_driver::jfn_playback_ingest_mpv_event_owned(&event);
            BootEvent::Consumed
        }
    }
}

/// Stores the user's color for the theme rotator, then writes
/// [`STARTUP_BG_HEX`] in its place. Latches [`STARTUP_BG_APPLIED`] even when
/// the reply carried no value.
fn apply_startup_background(value: &jfn_mpv::PropertyValue) {
    if let Some(user_bg) = jfn_mpv::api::background_color_from_reply(value) {
        video_bg_set(user_bg);
        tracing::info!(target: "Main", "video bg captured: #{user_bg:06x}");
    }
    let startup_bg = cs(STARTUP_BG_HEX);
    unsafe { jfn_mpv::api::jfn_mpv_set_background_color_hex(startup_bg.as_ptr()) };
    STARTUP_BG_APPLIED.store(true, std::sync::atomic::Ordering::Release);
}

/// Ready once the window authority reports an extent, the host's own startup
/// gate is open, and the startup background override has landed. The window's
/// mode is never a boot precondition: where mpv owns the toplevel the
/// `window-maximized` report is an echo of the boot option, and where a host
/// owns the toplevel the WM/compositor may decline the maximize outright.
fn boot_ready() -> bool {
    crate::window_geometry::controller()
        .source()
        .snapshot()
        .extent
        .is_some()
        && plat().mpv_host().host_ready()
        && STARTUP_BG_APPLIED.load(std::sync::atomic::Ordering::Acquire)
}

// =====================================================================
// run_with_cef body — Rust port
// =====================================================================

// cef_log_severity_t ABI: 1 VERBOSE, 2 INFO, 3 WARNING, 4 ERROR.
// Must match `jfn_cef::ffi::log_severity_from_int` / `client/events.rs`.
const LOG_SEVERITY_VERBOSE: c_int = 1;
const LOG_SEVERITY_INFO: c_int = 2;
const LOG_SEVERITY_WARNING: c_int = 3;
const LOG_SEVERITY_ERROR: c_int = 4;

fn cef_severity_for_cef_filter() -> c_int {
    // Map the CEF category level to CEF severity:
    //   Trace/Debug -> VERBOSE, Info -> INFO, Warn -> WARNING, Error -> ERROR.
    let e = |level| jfn_logging::log_enabled(Category::Cef, level);
    if e(Level::Trace) || e(Level::Debug) {
        LOG_SEVERITY_VERBOSE
    } else if e(Level::Info) {
        LOG_SEVERITY_INFO
    } else if e(Level::Warn) {
        LOG_SEVERITY_WARNING
    } else {
        LOG_SEVERITY_ERROR
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

fn h_shutdown_wake_manager() {
    // Runs inline on whichever thread called jfn_shutdown_initiate (signal
    // handler, CEF dispatch, input thread, …). Signal-only by contract: just
    // wake the manager, which orchestrates the close/drain off-thread. Never
    // close a browser or wake the main loop here — that would reenter CEF or
    // race the drain.
    crate::manager::jfn_manager_notify_shutdown();
}

/// Owns the run_with_cef body — invoked once by `jfn_app_main`.
unsafe fn run_with_cef(ba: &BootArgs, instance: &Instance) -> c_int {
    // Platform init already ran in `run_app`, ahead of the shell overlay and
    // `CefInitialize`. Cleanup still happens in shutdown_runtime.
    let mpv_raw = jfn_mpv::boot::jfn_mpv_handle_get();

    // 3. Apply titlebar theme color before CefInitialize so the window doesn't
    //    sit with the system default palette during init.
    if jfn_config::titlebar_theme_color() {
        plat().set_theme_color(0x101010);
    }

    // 4. Build device profile. Must run after VO-init wait — sync mpv API
    //    calls would deadlock against core_thread on macOS.
    publish_device_profile(mpv_raw);

    // 5. CEF init flags + initialise.
    if !ensure_cef_initialized(ba) {
        return 1;
    }

    let rate = boot_mpv_reconcile();

    let (manager_thread, overlay) = start_web_overlay(rate, shared_textures());
    WEB_OVERLAY.lock().replace(overlay.clone());

    if !start_playback_coordination(instance) {
        return 1;
    }

    tracing::info!(target: "Main", "[FLOW] Running — about to enter run_main_loop");

    // 15. Park the main thread until the manager has closed + drained every
    //     browser, at which point it calls plat().wake_main_loop() to release
    //     us. Unified across platforms: macOS parks in [NSApp run] (whose
    //     pump runs the posted close + OnBeforeClose while the manager waits);
    //     other platforms park on the Condvar main-park. Exit is driven by the
    //     shutdown signal (routed through the manager), never by transient
    //     browser-close state when the overlay resets the main layer.
    plat().run_main_loop();
    tracing::info!(target: "Main", "[FLOW] run_main_loop returned — browsers drained, running teardown");

    if let Err(e) = shutdown_runtime(manager_thread, overlay) {
        tracing::error!(target: "Main", "shutdown: {e}");
        return 1;
    }

    0
}

static PLATFORM_INITED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CEF_INITED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static COORD_INITED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
