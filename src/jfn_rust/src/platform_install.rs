//! Composition root for the `Platform` backend: the only place that
//! decides which backend a build installs, and when.
//!
//! Windows and macOS have exactly one backend, known at compile time;
//! Linux picks Wayland or X11 at runtime. That difference also dictates
//! ordering — see the two install functions.

#[cfg(target_os = "linux")]
use jfn_platform_abi::{DisplayBackend, Platform};

/// Install the backend on OSes with a single compile-time backend
/// (Windows, macOS). Must run before `jfn_cef_start`: CEF subprocesses
/// bail out of the browser-process flow but may still query the
/// platform. No-op on Linux ([`install_from_cli`] runs there instead).
pub fn install_early() {
    #[cfg(target_os = "windows")]
    {
        jfn_platform_abi::install(jfn_windows::make_windows_platform());
        jfn_platform_abi::get().early_init();
    }
    #[cfg(target_os = "macos")]
    {
        jfn_platform_abi::install(jfn_macos::make_macos_platform());
        jfn_platform_abi::get().early_init();
    }
}

/// Install the backend on Linux, where Wayland vs X11 is chosen at
/// runtime from `--platform` / session env. Runs after logging init so
/// the backend choice is logged. No-op on OSes whose backend
/// [`install_early`] already installed.
pub fn install_from_cli(cli: &crate::cli::Cli) {
    #[cfg(target_os = "linux")]
    {
        let backend = match cli.linux.platform {
            Some(jfn_linux_util::cli::PlatformArg::Wayland) => DisplayBackend::Wayland,
            Some(jfn_linux_util::cli::PlatformArg::X11) => DisplayBackend::X11,
            None => {
                let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
                let has_display = std::env::var_os("DISPLAY").is_some();
                if has_wayland || !has_display {
                    DisplayBackend::Wayland
                } else {
                    DisplayBackend::X11
                }
            }
        };
        if backend == DisplayBackend::X11
            && let Some(p) = cli.linux.platform_paint
        {
            jfn_x11::set_paint_override(match p {
                jfn_linux_util::cli::Paint::Dmabuf => jfn_x11::X11PaintOverride::Dmabuf,
                jfn_linux_util::cli::Paint::Gpu => jfn_x11::X11PaintOverride::Gpu,
                jfn_linux_util::cli::Paint::Shm => jfn_x11::X11PaintOverride::Shm,
            });
        }

        let p: Box<dyn Platform> = match backend {
            DisplayBackend::Wayland => jfn_wayland::make_platform::make_wayland_platform(
                cli.linux.platform_paint.map(|p| match p {
                    jfn_linux_util::cli::Paint::Dmabuf => jfn_wayland::WlPaintOverride::Dmabuf,
                    jfn_linux_util::cli::Paint::Gpu => jfn_wayland::WlPaintOverride::Gpu,
                    jfn_linux_util::cli::Paint::Shm => jfn_wayland::WlPaintOverride::Shm,
                }),
            ),
            DisplayBackend::X11 => jfn_x11::make_platform::make_x11_platform(),
            _ => unreachable!(),
        };
        jfn_platform_abi::install(p);
        jfn_platform_abi::get().early_init();
        tracing::info!(target: "Main", "Display backend: {}",
            if backend == DisplayBackend::Wayland { "wayland" } else { "x11" });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cli;
}
