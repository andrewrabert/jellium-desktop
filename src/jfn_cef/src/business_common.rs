//! Shared helpers for the three `business_*` modules.
//!
//! Two groups, separated by the dividers below:
//!   1. Generic CEF/Rust helpers — could lift into a `cef-rs-helpers` crate.
//!   2. App-specific dispatch — Jellium Desktop config wiring.

use std::ffi::CString;

// --- generic Rust/C interop ------------------------------------------------

/// Convert a JS-supplied string into a `CString` for FFI, logging + dropping
/// on interior NUL. `label` names the IPC arm in the warn message so the
/// log line is enough to locate the offending handler.
///
/// Avoids the prior `CString::new(x).unwrap_or_default()` pattern that
/// silently handed `""` to downstream consumers (e.g. mpv).
pub(crate) fn js_cstr_or_warn(label: &str, s: &str) -> Option<CString> {
    match CString::new(s) {
        Ok(c) => Some(c),
        Err(_) => {
            jfn_logging::log(
                jfn_logging::Category::Cef,
                jfn_logging::Level::Warn,
                &format!("{label}: interior NUL in JS string; dropping IPC"),
            );
            None
        }
    }
}

// --- app-specific dispatch -------------------------------------------------

/// `setSettingValue` IPC dispatch. Superset of the keys the overlay and the
/// main web UI send today — both UIs share this single source of truth so
/// new keys land in one place.
pub(crate) fn apply_setting_value(_section: &str, key: &str, value: Option<&str>) {
    if key == "windowDecorations" {
        jfn_config::set_window_decorations(value);
        jfn_config::settings_save_async();
        return;
    }
    let Some(value) = value else {
        jfn_logging::log(
            jfn_logging::Category::Cef,
            jfn_logging::Level::Warn,
            &format!("Null value for setting key: {_section}.{key}"),
        );
        return;
    };
    match key {
        "hwdec" => match value.parse() {
            Ok(hwdec) => jfn_config::set_hwdec(hwdec),
            Err(e) => jfn_logging::log(
                jfn_logging::Category::Cef,
                jfn_logging::Level::Warn,
                &format!("Ignoring setting {_section}.{key}: {e}"),
            ),
        },
        "audioPassthrough" => jfn_config::set_audio_passthrough(value),
        "audioExclusive" => jfn_config::set_audio_exclusive(value == "true"),
        "audioChannels" => jfn_config::set_audio_channels(value),
        "transparentTitlebar" => jfn_config::set_transparent_titlebar(value == "true"),
        "hideScrollbar" => jfn_config::set_hide_scrollbar(value == "true"),
        "logLevel" => jfn_config::set_log_level(value),
        "forceTranscoding" => jfn_config::set_force_transcoding(value == "true"),
        "deviceName" => {
            jfn_config::set_device_name(value, &jfn_config::default_device_name());
        }
        _ => jfn_logging::log(
            jfn_logging::Category::Cef,
            jfn_logging::Level::Warn,
            &format!("Unknown setting key: {_section}.{key}"),
        ),
    }
    jfn_config::settings_save_async();
}
