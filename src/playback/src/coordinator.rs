//! Coordinator: owns the single mutable state machine, worker thread, and
//! sink fanout. Producers post inputs from any thread; the worker drains
//! a finite batch of queued inputs, runs transitions, publishes the canonical
//! snapshot, and invokes a stable set of sink callbacks without holding locks.

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::ffi::{ActionSink, EventSink};
use crate::state_machine::PlaybackStateMachine;
use crate::types::*;

#[derive(Debug)]
pub enum Input {
    FileLoaded,
    LoadStarting(String),
    PauseChanged(bool),
    EndFile {
        reason: EndReason,
        error_message: String,
    },
    SeekingChanged(bool),
    PausedForCache(bool),
    CoreIdle(bool),
    Position(i64),
    MediaType(MediaType),
    VideoFrameAvailable(bool),
    Speed(f64),
    Duration(i64),
    Fullscreen {
        fullscreen: bool,
        was_maximized: bool,
    },
    BufferedRanges(Vec<PlaybackBufferedRange>),
    DisplayHz(f64),
    Metadata(MediaMetadata),
    Artwork(String),
    QueueCaps {
        can_go_next: bool,
        can_go_prev: bool,
    },
    Seeked(i64),
}

struct Shared {
    /// Producer end, cleared by `stop` so the worker's `recv` disconnects
    /// even while handles remain alive.
    tx: Mutex<Option<Sender<Input>>>,
    snapshot: Mutex<PlaybackSnapshot>,
    sinks: Mutex<Sinks>,
}

type SharedEventSink = Arc<dyn Fn(&PlaybackEvent) + Send + Sync>;
type SharedActionSink = Arc<dyn Fn(&PlaybackAction) + Send + Sync>;

/// All registrations share one boundary: changes during dispatch take effect
/// on the next batch. Shared ownership lets callbacks run without this lock.
#[derive(Clone, Default)]
struct Sinks {
    events: Vec<SharedEventSink>,
    actions: Vec<SharedActionSink>,
    builtin_events: Vec<SharedEventSink>,
    builtin_actions: Vec<SharedActionSink>,
}

pub struct PlaybackCoordinator {
    shared: Arc<Shared>,
    /// Handed to the worker by `start`; `None` afterwards, which makes a
    /// second `start` a no-op.
    rx: Option<Receiver<Input>>,
    join: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct CoordinatorHandle(Arc<Shared>);

impl CoordinatorHandle {
    pub fn enqueue(&self, in_: Input) {
        if let Some(tx) = self.0.tx.lock().as_ref() {
            let _ = tx.send(in_);
        }
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        self.0.snapshot.lock().clone()
    }
}

impl PlaybackCoordinator {
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        Self {
            shared: Arc::new(Shared {
                tx: Mutex::new(Some(tx)),
                snapshot: Mutex::new(PlaybackSnapshot::fresh()),
                sinks: Mutex::new(Sinks::default()),
            }),
            rx: Some(rx),
            join: None,
        }
    }

    pub fn start(&mut self) {
        let Some(rx) = self.rx.take() else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        self.join = Some(thread::spawn(move || worker(rx, shared)));
    }

    pub fn stop(&mut self) {
        drop(self.shared.tx.lock().take());
        if let Some(h) = self.join.take()
            && let Err(e) = h.join()
        {
            eprintln!("[playback] coordinator worker panicked: {e:?}");
        }
    }

    pub fn handle(&self) -> CoordinatorHandle {
        CoordinatorHandle(Arc::clone(&self.shared))
    }

    pub fn enqueue(&self, in_: Input) {
        self.handle().enqueue(in_);
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        self.handle().snapshot()
    }

    pub fn add_event_sink(&self, sink: EventSink) {
        self.shared.sinks.lock().events.push(Arc::from(sink));
    }

    pub fn add_action_sink(&self, sink: ActionSink) {
        self.shared.sinks.lock().actions.push(Arc::from(sink));
    }

    pub fn add_builtin_event_sink(&self, sink: EventSink) {
        self.shared
            .sinks
            .lock()
            .builtin_events
            .push(Arc::from(sink));
    }

    pub fn add_builtin_action_sink(&self, sink: ActionSink) {
        self.shared
            .sinks
            .lock()
            .builtin_actions
            .push(Arc::from(sink));
    }
}

impl Default for PlaybackCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PlaybackCoordinator {
    fn drop(&mut self) {
        self.stop();
    }
}

fn worker(rx: Receiver<Input>, shared: Arc<Shared>) {
    let mut sm = PlaybackStateMachine::new();
    while let Ok(first) = rx.recv() {
        let mut events: Vec<PlaybackEvent> = Vec::new();
        let mut actions: Vec<PlaybackAction> = Vec::new();
        for input in pending_batch(&rx, first) {
            apply(&mut sm, input, &mut events);
            actions.extend(sm.consume_actions());
        }

        let snap = sm.snapshot();
        for e in &mut events {
            e.snapshot = snap.clone();
        }
        {
            let mut s = shared.snapshot.lock();
            *s = snap;
        }

        dispatch(&shared, &events, &actions);
    }
}

/// Capture the queued count once so producers cannot extend this batch
/// indefinitely and starve snapshot publication and sink delivery.
fn pending_batch(rx: &Receiver<Input>, first: Input) -> impl Iterator<Item = Input> + '_ {
    std::iter::once(first).chain(rx.try_iter().take(rx.len()))
}

fn dispatch(shared: &Shared, events: &[PlaybackEvent], actions: &[PlaybackAction]) {
    let sinks = shared.sinks.lock().clone();
    // Preserve registration order and external-before-builtin delivery.
    // Callbacks must not block; sinks own their own queue + consumer thread.
    for sink in &sinks.events {
        for event in events {
            sink(event);
        }
    }
    for sink in &sinks.actions {
        for action in actions {
            sink(action);
        }
    }
    for sink in &sinks.builtin_events {
        for event in events {
            sink(event);
        }
    }
    for sink in &sinks.builtin_actions {
        for action in actions {
            sink(action);
        }
    }
}

fn apply(sm: &mut PlaybackStateMachine, input: Input, out: &mut Vec<PlaybackEvent>) {
    let mut emitted = match input {
        Input::FileLoaded => sm.on_file_loaded(),
        Input::LoadStarting(id) => sm.on_load_starting(id),
        Input::PauseChanged(paused) => sm.on_pause_changed(paused),
        Input::EndFile {
            reason,
            error_message,
        } => sm.on_end_file(reason, error_message),
        Input::SeekingChanged(seeking) => sm.on_seeking_changed(seeking),
        Input::PausedForCache(pfc) => sm.on_paused_for_cache(pfc),
        Input::CoreIdle(ci) => sm.on_core_idle(ci),
        Input::Position(p) => sm.on_position(p),
        Input::MediaType(t) => sm.on_media_type(t),
        Input::VideoFrameAvailable(a) => sm.on_video_frame_available(a),
        Input::Speed(r) => sm.on_speed(r),
        Input::Duration(d) => sm.on_duration(d),
        Input::Fullscreen {
            fullscreen,
            was_maximized,
        } => sm.on_fullscreen(fullscreen, was_maximized),
        Input::BufferedRanges(r) => sm.on_buffered_ranges(r),
        Input::DisplayHz(h) => sm.on_display_hz(h),
        Input::Metadata(m) => {
            // Route media_type through the SM so snapshot.media_type
            // tracks metadata changes (idle inhibit reads it).
            let mut events = sm.on_media_type(m.media_type);
            let mut ev = PlaybackEvent::new(PlaybackEventKind::MetadataChanged);
            ev.metadata = m;
            events.push(ev);
            events
        }
        Input::Artwork(uri) => {
            let mut ev = PlaybackEvent::new(PlaybackEventKind::ArtworkChanged);
            ev.artwork_uri = uri;
            vec![ev]
        }
        Input::QueueCaps {
            can_go_next,
            can_go_prev,
        } => {
            let mut ev = PlaybackEvent::new(PlaybackEventKind::QueueCapsChanged);
            ev.can_go_next = can_go_next;
            ev.can_go_prev = can_go_prev;
            vec![ev]
        }
        Input::Seeked(p) => {
            let mut events = sm.on_position(p);
            events.push(PlaybackEvent::new(PlaybackEventKind::Seeked));
            events
        }
    };
    out.append(&mut emitted);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn batch_leaves_new_arrivals_for_the_next_dispatch() {
        let (tx, rx) = unbounded();
        tx.send(Input::Position(1)).expect("receiver alive");
        let first = rx.recv().expect("first input");
        tx.send(Input::Position(2)).expect("receiver alive");
        let batch = pending_batch(&rx, first);
        tx.send(Input::Position(3)).expect("receiver alive");

        let positions: Vec<_> = batch
            .map(|input| match input {
                Input::Position(position) => position,
                _ => unreachable!("only positions queued"),
            })
            .collect();
        assert_eq!(positions, [1, 2]);
        assert!(matches!(rx.try_recv(), Ok(Input::Position(3))));
    }

    #[test]
    fn dispatch_allows_registration_for_the_next_batch_and_preserves_order() {
        let coord = Arc::new(PlaybackCoordinator::new());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let weak = Arc::downgrade(&coord);
        let registered = std::sync::atomic::AtomicBool::new(false);
        let recorded = Arc::clone(&calls);
        coord.add_event_sink(Box::new(move |_| {
            recorded.lock().push("event");
            let coord = weak.upgrade().expect("coordinator alive");
            // Fail instead of deadlocking if dispatch retains the registry lock.
            assert!(coord.shared.sinks.try_lock().is_some());
            if !registered.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let recorded = Arc::clone(&recorded);
                coord.add_builtin_action_sink(Box::new(move |_| {
                    recorded.lock().push("late action");
                }));
            }
            // Callbacks can also read the published state and enqueue work.
            let _ = coord.snapshot();
            coord.enqueue(Input::Position(7));
        }));
        let recorded = Arc::clone(&calls);
        coord.add_action_sink(Box::new(move |_| recorded.lock().push("action")));
        let recorded = Arc::clone(&calls);
        coord.add_builtin_event_sink(Box::new(move |_| recorded.lock().push("builtin event")));
        let recorded = Arc::clone(&calls);
        coord.add_builtin_action_sink(Box::new(move |_| recorded.lock().push("builtin action")));

        let events = [PlaybackEvent::new(PlaybackEventKind::Seeked)];
        let actions = [PlaybackAction {
            kind: PlaybackActionKind::ApplyPendingTrackSelectionAndPlay,
        }];
        dispatch(&coord.shared, &events, &actions);
        assert_eq!(
            *calls.lock(),
            ["event", "action", "builtin event", "builtin action"]
        );
        calls.lock().clear();
        dispatch(&coord.shared, &events, &actions);
        assert_eq!(
            *calls.lock(),
            [
                "event",
                "action",
                "builtin event",
                "builtin action",
                "late action"
            ]
        );
    }

    #[test]
    fn stop_drains_accepted_inputs_with_live_producer_handles() {
        let mut coord = PlaybackCoordinator::new();
        let handle = coord.handle();
        handle.enqueue(Input::FileLoaded);
        handle.enqueue(Input::Position(42));
        coord.start();
        coord.stop();
        assert_eq!(coord.snapshot().presence, PlayerPresence::Present);
        assert_eq!(coord.snapshot().position_us, 42);
        assert!(coord.shared.tx.lock().is_none());
        handle.enqueue(Input::EndFile {
            reason: EndReason::Eof,
            error_message: String::new(),
        });
        assert_eq!(coord.snapshot().presence, PlayerPresence::Present);
    }

    #[test]
    fn snapshot_starts_fresh() {
        let coord = PlaybackCoordinator::new();
        let s = coord.snapshot();
        assert_eq!(s.presence, PlayerPresence::None);
        assert_eq!(s.phase, PlaybackPhase::Stopped);
        assert_eq!(s.rate, 1.0);
    }

    #[test]
    fn worker_updates_snapshot_after_input() {
        let mut coord = PlaybackCoordinator::new();
        coord.start();
        // Register a sink BEFORE enqueuing so the first dispatched batch
        // signals the channel. Sinks fire on the worker thread after the
        // snapshot is published, so receiving = snapshot is up-to-date.
        let (tx, rx) = mpsc::sync_channel::<()>(1);
        coord.add_event_sink(Box::new(move |_ev| {
            let _ = tx.try_send(());
        }));
        coord.enqueue(Input::FileLoaded);
        rx.recv().expect("worker never published an event");
        let s = coord.snapshot();
        assert_eq!(s.presence, PlayerPresence::Present);
        assert_eq!(s.phase, PlaybackPhase::Starting);
        coord.stop();
    }
}
