//! Settings store. Owns the in-memory state, JSON persistence, and the
//! singleton accessor that the rest of the workspace calls into.
//!
//! On-disk schema is [`SettingsFile`]. Missing, unknown, and malformed keys
//! keep their defaults on load; save suppresses fields that are at their
//! default (empty strings, sentinel values, zero geometry) so existing config
//! files round-trip unchanged.

use jfn_mailbox::Mailbox;
use jfn_platform_abi::WindowDecorations;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};

mod hardware_decode;
pub use hardware_decode::{HWDEC_DEFAULT, Hwdec, UnknownHwdec, hwdec_options};

const DEVICE_NAME_MAX: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct JfnWindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub logical_width: i32,
    pub logical_height: i32,
    pub maximized: bool,
}

impl Default for JfnWindowGeometry {
    fn default() -> Self {
        Self {
            x: -1,
            y: -1,
            width: 0,
            height: 0,
            logical_width: 0,
            logical_height: 0,
            maximized: false,
        }
    }
}

#[derive(Clone, Debug)]
struct SettingsData {
    server_url: String,
    hwdec: Hwdec,
    audio_passthrough: String,
    audio_channels: String,
    log_level: String,
    device_name: String,
    window: JfnWindowGeometry,
    audio_exclusive: bool,
    disable_gpu_compositing: bool,
    transparent_titlebar: bool,
    force_transcoding: bool,
    window_decorations: Option<WindowDecorations>,
    hide_scrollbar: bool,
}

impl Default for SettingsData {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            hwdec: Hwdec::default(),
            audio_passthrough: String::new(),
            audio_channels: String::new(),
            log_level: String::new(),
            device_name: String::new(),
            window: JfnWindowGeometry::default(),
            audio_exclusive: false,
            disable_gpu_compositing: false,
            transparent_titlebar: true,
            force_transcoding: false,
            window_decorations: None,
            hide_scrollbar: true,
        }
    }
}

/// The settings.json document. Every key is optional on load; a key at its
/// default is absent on save. Field order is the on-disk key order.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
struct SettingsFile {
    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    server_url: Option<String>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    window_width: Option<i32>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    window_height: Option<i32>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    window_logical_width: Option<i32>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    window_logical_height: Option<i32>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    window_x: Option<i32>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    window_y: Option<i32>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    window_maximized: Option<bool>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    hwdec: Option<Hwdec>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    audio_passthrough: Option<String>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    audio_exclusive: Option<bool>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    audio_channels: Option<String>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    disable_gpu_compositing: Option<bool>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    transparent_titlebar: Option<bool>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    log_level: Option<String>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    force_transcoding: Option<bool>,

    #[serde(
        deserialize_with = "lenient_decorations",
        serialize_with = "serialize_decorations",
        skip_serializing_if = "Option::is_none"
    )]
    window_decorations: Option<WindowDecorations>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    hide_scrollbar: Option<bool>,

    #[serde(deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    device_name: Option<String>,
}

/// Reads any JSON value and yields `None` unless it deserializes as `T`, so a
/// key of the wrong type is ignored instead of failing the whole load.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Value::deserialize(deserializer)?;
    Ok(T::deserialize(value).ok())
}

/// Decoration names outside the wire contract are ignored like any other
/// malformed key.
fn lenient_decorations<'de, D>(deserializer: D) -> Result<Option<WindowDecorations>, D::Error>
where
    D: Deserializer<'de>,
{
    let name: Option<String> = lenient(deserializer)?;
    Ok(name.as_deref().and_then(WindowDecorations::parse))
}

/// Emits the wire literal from `WindowDecorations::as_str`.
fn serialize_decorations<S>(
    value: &Option<WindowDecorations>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(d) => serializer.serialize_str(d.as_str()),
        None => serializer.serialize_none(),
    }
}

/// The settings blob the web UI parses. `windowDecorations` is absent:
/// resolving its effective value needs the Platform default, unavailable in
/// the CEF renderer where this is built.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CliSettings<'a> {
    hwdec: Hwdec,

    #[serde(skip_serializing_if = "Option::is_none")]
    audio_passthrough: Option<&'a str>,

    #[serde(skip_serializing_if = "Option::is_none")]
    audio_exclusive: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    audio_channels: Option<&'a str>,

    #[serde(skip_serializing_if = "Option::is_none")]
    disable_gpu_compositing: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    transparent_titlebar: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    log_level: Option<&'a str>,

    force_transcoding: bool,

    hide_scrollbar: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    device_name: Option<&'a str>,

    device_name_default: String,

    hwdec_options: &'static [&'static str],
}

impl SettingsData {
    fn overlay(&mut self, file: SettingsFile) {
        if let Some(v) = file.server_url {
            self.server_url = v;
        }
        if let Some(v) = file.hwdec {
            self.hwdec = v;
        }
        if let Some(v) = file.audio_passthrough {
            self.audio_passthrough = v;
        }
        if let Some(v) = file.audio_channels {
            self.audio_channels = v;
        }
        if let Some(v) = file.log_level {
            self.log_level = v;
        }
        if let Some(mut v) = file.device_name {
            truncate_device_name(&mut v);
            self.device_name = v;
        }
        if let Some(v) = file.window_width {
            self.window.width = v;
        }
        if let Some(v) = file.window_height {
            self.window.height = v;
        }
        if let Some(v) = file.window_logical_width {
            self.window.logical_width = v;
        }
        if let Some(v) = file.window_logical_height {
            self.window.logical_height = v;
        }
        if let Some(v) = file.window_x {
            self.window.x = v;
        }
        if let Some(v) = file.window_y {
            self.window.y = v;
        }
        if let Some(v) = file.window_maximized {
            self.window.maximized = v;
        }
        if let Some(v) = file.audio_exclusive {
            self.audio_exclusive = v;
        }
        if let Some(v) = file.disable_gpu_compositing {
            self.disable_gpu_compositing = v;
        }
        if let Some(v) = file.transparent_titlebar {
            self.transparent_titlebar = v;
        }
        if let Some(v) = file.force_transcoding {
            self.force_transcoding = v;
        }
        if let Some(v) = file.window_decorations {
            self.window_decorations = Some(v);
        }
        if let Some(v) = file.hide_scrollbar {
            self.hide_scrollbar = v;
        }
    }

    fn to_file(&self) -> SettingsFile {
        let size = self.window.width > 0 && self.window.height > 0;
        let logical = self.window.logical_width > 0 && self.window.logical_height > 0;
        let position = self.window.x >= 0 && self.window.y >= 0;
        SettingsFile {
            server_url: Some(self.server_url.clone()),
            window_width: size.then_some(self.window.width),
            window_height: size.then_some(self.window.height),
            window_logical_width: logical.then_some(self.window.logical_width),
            window_logical_height: logical.then_some(self.window.logical_height),
            window_x: position.then_some(self.window.x),
            window_y: position.then_some(self.window.y),
            window_maximized: Some(self.window.maximized),
            hwdec: (self.hwdec != Hwdec::default()).then_some(self.hwdec),
            audio_passthrough: (!self.audio_passthrough.is_empty())
                .then(|| self.audio_passthrough.clone()),
            audio_exclusive: self.audio_exclusive.then_some(true),
            audio_channels: (!self.audio_channels.is_empty()).then(|| self.audio_channels.clone()),
            disable_gpu_compositing: self.disable_gpu_compositing.then_some(true),
            transparent_titlebar: (!self.transparent_titlebar).then_some(false),
            log_level: (!self.log_level.is_empty()).then(|| self.log_level.clone()),
            force_transcoding: self.force_transcoding.then_some(true),
            window_decorations: self.window_decorations,
            hide_scrollbar: (!self.hide_scrollbar).then_some(false),
            device_name: (!self.device_name.is_empty()).then(|| self.device_name.clone()),
        }
    }

    fn cli_json(&self) -> String {
        let view = CliSettings {
            hwdec: self.hwdec,
            audio_passthrough: (!self.audio_passthrough.is_empty())
                .then_some(self.audio_passthrough.as_str()),
            audio_exclusive: self.audio_exclusive.then_some(true),
            audio_channels: (!self.audio_channels.is_empty())
                .then_some(self.audio_channels.as_str()),
            disable_gpu_compositing: self.disable_gpu_compositing.then_some(true),
            transparent_titlebar: (!self.transparent_titlebar).then_some(false),
            log_level: (!self.log_level.is_empty()).then_some(self.log_level.as_str()),
            force_transcoding: self.force_transcoding,
            hide_scrollbar: self.hide_scrollbar,
            device_name: (!self.device_name.is_empty()).then_some(self.device_name.as_str()),
            device_name_default: default_device_name(),
            hwdec_options: hwdec_options(),
        };
        serde_json::to_string(&view).unwrap_or_default()
    }
}

struct State {
    data: SettingsData,
    path: PathBuf,
}

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| {
        Mutex::new(State {
            data: SettingsData::default(),
            path: PathBuf::new(),
        })
    })
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static SAVE_LOCK: Mutex<()> = Mutex::new(());

// Single persistent background save worker. save_async() coalesces into
// SavePending::data (only the newest snapshot survives); the worker wakes,
// writes the latest snapshot, then sleeps. Shutdown drains any queued write
// and joins the thread so nothing is lost at exit.

/// Coalescing slot for the background writer: only the newest snapshot
/// survives, and `stop` both drains the slot and ends the worker.
struct SavePending {
    data: Option<SettingsData>,
    path: PathBuf,
    stop: bool,
}

struct SaveWorker {
    mailbox: Mailbox<SavePending>,
    /// `Some` exactly while the worker thread is running; taken by shutdown.
    handle: Mutex<Option<JoinHandle<()>>>,
}

static SAVE_WORKER: OnceLock<SaveWorker> = OnceLock::new();

fn save_worker() -> &'static SaveWorker {
    SAVE_WORKER.get_or_init(|| SaveWorker {
        mailbox: Mailbox::new(SavePending {
            data: None,
            path: PathBuf::new(),
            stop: false,
        }),
        handle: Mutex::new(None),
    })
}

fn save_worker_loop(w: &'static SaveWorker) {
    // A stop with a snapshot still queued writes it, then exits on the next
    // pass with an empty slot.
    while let Some((data, path)) = w.mailbox.wait(
        |p| p.data.is_some() || p.stop,
        |p| p.data.take().map(|d| (d, p.path.clone())),
    ) {
        save_data(&path, &data);
    }
}

fn save_data(path: &Path, data: &SettingsData) -> bool {
    let Ok(mut text) = serde_json::to_string_pretty(&data.to_file()) else {
        return false;
    };
    text.push('\n');
    let _guard = SAVE_LOCK.lock();
    jfn_paths::write_atomic(path, text.as_bytes()).is_ok()
}

// =====================================================================
// Public Rust API
// =====================================================================

/// Initialize the settings store with the on-disk path. Idempotent: only the
/// first call sets the path; subsequent calls are ignored.
pub fn settings_init(path: &Path) {
    let mut st = state().lock();
    if st.path.as_os_str().is_empty() {
        st.path = path.to_path_buf();
    }
}

/// Load settings from the configured path. Missing keys keep their defaults.
/// Returns false if the file is missing or contains invalid JSON.
pub fn settings_load() -> bool {
    let mut st = state().lock();
    let path = st.path.clone();
    let Ok(contents) = fs::read_to_string(&path) else {
        return false;
    };
    let Ok(file) = serde_json::from_str::<SettingsFile>(&contents) else {
        return false;
    };
    st.data.overlay(file);
    true
}

/// Serialize current state and atomically write to the configured path.
pub fn settings_save() -> bool {
    let (path, snap) = {
        let st = state().lock();
        (st.path.clone(), st.data.clone())
    };
    save_data(&path, &snap)
}

/// Snapshot current state and hand it to the background save worker. Repeated
/// calls coalesce: only the most recent snapshot is written. The worker is
/// started lazily on the first call. After [`settings_shutdown_save_worker`]
/// this becomes a no-op.
pub fn settings_save_async() {
    let (path, snap) = {
        let st = state().lock();
        (st.path.clone(), st.data.clone())
    };
    let w = save_worker();
    // Hold `handle` across the spawn so a second caller racing in between the
    // enqueue and the JoinHandle store can't observe a started worker before
    // the thread actually exists.
    let mut handle = w.handle.lock();
    let queued = w.mailbox.update(|p| {
        if p.stop {
            return false;
        }
        p.data = Some(snap);
        p.path = path;
        true
    });
    if queued && handle.is_none() {
        *handle = Some(thread::spawn(|| save_worker_loop(save_worker())));
    }
}

/// Stop the background save worker after draining any pending write. Safe to
/// call if the worker was never started; safe to call multiple times.
pub fn settings_shutdown_save_worker() {
    let Some(w) = SAVE_WORKER.get() else {
        return;
    };
    if !w.mailbox.update(|p| !std::mem::replace(&mut p.stop, true)) {
        return;
    }
    let handle = w.handle.lock().take();
    if let Some(h) = handle
        && let Err(e) = h.join()
    {
        eprintln!("[config] save worker panicked: {e:?}");
    }
}

macro_rules! string_accessors {
    ($getter:ident, $setter:ident, $field:ident) => {
        pub fn $getter() -> String {
            state().lock().data.$field.clone()
        }
        pub fn $setter(v: &str) {
            state().lock().data.$field = v.to_string();
        }
    };
}

macro_rules! bool_accessors {
    ($getter:ident, $setter:ident, $field:ident) => {
        pub fn $getter() -> bool {
            state().lock().data.$field
        }
        pub fn $setter(v: bool) {
            state().lock().data.$field = v;
        }
    };
}

string_accessors!(server_url, set_server_url, server_url);

pub fn hwdec() -> Hwdec {
    state().lock().data.hwdec
}

pub fn set_hwdec(v: Hwdec) {
    state().lock().data.hwdec = v;
}
string_accessors!(audio_passthrough, set_audio_passthrough, audio_passthrough);
string_accessors!(audio_channels, set_audio_channels, audio_channels);
string_accessors!(log_level, set_log_level, log_level);

pub fn device_name() -> String {
    state().lock().data.device_name.clone()
}

/// Clamp to the server's 64-byte DeviceName column, never splitting a
/// character.
fn truncate_device_name(s: &mut String) {
    if s.len() <= DEVICE_NAME_MAX {
        return;
    }
    let mut end = DEVICE_NAME_MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

#[cfg(unix)]
pub fn default_device_name() -> String {
    let mut s = gethostname::gethostname().to_string_lossy().into_owned();
    truncate_device_name(&mut s);
    s
}

#[cfg(windows)]
pub fn default_device_name() -> String {
    let mut s = std::env::var("COMPUTERNAME").unwrap_or_default();
    truncate_device_name(&mut s);
    s
}

/// Setter for device_name. Trims and collapses whitespace, truncates to the
/// server's 64-char DeviceName column limit, and clears the override when the
/// result matches `platform_default` (so hostname changes propagate
/// automatically on the next launch).
pub fn set_device_name(raw: &str, platform_default: &str) {
    let cleaned = normalize_device_name(raw, platform_default);
    state().lock().data.device_name = cleaned;
}

bool_accessors!(audio_exclusive, set_audio_exclusive, audio_exclusive);
bool_accessors!(
    disable_gpu_compositing,
    set_disable_gpu_compositing,
    disable_gpu_compositing
);
bool_accessors!(
    transparent_titlebar,
    set_transparent_titlebar,
    transparent_titlebar
);
bool_accessors!(force_transcoding, set_force_transcoding, force_transcoding);
/// The user's explicit decoration choice, unresolved; `None` when unset.
pub fn configured_window_decorations() -> Option<WindowDecorations> {
    state().lock().data.window_decorations
}

/// Browser-process only: falls back to the installed `Platform`, which panics
/// if absent.
pub fn window_decorations_mode() -> WindowDecorations {
    let configured = state().lock().data.window_decorations;
    jfn_platform_abi::get().resolve_window_decorations(configured)
}

pub fn window_decorations() -> String {
    window_decorations_mode().as_str().to_string()
}
pub fn set_window_decorations(v: Option<&str>) {
    state().lock().data.window_decorations = v.and_then(WindowDecorations::parse);
}

/// True when the app draws its own (client-side) titlebar.
pub fn client_side_decorations() -> bool {
    window_decorations_mode() == WindowDecorations::Csd
}
pub fn titlebar_theme_color() -> bool {
    window_decorations_mode() == WindowDecorations::ServerThemed
}
bool_accessors!(hide_scrollbar, set_hide_scrollbar, hide_scrollbar);

pub fn window_geometry() -> JfnWindowGeometry {
    state().lock().data.window
}

pub fn set_window_geometry(g: JfnWindowGeometry) {
    state().lock().data.window = g;
}

pub fn cli_json() -> String {
    let snap = state().lock().data.clone();
    snap.cli_json()
}

fn normalize_device_name(raw: &str, platform_default: &str) -> String {
    // Server's auth header parser preserves whitespace verbatim, so " foo "
    // would round-trip into the Devices table.
    let mut trimmed = String::with_capacity(raw.len());
    let mut in_space = true;
    for c in raw.chars() {
        let ws = matches!(c, ' ' | '\t' | '\r' | '\n' | '\u{0b}' | '\u{0c}');
        if ws {
            if !in_space {
                trimmed.push(' ');
            }
            in_space = true;
        } else {
            trimmed.push(c);
            in_space = false;
        }
    }
    if trimmed.ends_with(' ') {
        trimmed.pop();
    }
    truncate_device_name(&mut trimmed);
    if trimmed == platform_default {
        trimmed.clear();
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::{
        SettingsData, SettingsFile, WindowDecorations, default_device_name, normalize_device_name,
    };

    const PLATFORM: &str = "platform-host";

    /// Top-level keys in the order they appear in the text; `serde_json::Value`
    /// would reorder them.
    fn keys(json: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut depth = 0usize;
        let mut chars = json.chars().peekable();
        let mut current = String::new();
        let mut in_string = false;
        while let Some(c) = chars.next() {
            if in_string {
                match c {
                    '\\' => {
                        chars.next();
                    }
                    '"' => in_string = false,
                    _ => current.push(c),
                }
                continue;
            }
            match c {
                '"' => {
                    in_string = true;
                    current.clear();
                }
                ':' if depth == 1 => out.push(std::mem::take(&mut current)),
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                _ => {}
            }
        }
        out
    }

    fn loaded(json: &str) -> SettingsData {
        let file: SettingsFile = serde_json::from_str(json).expect("valid json");
        let mut data = SettingsData::default();
        data.overlay(file);
        data
    }

    #[test]
    fn default_settings_write_only_server_url_and_maximized() {
        let text = serde_json::to_string(&SettingsData::default().to_file()).expect("serializes");
        assert_eq!(text, r#"{"serverUrl":"","windowMaximized":false}"#);
    }

    #[test]
    fn every_key_writes_in_schema_order() {
        let data = SettingsData {
            server_url: "http://host".into(),
            hwdec: "auto".parse().expect("auto is offered everywhere"),
            audio_passthrough: "eac3".into(),
            audio_channels: "stereo".into(),
            log_level: "debug".into(),
            device_name: "box".into(),
            window: super::JfnWindowGeometry {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
                logical_width: 5,
                logical_height: 6,
                maximized: true,
            },
            audio_exclusive: true,
            disable_gpu_compositing: true,
            transparent_titlebar: false,
            force_transcoding: true,
            window_decorations: Some(WindowDecorations::ServerThemed),
            hide_scrollbar: false,
        };
        let text = serde_json::to_string(&data.to_file()).expect("serializes");
        assert_eq!(
            keys(&text),
            [
                "serverUrl",
                "windowWidth",
                "windowHeight",
                "windowLogicalWidth",
                "windowLogicalHeight",
                "windowX",
                "windowY",
                "windowMaximized",
                "hwdec",
                "audioPassthrough",
                "audioExclusive",
                "audioChannels",
                "disableGpuCompositing",
                "transparentTitlebar",
                "logLevel",
                "forceTranscoding",
                "windowDecorations",
                "hideScrollbar",
                "deviceName",
            ]
        );
        assert!(text.contains(r#""windowDecorations":"serverThemed""#));
    }

    #[test]
    fn absent_keys_leave_defaults() {
        let data = loaded(r#"{"serverUrl":"http://host"}"#);
        assert_eq!(data.server_url, "http://host");
        assert!(data.transparent_titlebar);
        assert!(data.hide_scrollbar);
        assert_eq!(data.window.x, -1);
    }

    #[test]
    fn transparent_titlebar_false_round_trips_and_true_is_omitted() {
        let false_data = loaded(r#"{"transparentTitlebar":false}"#);
        assert!(!false_data.transparent_titlebar);
        let false_json = serde_json::to_string(&false_data.to_file()).expect("serializes");
        assert!(false_json.contains(r#""transparentTitlebar":false"#));
        assert!(!loaded(&false_json).transparent_titlebar);

        let true_json =
            serde_json::to_string(&SettingsData::default().to_file()).expect("serializes");
        assert!(!true_json.contains("transparentTitlebar"));
        assert!(loaded(&true_json).transparent_titlebar);
    }

    #[test]
    fn wrong_typed_key_is_ignored_and_rest_of_file_loads() {
        let data = loaded(r#"{"windowWidth":"wide","serverUrl":"http://host","hideScrollbar":7}"#);
        assert_eq!(data.window.width, 0);
        assert!(data.hide_scrollbar);
        assert_eq!(data.server_url, "http://host");
    }

    #[test]
    fn unknown_keys_and_unknown_decorations_are_ignored() {
        let data = loaded(r#"{"nope":1,"windowDecorations":"fancy","serverUrl":"u"}"#);
        assert_eq!(data.window_decorations, None);
        assert_eq!(data.server_url, "u");
    }

    #[test]
    fn overlong_device_name_loads_truncated_on_char_boundary() {
        let ascii = "x".repeat(100);
        let data = loaded(&format!(r#"{{"deviceName":"{ascii}"}}"#));
        assert_eq!(data.device_name, "x".repeat(64));

        let multibyte = "é".repeat(40);
        let data = loaded(&format!(r#"{{"deviceName":"{multibyte}"}}"#));
        assert!(data.device_name.len() <= 64);
        assert_eq!(data.device_name, "é".repeat(32));
    }

    #[test]
    fn cli_json_emits_the_web_ui_contract() {
        let data = SettingsData {
            transparent_titlebar: false,
            device_name: "box".into(),
            ..SettingsData::default()
        };
        let text = data.cli_json();
        assert_eq!(
            keys(&text),
            [
                "hwdec",
                "transparentTitlebar",
                "forceTranscoding",
                "hideScrollbar",
                "deviceName",
                "deviceNameDefault",
                "hwdecOptions",
            ]
        );
        let options = serde_json::to_string(super::hwdec_options()).expect("serializes");
        assert!(text.contains(&format!(r#""hwdecOptions":{options}"#)));
        assert!(text.contains(&format!(
            r#""deviceNameDefault":"{}""#,
            default_device_name()
        )));
    }

    #[test]
    fn trims_leading_and_trailing_whitespace() {
        assert_eq!(normalize_device_name("  foo  ", PLATFORM), "foo");
        assert_eq!(normalize_device_name("\t\nfoo\r\n", PLATFORM), "foo");
    }

    #[test]
    fn collapses_internal_whitespace_runs() {
        assert_eq!(normalize_device_name("foo  bar", PLATFORM), "foo bar");
        assert_eq!(normalize_device_name("foo\t\tbar", PLATFORM), "foo bar");
        assert_eq!(
            normalize_device_name("foo \t\nbar   baz", PLATFORM),
            "foo bar baz"
        );
    }

    #[test]
    fn whitespace_only_is_empty() {
        assert_eq!(normalize_device_name("   \t\n  ", PLATFORM), "");
    }

    #[test]
    fn preserves_single_internal_spaces() {
        assert_eq!(
            normalize_device_name("Andrew's MacBook Pro", PLATFORM),
            "Andrew's MacBook Pro"
        );
    }

    #[test]
    fn clamps_to_64_chars() {
        let long_name = "x".repeat(100);
        assert_eq!(normalize_device_name(&long_name, PLATFORM), "x".repeat(64));
    }

    #[test]
    fn clamps_after_whitespace_normalization() {
        let padded = format!("  {}  ", "x".repeat(70));
        assert_eq!(normalize_device_name(&padded, PLATFORM).len(), 64);
    }

    #[test]
    fn clears_override_when_value_equals_platform_default() {
        assert_eq!(normalize_device_name(PLATFORM, PLATFORM), "");
    }

    #[test]
    fn clears_override_when_whitespace_padded_default() {
        let padded = format!("  {}  ", PLATFORM);
        assert_eq!(normalize_device_name(&padded, PLATFORM), "");
    }
}
