//! Native-shim injection. jellyfin-web is the process's one browser; its
//! JS function list + script list ship to the renderer via the `extra_info`
//! DictionaryValue, together with the cached Jellyfin device-profile JSON.
//!
//! Built fresh per-browser-create on the C++ thread that calls
//! `CefBrowserHost::CreateBrowser`. CEF copies the dictionary into the
//! cross-process payload, so we don't hold a long-lived reference.

use cef::{
    CefString, DictionaryValue, ImplDictionaryValue, ImplListValue, dictionary_value_create,
    list_value_create,
};

use crate::cef_string::userfree_to_string;
use jfn_platform_abi::{MenuKind, MenuScript, WindowDecorations};
use std::os::raw::c_char;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeFunction {
    PlayerLoad,
    PlayerStop,
    PlayerPause,
    PlayerPlay,
    PlayerSeek,
    PlayerSetVolume,
    PlayerSetMuted,
    PlayerSetSpeed,
    PlayerSetSubtitle,
    PlayerAddSubtitle,
    PlayerSetAudio,
    PlayerAddAudio,
    PlayerSetAudioDelay,
    PlayerSetSubtitleDelay,
    PlayerSetAspectMode,
    PlayerOsdActive,
    OpenClientSettings,
    OpenConfigDir,
    SaveServerUrl,
    NotifyMetadata,
    NotifyPosition,
    NotifySeek,
    NotifyPlaybackState,
    NotifyArtwork,
    NotifyQueueChange,
    NotifyRateChange,
    AppExit,
    SetSettingValue,
    ThemeColor,
    SetOsdVisible,
    ToggleFullscreen,
}

impl NativeFunction {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "playerLoad" => Self::PlayerLoad,
            "playerStop" => Self::PlayerStop,
            "playerPause" => Self::PlayerPause,
            "playerPlay" => Self::PlayerPlay,
            "playerSeek" => Self::PlayerSeek,
            "playerSetVolume" => Self::PlayerSetVolume,
            "playerSetMuted" => Self::PlayerSetMuted,
            "playerSetSpeed" => Self::PlayerSetSpeed,
            "playerSetSubtitle" => Self::PlayerSetSubtitle,
            "playerAddSubtitle" => Self::PlayerAddSubtitle,
            "playerSetAudio" => Self::PlayerSetAudio,
            "playerAddAudio" => Self::PlayerAddAudio,
            "playerSetAudioDelay" => Self::PlayerSetAudioDelay,
            "playerSetSubtitleDelay" => Self::PlayerSetSubtitleDelay,
            "playerSetAspectMode" => Self::PlayerSetAspectMode,
            "playerOsdActive" => Self::PlayerOsdActive,
            "openClientSettings" => Self::OpenClientSettings,
            "openConfigDir" => Self::OpenConfigDir,
            "saveServerUrl" => Self::SaveServerUrl,
            "notifyMetadata" => Self::NotifyMetadata,
            "notifyPosition" => Self::NotifyPosition,
            "notifySeek" => Self::NotifySeek,
            "notifyPlaybackState" => Self::NotifyPlaybackState,
            "notifyArtwork" => Self::NotifyArtwork,
            "notifyQueueChange" => Self::NotifyQueueChange,
            "notifyRateChange" => Self::NotifyRateChange,
            "appExit" => Self::AppExit,
            "setSettingValue" => Self::SetSettingValue,
            "themeColor" => Self::ThemeColor,
            "setOsdVisible" => Self::SetOsdVisible,
            "toggleFullscreen" => Self::ToggleFullscreen,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::PlayerLoad => "playerLoad",
            Self::PlayerStop => "playerStop",
            Self::PlayerPause => "playerPause",
            Self::PlayerPlay => "playerPlay",
            Self::PlayerSeek => "playerSeek",
            Self::PlayerSetVolume => "playerSetVolume",
            Self::PlayerSetMuted => "playerSetMuted",
            Self::PlayerSetSpeed => "playerSetSpeed",
            Self::PlayerSetSubtitle => "playerSetSubtitle",
            Self::PlayerAddSubtitle => "playerAddSubtitle",
            Self::PlayerSetAudio => "playerSetAudio",
            Self::PlayerAddAudio => "playerAddAudio",
            Self::PlayerSetAudioDelay => "playerSetAudioDelay",
            Self::PlayerSetSubtitleDelay => "playerSetSubtitleDelay",
            Self::PlayerSetAspectMode => "playerSetAspectMode",
            Self::PlayerOsdActive => "playerOsdActive",
            Self::OpenClientSettings => "openClientSettings",
            Self::OpenConfigDir => "openConfigDir",
            Self::SaveServerUrl => "saveServerUrl",
            Self::NotifyMetadata => "notifyMetadata",
            Self::NotifyPosition => "notifyPosition",
            Self::NotifySeek => "notifySeek",
            Self::NotifyPlaybackState => "notifyPlaybackState",
            Self::NotifyArtwork => "notifyArtwork",
            Self::NotifyQueueChange => "notifyQueueChange",
            Self::NotifyRateChange => "notifyRateChange",
            Self::AppExit => "appExit",
            Self::SetSettingValue => "setSettingValue",
            Self::ThemeColor => "themeColor",
            Self::SetOsdVisible => "setOsdVisible",
            Self::ToggleFullscreen => "toggleFullscreen",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InjectedScript {
    NativeShim,
    MpvPlayerBase,
    MpvVideoPlayer,
    MpvAudioPlayer,
    InputPlugin,
    SelectMenu,
}

impl InjectedScript {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "native-shim.js" => Self::NativeShim,
            "mpv-player-base.js" => Self::MpvPlayerBase,
            "mpv-video-player.js" => Self::MpvVideoPlayer,
            "mpv-audio-player.js" => Self::MpvAudioPlayer,
            "input-plugin.js" => Self::InputPlugin,
            "select-menu.js" => Self::SelectMenu,
            _ => return None,
        })
    }

    pub(crate) fn file_name(self) -> &'static str {
        match self {
            Self::NativeShim => "native-shim.js",
            Self::MpvPlayerBase => "mpv-player-base.js",
            Self::MpvVideoPlayer => "mpv-video-player.js",
            Self::MpvAudioPlayer => "mpv-audio-player.js",
            Self::InputPlugin => "input-plugin.js",
            Self::SelectMenu => "select-menu.js",
        }
    }

    fn from_menu(script: MenuScript) -> InjectedScript {
        match script {
            MenuScript::SelectMenu => Self::SelectMenu,
        }
    }
}

const WEB_FUNCTIONS: &[NativeFunction] = &[
    NativeFunction::PlayerLoad,
    NativeFunction::PlayerStop,
    NativeFunction::PlayerPause,
    NativeFunction::PlayerPlay,
    NativeFunction::PlayerSeek,
    NativeFunction::PlayerSetVolume,
    NativeFunction::PlayerSetMuted,
    NativeFunction::PlayerSetSpeed,
    NativeFunction::PlayerSetSubtitle,
    NativeFunction::PlayerAddSubtitle,
    NativeFunction::PlayerSetAudio,
    NativeFunction::PlayerAddAudio,
    NativeFunction::PlayerSetAudioDelay,
    NativeFunction::PlayerSetSubtitleDelay,
    NativeFunction::PlayerSetAspectMode,
    NativeFunction::PlayerOsdActive,
    NativeFunction::OpenClientSettings,
    NativeFunction::OpenConfigDir,
    NativeFunction::SaveServerUrl,
    NativeFunction::NotifyMetadata,
    NativeFunction::NotifyPosition,
    NativeFunction::NotifySeek,
    NativeFunction::NotifyPlaybackState,
    NativeFunction::NotifyArtwork,
    NativeFunction::NotifyQueueChange,
    NativeFunction::NotifyRateChange,
    NativeFunction::AppExit,
    NativeFunction::SetSettingValue,
    NativeFunction::ThemeColor,
    NativeFunction::SetOsdVisible,
    NativeFunction::ToggleFullscreen,
];

const WEB_SCRIPTS: &[InjectedScript] = &[
    InjectedScript::NativeShim,
    InjectedScript::MpvPlayerBase,
    InjectedScript::MpvVideoPlayer,
    InjectedScript::MpvAudioPlayer,
    InjectedScript::InputPlugin,
];
const FUNCTIONS_KEY: &str = "functions";
const SCRIPTS_KEY: &str = "scripts";
const DEVICE_PROFILE_JSON_KEY: &str = "device_profile_json";
const SHARED_TEXTURES_ENABLED_KEY: &str = "shared_textures_enabled";
const WINDOW_DECORATIONS_KEY: &str = "window_decorations";
const WINDOW_DECORATION_OPTIONS_KEY: &str = "window_decoration_options";

static DEVICE_PROFILE_JSON: OnceLock<String> = OnceLock::new();

#[derive(Clone, Debug)]
pub(crate) struct ExtraInfo {
    functions: Vec<NativeFunction>,
    scripts: Vec<InjectedScript>,
    device_profile_json: Option<String>,
    shared_textures_enabled: bool,
    window_decorations: Option<WindowDecorations>,
    /// Decoration modes the user may choose between; empty when the setting
    /// does not apply (non-Linux).
    window_decoration_options: Vec<WindowDecorations>,
}

impl ExtraInfo {
    pub(crate) fn from_dictionary(dict: DictionaryValue) -> Self {
        Self {
            functions: read_native_functions(&dict),
            scripts: read_injected_scripts(&dict),
            device_profile_json: read_string(&dict, DEVICE_PROFILE_JSON_KEY),
            shared_textures_enabled: read_bool(&dict, SHARED_TEXTURES_ENABLED_KEY),
            window_decorations: read_string(&dict, WINDOW_DECORATIONS_KEY)
                .as_deref()
                .and_then(WindowDecorations::parse),
            window_decoration_options: read_typed_list(
                &dict,
                WINDOW_DECORATION_OPTIONS_KEY,
                WindowDecorations::parse,
            ),
        }
    }

    pub(crate) fn into_dictionary(self) -> Option<DictionaryValue> {
        let dict = dictionary_value_create()?;
        write_native_functions(&dict, &self.functions)?;
        write_injected_scripts(&dict, &self.scripts)?;
        dict.set_bool(
            Some(&CefString::from(SHARED_TEXTURES_ENABLED_KEY)),
            if self.shared_textures_enabled { 1 } else { 0 },
        );
        write_string_list(
            &dict,
            WINDOW_DECORATION_OPTIONS_KEY,
            self.window_decoration_options.iter().map(|wd| wd.as_str()),
        )?;
        if let Some(json) = self.device_profile_json {
            dict.set_string(
                Some(&CefString::from(DEVICE_PROFILE_JSON_KEY)),
                Some(&CefString::from(json.as_str())),
            );
        }
        if let Some(wd) = self.window_decorations {
            dict.set_string(
                Some(&CefString::from(WINDOW_DECORATIONS_KEY)),
                Some(&CefString::from(wd.as_str())),
            );
        }
        Some(dict)
    }

    pub(crate) fn functions(&self) -> &[NativeFunction] {
        &self.functions
    }

    pub(crate) fn scripts(&self) -> &[InjectedScript] {
        &self.scripts
    }

    pub(crate) fn device_profile_json(&self) -> Option<&str> {
        self.device_profile_json.as_deref()
    }

    pub(crate) fn shared_textures_enabled(&self) -> bool {
        self.shared_textures_enabled
    }

    pub(crate) fn window_decorations(&self) -> Option<&'static str> {
        self.window_decorations.map(WindowDecorations::as_str)
    }

    pub(crate) fn window_decoration_options(&self) -> &[WindowDecorations] {
        &self.window_decoration_options
    }
}

fn read_native_functions(dict: &DictionaryValue) -> Vec<NativeFunction> {
    read_typed_list(dict, FUNCTIONS_KEY, NativeFunction::from_name)
}

fn read_injected_scripts(dict: &DictionaryValue) -> Vec<InjectedScript> {
    read_typed_list(dict, SCRIPTS_KEY, InjectedScript::from_name)
}

fn read_typed_list<T>(
    dict: &DictionaryValue,
    key: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Vec<T> {
    let Some(list) = dict.list(Some(&CefString::from(key))) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..list.size() {
        let value = userfree_to_string(&list.string(i));
        if let Some(value) = parse(&value) {
            out.push(value);
        }
    }
    out
}

fn read_string(dict: &DictionaryValue, key: &str) -> Option<String> {
    let key = CefString::from(key);
    if dict.has_key(Some(&key)) == 1 {
        Some(userfree_to_string(&dict.string(Some(&key))))
    } else {
        None
    }
}

fn read_bool(dict: &DictionaryValue, key: &str) -> bool {
    let key = CefString::from(key);
    dict.has_key(Some(&key)) == 1 && dict.bool(Some(&key)) == 1
}

fn write_native_functions(dict: &DictionaryValue, functions: &[NativeFunction]) -> Option<()> {
    write_string_list(dict, FUNCTIONS_KEY, functions.iter().map(|f| f.name()))
}

fn write_injected_scripts(dict: &DictionaryValue, scripts: &[InjectedScript]) -> Option<()> {
    write_string_list(dict, SCRIPTS_KEY, scripts.iter().map(|s| s.file_name()))
}

fn write_string_list<'a>(
    dict: &DictionaryValue,
    key: &str,
    values: impl IntoIterator<Item = &'a str>,
) -> Option<()> {
    let mut list = list_value_create()?;
    for (idx, value) in values.into_iter().enumerate() {
        list.set_string(idx, Some(&CefString::from(value)));
    }
    dict.set_list(Some(&CefString::from(key)), Some(&mut list));
    Some(())
}

/// Set the cached Jellyfin device-profile JSON. Called once at startup
/// after mpv capabilities are queried. Returns silently if already set.
///
/// # Safety
/// `json_utf8` must reference `len` valid UTF-8 bytes, or be null.
pub unsafe fn jfn_cef_set_device_profile_json(json_utf8: *const c_char, len: usize) {
    if json_utf8.is_null() || len == 0 {
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts(json_utf8 as *const u8, len) };
    let s = match std::str::from_utf8(slice) {
        Ok(s) => s.to_string(),
        Err(_) => return,
    };
    let _ = DEVICE_PROFILE_JSON.set(s);
}

pub(crate) fn build_web(shared_textures_enabled: bool) -> ExtraInfo {
    let mut extra_info = ExtraInfo {
        functions: WEB_FUNCTIONS.to_vec(),
        scripts: WEB_SCRIPTS.to_vec(),
        device_profile_json: None,
        shared_textures_enabled,
        window_decorations: jfn_config::configured_window_decorations(),
        window_decoration_options: Vec::new(),
    };
    if let Some(json) = DEVICE_PROFILE_JSON.get()
        && !json.is_empty()
    {
        extra_info.device_profile_json = Some(json.clone());
    }
    if let Some(p) = jfn_platform_abi::try_get()
        && p.window_decorations_supported()
    {
        extra_info.window_decoration_options = p.window_decoration_options().iter().collect();
    }
    extra_info.scripts.extend(
        jfn_platform_abi::menu_scripts(MenuKind::Dropdown)
            .iter()
            .copied()
            .map(InjectedScript::from_menu),
    );
    extra_info
}

#[cfg(test)]
mod tests {
    use super::{NativeFunction, WEB_FUNCTIONS};

    #[test]
    fn web_profile_has_native_settings_opener() {
        assert!(WEB_FUNCTIONS.contains(&NativeFunction::OpenClientSettings));
    }

    #[test]
    fn native_shell_calls_opener_and_retains_capabilities() {
        let shim = include_str!("../../web/native-shim.js");
        assert!(shim.contains("window.jmpNative.openClientSettings();"));
        assert!(shim.contains("'exitmenu', 'clientsettings'"));
    }
}
