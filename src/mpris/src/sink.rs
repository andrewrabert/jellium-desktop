//! MPRIS direct sink. A [`QueuedSink`] run by the playback sink harness on
//! its consumer thread, holding a zbus blocking Connection. Each delivered
//! PlaybackEvent updates the content/snapshot state and emits
//! PropertiesChanged + Seeked signals.
//!
//! Method/property handlers run inline on zbus's reactor thread; outbound
//! transport (Play/Pause/Stop/etc.) calls jfn_mpv_* directly. Next/
//! Previous/Seek/SetPosition route to the JS UI via the registered exec_js
//! callback.

use async_io::block_on;
use parking_lot::Mutex;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use zbus::blocking::Connection;
use zbus::blocking::object_server::InterfaceRef;
use zbus::fdo;
use zbus::interface;
use zbus::object_server::{Interface, SignalEmitter};
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

use crate::projection;
use jfn_playback::sink_core::{self, QueuedSink};
use jfn_playback::{MediaMetadata, PlaybackEvent, PlaybackEventKind, PlaybackSnapshot};

const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const BASE_SERVICE_NAME: &str = "org.mpris.MediaPlayer2.JelliumDesktop";
// MPRIS clients poll Position and every event moves it, so it is never
// part of a changed set
const POLLED_PROPERTY: &str = "Position";

// ============================================================================
// Content + projected view
// ============================================================================

#[derive(Clone, Debug, Default)]
struct Content {
    metadata: MediaMetadata,
    pending_rate: f64,
    volume: f64,
    can_go_next: bool,
    can_go_previous: bool,
}

impl Content {
    fn fresh() -> Self {
        Self {
            metadata: MediaMetadata::default(),
            pending_rate: 1.0,
            volume: 1.0,
            can_go_next: false,
            can_go_previous: false,
        }
    }
}

fn status_name(s: projection::MprisStatus) -> &'static str {
    match s {
        projection::MprisStatus::Playing => "Playing",
        projection::MprisStatus::Paused => "Paused",
        projection::MprisStatus::Stopped => "Stopped",
    }
}

fn insert_value(m: &mut HashMap<String, OwnedValue>, key: &str, v: Value<'_>) {
    match OwnedValue::try_from(v) {
        Ok(ov) => {
            m.insert(key.to_string(), ov);
        }
        Err(e) => eprintln!("mpris: encode {key}: {e}"),
    }
}

fn metadata_to_dict(meta: &MediaMetadata) -> HashMap<String, OwnedValue> {
    let mut m = HashMap::new();
    // mpris:trackid is required by spec.
    if let Ok(track_id) = ObjectPath::try_from("/net/nullsum/JelliumDesktop/track/1") {
        insert_value(&mut m, "mpris:trackid", Value::from(track_id));
    }
    if meta.duration_us > 0 {
        insert_value(&mut m, "mpris:length", Value::from(meta.duration_us));
    }
    if !meta.title.is_empty() {
        insert_value(&mut m, "xesam:title", Value::from(meta.title.as_str()));
    }
    if !meta.artist.is_empty() {
        insert_value(
            &mut m,
            "xesam:artist",
            Value::from(vec![meta.artist.as_str()]),
        );
    }
    if !meta.album.is_empty() {
        insert_value(&mut m, "xesam:album", Value::from(meta.album.as_str()));
    }
    if meta.track_number > 0 {
        insert_value(&mut m, "xesam:trackNumber", Value::from(meta.track_number));
    }
    if !meta.art_data_uri.is_empty() {
        insert_value(
            &mut m,
            "mpris:artUrl",
            Value::from(meta.art_data_uri.as_str()),
        );
    }
    m
}

// ============================================================================
// Shared state — accessed by zbus reactor thread (interface impls) and the
// event-pump thread (worker). Single Mutex; getters are read-only fast paths.
// ============================================================================

struct State {
    content: Content,
    snapshot: PlaybackSnapshot,
}

impl State {
    fn fresh() -> Self {
        Self {
            content: Content::fresh(),
            snapshot: PlaybackSnapshot::default(),
        }
    }

    fn derived(&self) -> projection::MprisDerived {
        projection::project(&projection::ProjectInput {
            phase: self.snapshot.phase,
            seeking: self.snapshot.seeking,
            buffering: self.snapshot.buffering,
            metadata_duration_us: self.content.metadata.duration_us,
            pending_rate: self.content.pending_rate,
        })
    }

    /// metadata_active=false -> clean transport while nothing is loaded.
    fn visible_metadata(&self) -> HashMap<String, OwnedValue> {
        if self.derived().metadata_active {
            metadata_to_dict(&self.content.metadata)
        } else {
            metadata_to_dict(&MediaMetadata::default())
        }
    }
}

// ============================================================================
// D-Bus interface impls
// ============================================================================

struct Root;

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {}
    fn quit(&self) {}

    #[zbus(property)]
    fn identity(&self) -> &str {
        "Jellium Desktop"
    }
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_set_fullscreen(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn fullscreen(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct Player {
    state: Arc<Mutex<State>>,
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn play(&self) {
        sink_core::execute(sink_core::MediaCommand::Play)
    }
    fn pause(&self) {
        sink_core::execute(sink_core::MediaCommand::Pause)
    }
    fn play_pause(&self) {
        sink_core::execute(sink_core::MediaCommand::PlayPause)
    }
    fn stop(&self) {
        sink_core::execute(sink_core::MediaCommand::Stop)
    }
    fn next(&self) {
        sink_core::execute(sink_core::MediaCommand::Next);
    }
    fn previous(&self) {
        sink_core::execute(sink_core::MediaCommand::Previous);
    }
    fn seek(&self, offset: i64) {
        let cur = self.state.lock().snapshot.position_us;
        let new_pos = (cur + offset).max(0);
        sink_core::seek_to_ms(new_pos / 1000);
    }
    fn set_position(&self, _track: ObjectPath<'_>, position_us: i64) {
        sink_core::seek_to_ms(position_us / 1000);
    }

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> String {
        status_name(self.state.lock().derived().status).to_string()
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        self.state.lock().derived().rate
    }
    #[zbus(property)]
    fn set_rate(&self, value: f64) {
        let clamped = value.clamp(0.25, 2.0);
        jfn_mpv::api::jfn_mpv_set_speed(clamped);
    }
    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        0.25
    }
    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        2.0
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        self.state.lock().visible_metadata()
    }
    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.state.lock().content.volume
    }
    #[zbus(property)]
    fn position(&self) -> i64 {
        self.state.lock().snapshot.position_us
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.state.lock().content.can_go_next
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.state.lock().content.can_go_previous
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.state.lock().derived().can_play
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.state.lock().derived().can_pause
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.state.lock().derived().can_seek
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        self.state.lock().derived().can_control
    }
}

// ============================================================================
// Queued sink
// ============================================================================

/// The MPRIS transport the harness drives: connects on `init`, releases the
/// bus name on `teardown`.
struct Transport {
    service_name: String,
    bus: Option<Bus>,
}

/// A live session-bus registration.
struct Bus {
    conn: Connection,
    state: Arc<Mutex<State>>,
    iface: InterfaceRef<Player>,
    last_props: HashMap<String, OwnedValue>,
}

impl Transport {
    fn new(service_suffix: &str) -> Self {
        Transport {
            service_name: format!("{BASE_SERVICE_NAME}{service_suffix}"),
            bus: None,
        }
    }
}

impl QueuedSink for Transport {
    fn init(&mut self) {
        self.bus = connect(&self.service_name);
    }

    fn deliver(&mut self, ev: &PlaybackEvent) {
        if let Some(bus) = &mut self.bus {
            handle_event(ev, &bus.state, &bus.iface, &mut bus.last_props);
        }
    }

    fn teardown(&mut self) {
        if let Some(bus) = self.bus.take() {
            let _ = bus.conn.release_name(self.service_name.as_str());
        }
    }
}

fn connect(service_name: &str) -> Option<Bus> {
    let conn = match Connection::session() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mpris: session bus connect failed: {}", e);
            return None;
        }
    };

    if let Err(e) = conn.object_server().at(MPRIS_PATH, Root) {
        eprintln!("mpris: register root iface: {}", e);
        return None;
    }

    let state = Arc::new(Mutex::new(State::fresh()));
    let player = Player {
        state: Arc::clone(&state),
    };
    if let Err(e) = conn.object_server().at(MPRIS_PATH, player) {
        eprintln!("mpris: register player iface: {}", e);
        return None;
    }

    let iface = match conn.object_server().interface::<_, Player>(MPRIS_PATH) {
        Ok(iface) => iface,
        Err(e) => {
            eprintln!("mpris: resolve player iface: {e}");
            return None;
        }
    };
    let last_props = match player_properties(&iface) {
        Ok(props) => props,
        Err(e) => {
            eprintln!("mpris: initial property snapshot: {e}");
            return None;
        }
    };

    if let Err(e) = conn.request_name(service_name) {
        eprintln!("mpris: request name {}: {}", service_name, e);
        return None;
    }
    eprintln!("mpris: registered as {}", service_name);

    Some(Bus {
        conn,
        state,
        iface,
        last_props,
    })
}

fn handle_event(
    ev: &PlaybackEvent,
    state: &Arc<Mutex<State>>,
    iface: &InterfaceRef<Player>,
    last: &mut HashMap<String, OwnedValue>,
) {
    let snap = ev.snapshot.clone();

    // last_snap_ tracks every snapshot so getPosition() reads the latest.
    state.lock().snapshot = snap.clone();

    let mut do_recompute = false;
    let mut emit_seeked = false;
    {
        let mut s = state.lock();
        match ev.kind {
            PlaybackEventKind::MetadataChanged => {
                // Same-Id dedup: same-Id setMetadata is a semantic no-op
                // (identical item). Otherwise empty art fields in the
                // incoming meta would clobber cached art from notifyArtwork
                // on every variant switch.
                if ev.metadata.id.is_empty() || ev.metadata.id != s.content.metadata.id {
                    s.content.metadata = ev.metadata.clone();
                    do_recompute = true;
                }
            }
            PlaybackEventKind::ArtworkChanged => {
                s.content.metadata.art_data_uri = ev.artwork_uri.clone();
                do_recompute = true;
            }
            PlaybackEventKind::QueueCapsChanged => {
                s.content.can_go_next = ev.can_go_next;
                s.content.can_go_previous = ev.can_go_prev;
                do_recompute = true;
            }
            PlaybackEventKind::Started => {
                do_recompute = true;
                emit_seeked = true;
            }
            PlaybackEventKind::Seeked => {
                emit_seeked = true;
            }
            PlaybackEventKind::Paused
            | PlaybackEventKind::Finished
            | PlaybackEventKind::Canceled
            | PlaybackEventKind::Error
            | PlaybackEventKind::SeekingChanged
            | PlaybackEventKind::BufferingChanged
            | PlaybackEventKind::TrackLoaded
            | PlaybackEventKind::RateChanged => {
                do_recompute = true;
            }
            // MPRIS Position is polled, not signaled. Snapshot already
            // refreshed above so the property getter returns latest value.
            PlaybackEventKind::PositionChanged => {}
            // Duration ships inside metadata; bare DurationChanged from mpv
            // isn't surfaced to MPRIS.
            PlaybackEventKind::DurationChanged => {}
            PlaybackEventKind::MediaTypeChanged
            | PlaybackEventKind::FullscreenChanged
            | PlaybackEventKind::BufferedRangesChanged
            | PlaybackEventKind::DisplayHzChanged => {}
        }
    }

    if do_recompute && let Err(e) = emit_properties_changed(iface, last) {
        eprintln!("mpris: emit PropertiesChanged: {e}");
    }

    if emit_seeked
        && let Err(e) = block_on(Player::seeked(iface.signal_emitter(), snap.position_us))
    {
        eprintln!("mpris: emit Seeked: {e}");
    }
}

/// Every readable Player property, valued by the same getters that answer
/// `org.freedesktop.DBus.Properties.Get`.
fn player_properties(iface: &InterfaceRef<Player>) -> zbus::Result<HashMap<String, OwnedValue>> {
    let emitter = iface.signal_emitter();
    let conn = emitter.connection();
    let props = block_on(
        iface
            .get()
            .get_all(conn.object_server(), conn, None, emitter),
    )?;
    Ok(props)
}

/// Entries of `next` that are absent from `last` or hold a different value.
fn changed_properties<'a>(
    last: &HashMap<String, OwnedValue>,
    next: &'a HashMap<String, OwnedValue>,
) -> zbus::Result<HashMap<&'a str, Value<'a>>> {
    let mut changed = HashMap::new();
    for (name, value) in next {
        if name == POLLED_PROPERTY || last.get(name) == Some(value) {
            continue;
        }
        changed.insert(name.as_str(), value.try_clone()?.into());
    }
    Ok(changed)
}

/// One PropertiesChanged carrying every property whose value moved, after
/// which `last` becomes the new baseline. Emits nothing when nothing moved.
fn emit_properties_changed(
    iface: &InterfaceRef<Player>,
    last: &mut HashMap<String, OwnedValue>,
) -> zbus::Result<()> {
    let next = player_properties(iface)?;
    let changed = changed_properties(last, &next)?;
    if !changed.is_empty() {
        block_on(fdo::Properties::properties_changed(
            iface.signal_emitter(),
            Player::name(),
            changed,
            Cow::Borrowed(&[]),
        ))?;
    }
    *last = next;
    Ok(())
}

// ============================================================================
// start / stop
// ============================================================================

/// Run the MPRIS sink on the harness thread. `service_suffix` is appended to
/// the base service name (`org.mpris.MediaPlayer2.JelliumDesktop<suffix>`).
/// No-op if already running.
pub(crate) fn start(service_suffix: &str) {
    let suffix = service_suffix.to_owned();
    sink_core::run_sink("mpris-sink", move || Transport::new(&suffix));
}

pub(crate) fn stop() {
    sink_core::stop();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props<'a>(
        entries: impl IntoIterator<Item = (&'a str, Value<'a>)>,
    ) -> zbus::Result<HashMap<String, OwnedValue>> {
        let mut out = HashMap::new();
        for (name, value) in entries {
            out.insert(name.to_string(), OwnedValue::try_from(value)?);
        }
        Ok(out)
    }

    fn dict<'a>(
        entries: impl IntoIterator<Item = (&'a str, Value<'a>)>,
    ) -> zbus::Result<Value<'static>> {
        Ok(Value::from(props(entries)?))
    }

    #[test]
    fn position_is_never_reported() -> zbus::Result<()> {
        let last = props([("Position", Value::from(0i64))])?;
        let next = props([("Position", Value::from(5i64))])?;
        assert!(changed_properties(&last, &next)?.is_empty());
        Ok(())
    }

    #[test]
    fn equal_values_are_dropped() -> zbus::Result<()> {
        let last = props([("Rate", Value::from(1.0f64))])?;
        let next = props([("Rate", Value::from(1.0f64))])?;
        assert!(changed_properties(&last, &next)?.is_empty());
        Ok(())
    }

    #[test]
    fn new_keys_are_reported() -> zbus::Result<()> {
        let last = HashMap::new();
        let next = props([("CanPlay", Value::from(true))])?;
        let changed = changed_properties(&last, &next)?;
        assert_eq!(changed.len(), 1);
        assert_eq!(changed.get("CanPlay"), Some(&Value::from(true)));
        Ok(())
    }

    #[test]
    fn dict_values_compare_by_content() -> zbus::Result<()> {
        let a = dict([
            ("xesam:title", Value::from("t")),
            ("xesam:album", Value::from("a")),
        ])?;
        let b = dict([
            ("xesam:album", Value::from("a")),
            ("xesam:title", Value::from("t")),
        ])?;
        let last = props([("Metadata", a)])?;
        let next = props([("Metadata", b)])?;
        assert!(changed_properties(&last, &next)?.is_empty());
        Ok(())
    }
}
