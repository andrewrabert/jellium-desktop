#![cfg(target_os = "linux")]

pub mod cli;
pub mod cursor;
pub mod dmabuf_probe;
pub mod egl;
pub mod idle_inhibit;
pub mod input;
mod keysym;
pub mod menu;
pub mod open_url;
pub mod xkb;

use jfn_platform_abi::{CefPaths, WindowDecorations};

pub fn cef_paths() -> CefPaths {
    let exe = std::fs::canonicalize("/proc/self/exe").unwrap_or_default();
    let res_dir = option_env!("CEF_RESOURCES_DIR")
        .map(str::to_string)
        .unwrap_or_else(|| {
            exe.parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    CefPaths {
        browser_subprocess_path: Some(exe),
        resources_dir_path: Some(std::path::PathBuf::from(&res_dir)),
        locales_dir_path: Some(std::path::PathBuf::from(format!("{res_dir}/locales"))),
        ..Default::default()
    }
}

pub fn default_window_decorations() -> WindowDecorations {
    let kde = std::env::var("XDG_CURRENT_DESKTOP")
        .map(|v| v.split(':').any(|s| s.eq_ignore_ascii_case("KDE")))
        .unwrap_or(false);
    if kde {
        WindowDecorations::ServerThemed
    } else {
        WindowDecorations::Csd
    }
}
