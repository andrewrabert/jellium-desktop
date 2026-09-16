use std::ffi::CString;

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
