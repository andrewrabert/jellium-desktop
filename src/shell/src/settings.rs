//! Native client settings modal.

use iced_core::widget::Id;
use iced_core::{Element, Length, Padding};
use iced_widget::{button, checkbox, column, scrollable, text};
use jfn_platform_abi::{DisplayBackend, WindowDecorations};

use crate::controls;
use crate::theme::{self, Theme};

pub const SETTINGS_SCROLL: Id = Id::new("shell-settings-scroll");
pub const CLOSE_CONTROL: Id = Id::new("shell-settings-close");
pub const HARDWARE_DECODING_CONTROL: Id = Id::new("shell-settings-hardware-decoding");
pub const AUDIO_PASSTHROUGH_FIELD: Id = Id::new("shell-settings-audio-passthrough");
pub const EXCLUSIVE_AUDIO_CONTROL: Id = Id::new("shell-settings-exclusive-audio");
pub const CHANNEL_LAYOUT_CONTROL: Id = Id::new("shell-settings-channel-layout");
pub const FORCE_TRANSCODE_CONTROL: Id = Id::new("shell-settings-force-transcode");
pub const WINDOW_DECORATION_CONTROL: Id = Id::new("shell-settings-window-decoration");
pub const TRANSPARENT_TITLEBAR_CONTROL: Id = Id::new("shell-settings-transparent-titlebar");
pub const HIDE_SCROLLBAR_CONTROL: Id = Id::new("shell-settings-hide-scrollbar");
pub const DEVICE_NAME_FIELD: Id = Id::new("shell-settings-device-name");
pub const LOG_LEVEL_CONTROL: Id = Id::new("shell-settings-log-level");
pub const OPEN_MPV_CONFIG_CONTROL: Id = Id::new("shell-settings-open-mpv-config");
pub const RESET_SERVER_CONTROL: Id = Id::new("shell-settings-reset-server");

pub const TITLE: &str = "Settings";
pub const CLOSE_LABEL: &str = "Close Settings";

pub const SECTION_TITLES: [&str; 6] = [
    "Playback",
    "Audio",
    "Transcode",
    "Advanced",
    "MPV config",
    "Server",
];

#[derive(Clone, Debug)]
pub enum Message {
    HardwareDecodingChanged(String),
    AudioPassthroughEdited(String),
    CommitAudioPassthrough,
    ExclusiveAudioOutputChanged(bool),
    AudioChannelLayoutChanged(String),
    ForceTranscodingChanged(bool),
    WindowDecorationsChanged(Option<WindowDecorations>),
    TransparentTitlebarChanged(bool),
    HideScrollbarChanged(bool),
    DeviceNameEdited(String),
    CommitDeviceName,
    LogLevelChanged(String),
    OpenMpvConfigDirectory,
    ResetSavedServer,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    None,
    Dismiss,
    ResetSavedServer,
}

pub struct Settings {
    pub audio_passthrough: String,
    pub device_name: String,
    pub device_name_default: String,
    server_available: bool,
    decoration_options: Vec<Option<WindowDecorations>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

impl Settings {
    pub fn new() -> Self {
        let platform = jfn_platform_abi::get();
        let decoration_options = decoration_options(
            platform.window_decorations_supported(),
            platform.window_decoration_options().iter(),
        );
        Self {
            audio_passthrough: jfn_config::audio_passthrough(),
            device_name: jfn_config::device_name(),
            device_name_default: jfn_config::default_device_name(),
            server_available: !jfn_config::server_url().is_empty(),
            decoration_options,
        }
    }

    pub fn view(&self) -> Element<'_, Message, Theme, iced_wgpu::Renderer> {
        let hwdec_selected = Self::selected_hwdec(jfn_config::hwdec());
        let audio_channels = jfn_config::audio_channels();
        let log_level = jfn_config::log_level();

        let mut controls = column![
            text("Changes take effect after restarting the application.").class(Some(theme::MUTED)),
            self.group(SECTION_TITLES[0], column![self.selection(
                "Hardware Decoding",
                controls::select(
                    HARDWARE_DECODING_CONTROL,
                    hwdec_selected,
                    Self::hardware_decoding_choices(),
                    Clone::clone,
                    Message::HardwareDecodingChanged,
                ),
                "Hardware video decoding mode. Use \"auto\" for automatic detection or \"no\" to disable.",
            )]),
            self.group(SECTION_TITLES[1], column![
                self.setting(
                    "Audio Passthrough",
                    crate::field::field(AUDIO_PASSTHROUGH_FIELD, "", &self.audio_passthrough)
                        .on_input(Message::AudioPassthroughEdited)
                        .on_submit(Message::CommitAudioPassthrough)
                        .on_unfocus(Message::CommitAudioPassthrough)
                        .padding(Padding::from([8, 10]))
                        .into(),
                    "Comma-separated list of codecs to pass through to the audio device (e.g. ac3,eac3,dts-hd,truehd). Leave empty to disable.",
                ),
                self.toggle(
                    EXCLUSIVE_AUDIO_CONTROL,
                    "Exclusive Audio Output",
                    jfn_config::audio_exclusive(),
                    Message::ExclusiveAudioOutputChanged,
                    "Take exclusive control of the audio device during playback. May reduce latency but prevents other apps from playing audio.",
                ),
                self.selection(
                    "Audio Channel Layout",
                    controls::select(
                        CHANNEL_LAYOUT_CONTROL,
                        audio_channels,
                        ["", "stereo", "5.1", "7.1"].map(str::to_owned).to_vec(),
                        |value: &String| audio_channel_label(value),
                        Message::AudioChannelLayoutChanged,
                    ),
                    "Force a specific channel layout. Leave empty for auto-detection.",
                ),
            ]),
            self.group(SECTION_TITLES[2], column![self.toggle(
                FORCE_TRANSCODE_CONTROL,
                "Force Transcoding",
                jfn_config::force_transcoding(),
                Message::ForceTranscodingChanged,
                "Always request a transcoded stream from the server, even when direct play would work.",
            )]),
        ]
        .spacing(22);

        let mut advanced = column![].spacing(16);
        if !self.decoration_options.is_empty() {
            advanced = advanced.push(self.selection(
                "Window Decorations",
                controls::select(
                    WINDOW_DECORATION_CONTROL,
                    jfn_config::configured_window_decorations(),
                    self.decoration_options.clone(),
                    decoration_label,
                    Message::WindowDecorationsChanged,
                ),
                "How the window titlebar is drawn. Changing requires restart.",
            ));
        }
        if transparent_titlebar_visible(jfn_platform_abi::get().display()) {
            advanced = advanced.push(self.toggle(
                TRANSPARENT_TITLEBAR_CONTROL,
                "Transparent Titlebar",
                jfn_config::transparent_titlebar(),
                Message::TransparentTitlebarChanged,
                "Overlay traffic light buttons on the window content instead of a separate titlebar. Requires restart.",
            ));
        }
        advanced = advanced
            .push(self.toggle(
                HIDE_SCROLLBAR_CONTROL,
                "Hide Scrollbar",
                jfn_config::hide_scrollbar(),
                Message::HideScrollbarChanged,
                "Hide scrollbars throughout the app. Scrolling with the wheel, trackpad, and keyboard still works. Requires restart.",
            ))
            .push(self.setting(
                "Device Name",
                crate::field::field(DEVICE_NAME_FIELD, &self.device_name_default, &self.device_name)
                    .on_input(Message::DeviceNameEdited)
                    .on_submit(Message::CommitDeviceName)
                    .on_unfocus(Message::CommitDeviceName)
                    .padding(Padding::from([8, 10]))
                    .into(),
                "Identifies this machine to the server. Leave blank to use the system hostname.",
            ))
            .push(self.selection(
                "Log Level",
                controls::select(
                    LOG_LEVEL_CONTROL,
                    log_level,
                    ["", "verbose", "debug", "warn", "error"]
                        .map(str::to_owned)
                        .to_vec(),
                    |value: &String| log_level_label(value),
                    Message::LogLevelChanged,
                ),
                "Set the application log verbosity level.",
            ));
        controls = controls.push(self.group(SECTION_TITLES[3], advanced));
        if self.server_available {
            controls = controls
                .push(self.group(
                    SECTION_TITLES[4],
                    column![self.action(
                        OPEN_MPV_CONFIG_CONTROL,
                        "Open mpv config directory",
                        Message::OpenMpvConfigDirectory,
                    )],
                ))
                .push(self.group(
                    SECTION_TITLES[5],
                    column![self.action(
                        RESET_SERVER_CONTROL,
                        "Reset Saved Server",
                        Message::ResetSavedServer,
                    )],
                ));
        }

        scrollable(controls)
            .id(SETTINGS_SCROLL)
            .height(Length::Fill)
            .into()
    }

    /// The mode the Hardware Decoding control shows: the stored value itself,
    /// so an unset setting shows the mode mpv is given, not a guess.
    fn selected_hwdec(hwdec: jfn_config::Hwdec) -> String {
        hwdec.as_str().to_owned()
    }

    pub fn hardware_decoding_choices() -> Vec<String> {
        jfn_config::hwdec_options()
            .iter()
            .map(|value| (*value).to_owned())
            .collect()
    }

    pub fn commit_text(&mut self, message: Message) {
        match message {
            Message::CommitAudioPassthrough => {
                jfn_config::set_audio_passthrough(&self.audio_passthrough);
                jfn_config::settings_save_async();
            }
            Message::CommitDeviceName => {
                jfn_config::set_device_name(&self.device_name, &self.device_name_default);
                jfn_config::settings_save_async();
            }
            _ => {}
        }
    }

    /// Commits both text drafts before the containing overlay is dismissed.
    pub fn dismiss(&mut self) -> Outcome {
        self.commit_text(Message::CommitAudioPassthrough);
        self.commit_text(Message::CommitDeviceName);
        Outcome::Dismiss
    }

    pub fn update(&mut self, message: Message) -> Outcome {
        match message {
            Message::HardwareDecodingChanged(value) => {
                if let Ok(hwdec) = value.parse() {
                    jfn_config::set_hwdec(hwdec);
                }
            }
            Message::AudioPassthroughEdited(value) => {
                self.audio_passthrough = value;
                return Outcome::None;
            }
            Message::CommitAudioPassthrough => {
                self.commit_text(Message::CommitAudioPassthrough);
                return Outcome::None;
            }
            Message::ExclusiveAudioOutputChanged(value) => jfn_config::set_audio_exclusive(value),
            Message::AudioChannelLayoutChanged(value) => jfn_config::set_audio_channels(&value),
            Message::ForceTranscodingChanged(value) => jfn_config::set_force_transcoding(value),
            Message::WindowDecorationsChanged(value) => {
                jfn_config::set_window_decorations(value.map(WindowDecorations::as_str));
            }
            Message::TransparentTitlebarChanged(value) => {
                jfn_config::set_transparent_titlebar(value);
            }
            Message::HideScrollbarChanged(value) => jfn_config::set_hide_scrollbar(value),
            Message::DeviceNameEdited(value) => {
                self.device_name = value;
                return Outcome::None;
            }
            Message::CommitDeviceName => {
                self.commit_text(Message::CommitDeviceName);
                return Outcome::None;
            }
            Message::LogLevelChanged(value) => jfn_config::set_log_level(&value),
            Message::OpenMpvConfigDirectory => {
                jfn_platform_abi::get().open_path(&jfn_paths::mpv_home());
                return Outcome::None;
            }
            Message::ResetSavedServer => {
                self.commit_text(Message::CommitAudioPassthrough);
                self.commit_text(Message::CommitDeviceName);
                jfn_config::set_server_url("");
                jfn_config::settings_save_async();
                return Outcome::ResetSavedServer;
            }
        }
        jfn_config::settings_save_async();
        Outcome::None
    }

    pub fn focus_target(&self) -> Option<Id> {
        Some(AUDIO_PASSTHROUGH_FIELD)
    }

    #[cfg(test)]
    pub(crate) fn testing() -> Self {
        Self {
            audio_passthrough: String::new(),
            device_name: String::new(),
            device_name_default: "host".to_owned(),
            server_available: false,
            decoration_options: Vec::new(),
        }
    }

    fn group<'a>(
        &self,
        title: &'a str,
        controls: iced_widget::Column<'a, Message, Theme, iced_wgpu::Renderer>,
    ) -> Element<'a, Message, Theme, iced_wgpu::Renderer> {
        column![text(title).size(20), controls.spacing(16)]
            .spacing(10)
            .into()
    }

    fn setting<'a>(
        &self,
        label: &'a str,
        control: Element<'a, Message, Theme, iced_wgpu::Renderer>,
        help: &'a str,
    ) -> Element<'a, Message, Theme, iced_wgpu::Renderer> {
        column![
            text(label),
            control,
            text(help).size(13).class(Some(theme::MUTED))
        ]
        .spacing(5)
        .into()
    }

    fn selection<'a>(
        &self,
        label: &'a str,
        control: Element<'a, Message, Theme, iced_wgpu::Renderer>,
        help: &'a str,
    ) -> Element<'a, Message, Theme, iced_wgpu::Renderer> {
        self.setting(label, control, help)
    }

    fn toggle<'a>(
        &self,
        id: Id,
        label: &'a str,
        value: bool,
        changed: fn(bool) -> Message,
        help: &'a str,
    ) -> Element<'a, Message, Theme, iced_wgpu::Renderer> {
        column![
            controls::action(
                id,
                checkbox(value).label(label).on_toggle(changed),
                changed(!value),
            ),
            text(help).size(13).class(Some(theme::MUTED)),
        ]
        .spacing(5)
        .into()
    }

    fn action<'a>(
        &self,
        id: Id,
        label: &'a str,
        message: Message,
    ) -> Element<'a, Message, Theme, iced_wgpu::Renderer> {
        controls::action(id, button(text(label)).on_press(message.clone()), message)
    }
}

fn decoration_options(
    supported: bool,
    options: impl Iterator<Item = WindowDecorations>,
) -> Vec<Option<WindowDecorations>> {
    let options: Vec<_> = options.collect();
    if !supported || options.len() <= 1 {
        return Vec::new();
    }
    std::iter::once(None)
        .chain(options.into_iter().map(Some))
        .collect()
}

fn transparent_titlebar_visible(display: DisplayBackend) -> bool {
    display == DisplayBackend::MacOS
}

#[cfg(test)]
fn control_order(display: DisplayBackend, decorations: bool, server: bool) -> Vec<Id> {
    let mut ids = vec![
        HARDWARE_DECODING_CONTROL,
        AUDIO_PASSTHROUGH_FIELD,
        EXCLUSIVE_AUDIO_CONTROL,
        CHANNEL_LAYOUT_CONTROL,
        FORCE_TRANSCODE_CONTROL,
    ];
    if decorations {
        ids.push(WINDOW_DECORATION_CONTROL);
    }
    if transparent_titlebar_visible(display) {
        ids.push(TRANSPARENT_TITLEBAR_CONTROL);
    }
    ids.extend([HIDE_SCROLLBAR_CONTROL, DEVICE_NAME_FIELD, LOG_LEVEL_CONTROL]);
    if server {
        ids.extend([OPEN_MPV_CONFIG_CONTROL, RESET_SERVER_CONTROL]);
    }
    ids
}

fn audio_channel_label(value: &str) -> String {
    match value {
        "" => "Auto",
        "stereo" => "Stereo",
        "5.1" => "5.1 Surround",
        "7.1" => "7.1 Surround",
        value => value,
    }
    .to_owned()
}

fn log_level_label(value: &str) -> String {
    match value {
        "" => "Default (Info)",
        "verbose" => "Verbose",
        "debug" => "Debug",
        "warn" => "Warning",
        "error" => "Error",
        value => value,
    }
    .to_owned()
}

fn decoration_label(value: &Option<WindowDecorations>) -> String {
    match value {
        None => "Auto",
        Some(WindowDecorations::Csd) => "In-app (client-side)",
        Some(WindowDecorations::Server) => "System (server-side)",
        Some(WindowDecorations::ServerThemed) => "System, themed (KDE)",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::{DisplayBackend, WindowDecorations};

    #[test]
    fn unset_hwdec_shows_the_mode_mpv_is_given() {
        assert_eq!(
            Settings::selected_hwdec(jfn_config::Hwdec::default()),
            jfn_config::HWDEC_DEFAULT
        );
    }

    #[test]
    fn hardware_choices_are_the_mpv_authority() {
        assert_eq!(
            Settings::hardware_decoding_choices(),
            jfn_config::hwdec_options()
        );
    }

    #[test]
    fn decoration_choices_follow_platform_order_after_auto() {
        let choices = decoration_options(
            true,
            [
                WindowDecorations::Csd,
                WindowDecorations::Server,
                WindowDecorations::ServerThemed,
            ]
            .into_iter(),
        );
        assert_eq!(
            choices,
            [
                None,
                Some(WindowDecorations::Csd),
                Some(WindowDecorations::Server),
                Some(WindowDecorations::ServerThemed),
            ]
        );
        assert_eq!(
            choices.iter().map(decoration_label).collect::<Vec<_>>(),
            [
                "Auto",
                "In-app (client-side)",
                "System (server-side)",
                "System, themed (KDE)",
            ]
        );
    }

    #[test]
    fn unsupported_and_csd_only_omit_decorations() {
        assert!(
            decoration_options(
                false,
                [WindowDecorations::Csd, WindowDecorations::Server].into_iter()
            )
            .is_empty()
        );
        assert!(decoration_options(true, [WindowDecorations::Csd].into_iter()).is_empty());
    }

    #[test]
    fn transparent_titlebar_is_macos_only() {
        assert!(transparent_titlebar_visible(DisplayBackend::MacOS));
        assert!(!transparent_titlebar_visible(DisplayBackend::Wayland));
        assert!(!transparent_titlebar_visible(DisplayBackend::X11));
        assert!(!transparent_titlebar_visible(DisplayBackend::Windows));
    }

    #[test]
    fn labels_and_empty_audio_focus_target_are_stable() {
        assert_eq!(audio_channel_label(""), "Auto");
        assert_eq!(audio_channel_label("5.1"), "5.1 Surround");
        assert_eq!(log_level_label(""), "Default (Info)");
        let settings = Settings::testing();
        assert!(settings.audio_passthrough.is_empty());
        assert_eq!(settings.focus_target(), Some(AUDIO_PASSTHROUGH_FIELD));
    }

    #[test]
    fn section_headings_are_the_six_required_peers_in_order() {
        assert_eq!(
            SECTION_TITLES,
            [
                "Playback",
                "Audio",
                "Transcode",
                "Advanced",
                "MPV config",
                "Server",
            ]
        );
    }

    #[test]
    fn control_order_follows_platform_and_server_visibility() {
        assert_eq!(
            control_order(DisplayBackend::Wayland, true, true),
            [
                HARDWARE_DECODING_CONTROL,
                AUDIO_PASSTHROUGH_FIELD,
                EXCLUSIVE_AUDIO_CONTROL,
                CHANNEL_LAYOUT_CONTROL,
                FORCE_TRANSCODE_CONTROL,
                WINDOW_DECORATION_CONTROL,
                HIDE_SCROLLBAR_CONTROL,
                DEVICE_NAME_FIELD,
                LOG_LEVEL_CONTROL,
                OPEN_MPV_CONFIG_CONTROL,
                RESET_SERVER_CONTROL,
            ]
        );
        assert_eq!(
            control_order(DisplayBackend::MacOS, false, false),
            [
                HARDWARE_DECODING_CONTROL,
                AUDIO_PASSTHROUGH_FIELD,
                EXCLUSIVE_AUDIO_CONTROL,
                CHANNEL_LAYOUT_CONTROL,
                FORCE_TRANSCODE_CONTROL,
                TRANSPARENT_TITLEBAR_CONTROL,
                HIDE_SCROLLBAR_CONTROL,
                DEVICE_NAME_FIELD,
                LOG_LEVEL_CONTROL,
            ]
        );
    }

    #[test]
    fn committing_device_name_preserves_the_whitespace_padded_draft() {
        let mut settings = Settings::testing();
        settings.device_name = "  living   room  ".to_owned();

        settings.commit_text(Message::CommitDeviceName);

        assert_eq!(settings.device_name, "  living   room  ");
    }

    #[test]
    fn display_copy_and_explicit_dismissal_are_stable() {
        assert_eq!(TITLE, "Settings");
        assert_eq!(CLOSE_LABEL, "Close Settings");
        assert_eq!(Settings::testing().dismiss(), Outcome::Dismiss);
    }
}
