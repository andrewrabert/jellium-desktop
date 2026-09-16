use cef::*;
#[cfg(not(windows))]
use std::ffi::CString;
#[cfg(not(windows))]
use std::os::raw::c_char;
use std::os::raw::c_int;
#[cfg(not(windows))]
use std::sync::OnceLock;

use jfn_platform_abi::DisplayBackend;

use crate::app::{JfnApp, JfnAppBuilder};
use crate::state;

pub(crate) fn jfn_cef_start(_runtime: &crate::LoadedCef) -> c_int {
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let args = args::Args::new();
    let mut app = JfnAppBuilder::new(JfnApp::new());
    execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    )
}

fn platform_switches(backend: DisplayBackend) -> Vec<state::PendingSwitch> {
    let mut c = state::Config::default();
    match backend {
        DisplayBackend::Wayland => {
            c.pending_switches.push(state::PendingSwitch::with_value(
                "ozone-platform",
                "wayland",
            ));
            c.pending_switches.push(state::PendingSwitch::with_value(
                "disable-features",
                "WaylandFractionalScaleV1",
            ));
        }
        DisplayBackend::X11 => {
            c.pending_switches
                .push(state::PendingSwitch::with_value("ozone-platform", "x11"));
        }
        DisplayBackend::MacOS => {
            c.pending_switches
                .push(state::PendingSwitch::flag("single-process"));
            c.pending_switches
                .push(state::PendingSwitch::flag("use-mock-keychain"));
            c.pending_switches
                .push(state::PendingSwitch::with_value("password-store", "basic"));
        }
        DisplayBackend::Windows => {
            c.pending_switches
                .push(state::PendingSwitch::with_value("use-angle", "d3d11"));
            #[cfg(windows)]
            if let Some(luid) =
                jfn_gpu_paint::surfaces().and_then(jfn_gpu_paint::Surfaces::adapter_luid)
            {
                c.pending_switches.push(state::PendingSwitch::with_value(
                    "use-adapter-luid",
                    &format!("{},{}", luid >> 32, luid as u32),
                ));
            }
        }
    };
    c.pending_switches
}

pub(crate) fn jfn_cef_initialize(
    _runtime: &crate::LoadedCef,
    platform: &dyn jfn_platform_abi::Platform,
    options: &crate::InitOptions,
) -> Result<(), crate::InitError> {
    let mut pending_switches = platform_switches(platform.display());
    if options.disable_gpu_compositing {
        pending_switches.push(state::PendingSwitch::flag("disable-gpu-compositing"));
    }
    state::configure(state::Config { pending_switches });

    let settings_path = jfn_paths::config_dir().join("settings.json");
    jfn_config::settings_init(&settings_path);

    let mut settings = Settings {
        no_sandbox: 1,
        windowless_rendering_enabled: 1,
        disable_signal_handlers: 1,
        log_severity: options.log_severity.native(),
        remote_debugging_port: options.remote_debugging_port.native(),
        locale: CefString::from("en-US"),
        user_agent: CefString::from(concat!(
            "Mozilla/5.0 jellium-desktop/",
            env!("JFN_APP_VERSION")
        )),
        root_cache_path: CefString::from(jfn_paths::cache_dir().to_string_lossy().as_ref()),
        ..Settings::default()
    };
    let cef_host = platform.cef_host();
    if cef_host.is_some() {
        settings.external_message_pump = 1;
    } else {
        settings.multi_threaded_message_loop = 1;
    }

    fill_paths(&mut settings, platform);

    if let Some(host) = cef_host {
        host.pump_init();
    }

    let _sig_guard = jfn_platform_abi::SignalGuard::new();

    let mut app = JfnAppBuilder::new(JfnApp::new());
    let full_argv = platform.display().cef_full_browser_argv();
    let main_args = if full_argv {
        args::Args::new().as_main_args().clone()
    } else {
        #[cfg(not(windows))]
        {
            browser_main_args()
        }
        #[cfg(windows)]
        {
            args::Args::new().as_main_args().clone()
        }
    };
    let ok = initialize(
        Some(&main_args),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) == 1;
    if !ok {
        if let Some(host) = cef_host {
            host.pump_shutdown();
        }
        crate::ready::stop();
        return Err(crate::InitError::Native);
    }
    if crate::ready::post_cef_ready().is_err() {
        if let Some(host) = cef_host {
            host.pump_shutdown();
        }
        crate::ready::stop();
        shutdown();
        return Err(crate::InitError::Readiness);
    }
    Ok(())
}

#[cfg(not(windows))]
fn browser_main_args() -> MainArgs {
    struct CleanArgv {
        argc: c_int,
        argv: *mut *mut c_char,
    }
    unsafe impl Send for CleanArgv {}
    unsafe impl Sync for CleanArgv {}

    static CLEAN: OnceLock<CleanArgv> = OnceLock::new();
    let c = CLEAN.get_or_init(|| {
        let program = std::env::args()
            .next()
            .unwrap_or_else(|| "jellium-desktop".to_string());
        let cstr = CString::new(program).unwrap_or_default();
        let cstr_ptr = cstr.as_ptr() as *mut c_char;
        Box::leak(Box::new(cstr));
        let argv_vec: Vec<*mut c_char> = vec![cstr_ptr];
        let leaked: &'static mut Vec<*mut c_char> = Box::leak(Box::new(argv_vec));
        CleanArgv {
            argc: leaked.len() as c_int,
            argv: leaked.as_mut_ptr(),
        }
    });
    MainArgs {
        argc: c.argc,
        argv: c.argv,
    }
}

pub(crate) fn jfn_cef_shutdown(runtime: &crate::InitializedCef) {
    if let Some(host) = runtime.platform().cef_host() {
        host.pump_shutdown();
    }
    shutdown();
}

fn fill_paths(settings: &mut Settings, platform: &dyn jfn_platform_abi::Platform) {
    let paths = platform.cef_paths();
    let set = |dst: &mut CefString, v: Option<std::path::PathBuf>| {
        if let Some(v) = v {
            *dst = CefString::from(v.to_string_lossy().as_ref());
        }
    };
    set(
        &mut settings.browser_subprocess_path,
        paths.browser_subprocess_path,
    );
    set(&mut settings.framework_dir_path, paths.framework_dir_path);
    set(&mut settings.resources_dir_path, paths.resources_dir_path);
    set(&mut settings.locales_dir_path, paths.locales_dir_path);
}
