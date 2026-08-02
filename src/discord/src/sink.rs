use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use std::sync::OnceLock;

use jfn_playback::{MediaMetadata, PlaybackEvent, PlaybackEventKind, PlaybackSnapshot};

use crate::activity;
use crate::ipc::Connection;
use crate::projection::{self, Activity, ProjectInput};

const DEFAULT_APPLICATION_ID: &str = "";

const MIN_PUSH_INTERVAL: Duration = Duration::from_secs(4);

const WATCHDOG: Duration = Duration::from_secs(60);

const TICK: Duration = Duration::from_millis(250);
const RECONNECT_MIN: Duration = Duration::from_secs(5);
const RECONNECT_MAX: Duration = Duration::from_secs(60);

enum Msg {
    Event(Box<PlaybackEvent>),
    Stop,
}

struct Sink {
    tx: Sender<Msg>,
    join: Option<JoinHandle<()>>,
}

fn sink_slot() -> &'static Mutex<Option<Sink>> {
    static SLOT: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

#[derive(Default)]
struct State {
    meta: MediaMetadata,
    snapshot: PlaybackSnapshot,
    timeline_armed: bool,
}

fn is_interesting(kind: PlaybackEventKind) -> bool {
    matches!(
        kind,
        PlaybackEventKind::Started
            | PlaybackEventKind::Paused
            | PlaybackEventKind::Finished
            | PlaybackEventKind::Canceled
            | PlaybackEventKind::Error
            | PlaybackEventKind::TrackLoaded
            | PlaybackEventKind::MetadataChanged
            | PlaybackEventKind::RateChanged
            | PlaybackEventKind::DurationChanged
            | PlaybackEventKind::Seeked
            | PlaybackEventKind::SeekingChanged
            | PlaybackEventKind::BufferingChanged
    )
}

fn apply(st: &mut State, ev: &PlaybackEvent) -> bool {
    st.snapshot = ev.snapshot.clone();

    match ev.kind {
        PlaybackEventKind::MetadataChanged => {
            if ev.metadata.id.is_empty() {
                return false;
            }
            if ev.metadata.id == st.meta.id {
                let art = std::mem::take(&mut st.meta.art_url);
                st.meta = ev.metadata.clone();
                if st.meta.art_url.is_empty() {
                    st.meta.art_url = art;
                }
            } else {
                st.meta = ev.metadata.clone();
                st.timeline_armed = false;
            }
            true
        }
        PlaybackEventKind::Started => {
            st.timeline_armed = true;
            true
        }
        PlaybackEventKind::SeekingChanged
        | PlaybackEventKind::BufferingChanged
        | PlaybackEventKind::Paused
        | PlaybackEventKind::Finished
        | PlaybackEventKind::Canceled
        | PlaybackEventKind::Error
        | PlaybackEventKind::TrackLoaded
        | PlaybackEventKind::RateChanged
        | PlaybackEventKind::DurationChanged
        | PlaybackEventKind::Seeked => true,
        _ => false,
    }
}

fn project_state(st: &State) -> Option<Activity> {
    projection::project(&ProjectInput {
        phase: st.snapshot.phase,
        seeking: st.snapshot.seeking,
        buffering: st.snapshot.buffering,
        rate: st.snapshot.rate,
        position_us: st.snapshot.position_us,
        duration_us: if st.meta.duration_us > 0 {
            st.meta.duration_us
        } else {
            st.snapshot.duration_us
        },
        meta: &st.meta,
        timeline_armed: st.timeline_armed,
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

struct Backoff {
    next_attempt: Instant,
    delay: Duration,
}

impl Backoff {
    fn new(now: Instant) -> Self {
        Self {
            next_attempt: now,
            delay: RECONNECT_MIN,
        }
    }
    fn due(&self, now: Instant) -> bool {
        now >= self.next_attempt
    }
    fn fail(&mut self, now: Instant) {
        self.next_attempt = now + self.delay;
        self.delay = (self.delay * 2).min(RECONNECT_MAX);
    }
    fn reset(&mut self) {
        self.delay = RECONNECT_MIN;
    }
}

fn worker(rx: Receiver<Msg>, application_id: String) {
    let mut st = State::default();
    let mut conn: Option<Connection> = None;
    let mut backoff = Backoff::new(Instant::now());
    let mut nonce: u64 = 0;

    let mut last_sent: Option<Option<Activity>> = None;
    let mut pending = true;
    let mut last_push = Instant::now() - MIN_PUSH_INTERVAL;
    let mut next_watchdog = Instant::now() + WATCHDOG;

    loop {
        match rx.recv_timeout(TICK) {
            Ok(Msg::Stop) => break,
            Ok(Msg::Event(ev)) => {
                if apply(&mut st, &ev) {
                    pending = true;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        let now = Instant::now();

        if conn.as_ref().is_some_and(|c| !c.is_alive()) {
            conn = None;
            last_sent = None;
            backoff.fail(now);
        }

        if conn.is_none() && backoff.due(now) {
            conn = Connection::connect(&application_id);
            if conn.is_some() {
                backoff.reset();
                last_sent = None;
                pending = true;
            } else {
                backoff.fail(now);
            }
        }

        if now >= next_watchdog {
            next_watchdog = now + WATCHDOG;
            pending = true;
        }

        if pending
            && let Some(c) = conn.as_ref()
            && now.duration_since(last_push) >= MIN_PUSH_INTERVAL
        {
            let next = project_state(&st);
            if last_sent.as_ref() != Some(&next) {
                nonce = nonce.wrapping_add(1);
                let payload = next.as_ref().map(|a| activity::to_json(a, now_ms()));
                if c.send_activity(payload, nonce) {
                    last_sent = Some(next);
                    last_push = now;
                }
            }
            pending = false;
        }
    }

    if let Some(c) = conn.as_ref() {
        c.send_activity(None, nonce.wrapping_add(1));
        c.close();
    }
}

fn deliver(ev: PlaybackEvent) {
    if let Some(s) = sink_slot().lock().as_ref() {
        let _ = s.tx.send(Msg::Event(Box::new(ev)));
    }
}

fn is_valid_application_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())
}

fn resolve_application_id() -> String {
    let configured = jfn_config::discord_application_id();
    let trimmed = configured.trim();
    let id = if trimmed.is_empty() {
        DEFAULT_APPLICATION_ID
    } else {
        trimmed
    };
    if id.is_empty() {
        return String::new();
    }
    if !is_valid_application_id(id) {
        tracing::warn!(
            target: "Media",
            "discord: application id {id:?} is not a number; presence stays off"
        );
        return String::new();
    }
    id.to_owned()
}

pub fn start() {
    if !jfn_config::discord_rich_presence() {
        return;
    }
    let application_id = resolve_application_id();
    if application_id.is_empty() {
        tracing::info!(
            target: "Media",
            "discord: rich presence enabled but no application id is set; staying idle"
        );
        return;
    }

    let mut slot = sink_slot().lock();
    if slot.is_some() {
        return;
    }
    let (tx, rx) = channel::<Msg>();
    let join = match thread::Builder::new()
        .name("discord-sink".into())
        .spawn(move || worker(rx, application_id))
    {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(target: "Media", "discord: spawn sink thread: {e}");
            return;
        }
    };
    *slot = Some(Sink {
        tx,
        join: Some(join),
    });
    drop(slot);

    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| {
        jfn_playback::register_event_sink(Box::new(|ev: &PlaybackEvent| {
            if is_interesting(ev.kind) {
                deliver(ev.clone());
            }
        }));
    });
}

pub fn stop() {
    let taken = sink_slot().lock().take();
    let Some(mut s) = taken else {
        return;
    };
    let _ = s.tx.send(Msg::Stop);
    if let Some(h) = s.join.take()
        && h.join().is_err()
    {
        tracing::error!(target: "Media", "discord: sink thread panicked");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_playback::{ItemKind, MediaType, PlaybackPhase};

    fn meta(id: &str, title: &str) -> MediaMetadata {
        MediaMetadata {
            id: id.into(),
            title: title.into(),
            kind: ItemKind::Episode,
            media_type: MediaType::Video,
            ..MediaMetadata::default()
        }
    }

    fn event(kind: PlaybackEventKind) -> PlaybackEvent {
        let mut ev = PlaybackEvent {
            kind,
            flag: false,
            error_message: String::new(),
            snapshot: PlaybackSnapshot::default(),
            metadata: MediaMetadata::default(),
            artwork_uri: String::new(),
            can_go_next: false,
            can_go_prev: false,
        };
        ev.snapshot.rate = 1.0;
        ev.snapshot.phase = PlaybackPhase::Playing;
        ev
    }

    #[test]
    fn blank_metadata_never_clobbers_a_real_entry() {
        let mut st = State::default();
        let mut ev = event(PlaybackEventKind::MetadataChanged);
        ev.metadata = meta("abc", "Real Title");
        assert!(apply(&mut st, &ev));

        let mut blank = event(PlaybackEventKind::MetadataChanged);
        blank.metadata = MediaMetadata::default();
        assert!(!apply(&mut st, &blank));
        assert_eq!(st.meta.title, "Real Title");
    }

    #[test]
    fn same_id_reannounce_keeps_the_resolved_poster() {
        let mut st = State::default();
        let mut rich = event(PlaybackEventKind::MetadataChanged);
        rich.metadata = meta("abc", "Title");
        rich.metadata.art_url = "https://jf/poster.jpg".into();
        apply(&mut st, &rich);

        let mut poor = event(PlaybackEventKind::MetadataChanged);
        poor.metadata = meta("abc", "Title");
        apply(&mut st, &poor);
        assert_eq!(st.meta.art_url, "https://jf/poster.jpg");
    }

    #[test]
    fn a_different_item_clears_the_poster_and_disarms_the_bar() {
        let mut st = State::default();
        let mut first = event(PlaybackEventKind::MetadataChanged);
        first.metadata = meta("abc", "First");
        first.metadata.art_url = "https://jf/one.jpg".into();
        apply(&mut st, &first);
        st.timeline_armed = true;

        let mut second = event(PlaybackEventKind::MetadataChanged);
        second.metadata = meta("xyz", "Second");
        apply(&mut st, &second);
        assert_eq!(st.meta.art_url, "");
        assert!(!st.timeline_armed);
    }

    #[test]
    fn started_arms_the_bar() {
        let mut st = State::default();
        assert!(!st.timeline_armed);
        assert!(apply(&mut st, &event(PlaybackEventKind::Started)));
        assert!(st.timeline_armed);
    }

    #[test]
    fn position_changes_are_not_forwarded() {
        assert!(!is_interesting(PlaybackEventKind::PositionChanged));
        assert!(!is_interesting(PlaybackEventKind::BufferedRangesChanged));
        assert!(is_interesting(PlaybackEventKind::SeekingChanged));
    }

    #[test]
    fn stopped_playback_projects_to_no_activity() {
        let mut st = State::default();
        let mut ev = event(PlaybackEventKind::MetadataChanged);
        ev.metadata = meta("abc", "Title");
        apply(&mut st, &ev);
        st.snapshot.phase = PlaybackPhase::Stopped;
        assert!(project_state(&st).is_none());
    }

    #[test]
    fn only_numeric_application_ids_are_accepted() {
        assert!(is_valid_application_id("1533503787145363616"));
        for bad in [
            "",
            "not-an-id",
            "1533503787145363616 ",
            "https://discord.com/app/123",
            "Jellium",
        ] {
            assert!(
                !is_valid_application_id(bad),
                "should have rejected {bad:?}"
            );
        }
    }

    #[test]
    fn backoff_grows_and_caps() {
        let now = Instant::now();
        let mut b = Backoff::new(now);
        assert!(b.due(now));
        for _ in 0..20 {
            b.fail(now);
        }
        assert_eq!(b.delay, RECONNECT_MAX);
        b.reset();
        assert_eq!(b.delay, RECONNECT_MIN);
    }
}
