#[derive(Copy, Clone, PartialEq, Eq)]
pub enum MediaCommand {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
}

pub fn execute(cmd: MediaCommand) {
    match cmd {
        MediaCommand::Play => jfn_mpv::api::jfn_mpv_play(),
        MediaCommand::Pause => jfn_mpv::api::jfn_mpv_pause(),
        MediaCommand::PlayPause => jfn_mpv::api::jfn_mpv_toggle_pause(),
        MediaCommand::Stop => jfn_mpv::api::jfn_mpv_stop(),
        MediaCommand::Next => {
            crate::exec_js::call("if(window._nativeHostInput) window._nativeHostInput(['next']);")
        }
        MediaCommand::Previous => crate::exec_js::call(
            "if(window._nativeHostInput) window._nativeHostInput(['previous']);",
        ),
    }
}

pub fn seek_to_ms(ms: i64) {
    crate::exec_js::call(&format!("if(window._nativeSeek) window._nativeSeek({ms});"));
}

#[derive(Default)]
pub struct PositionThrottle {
    last: Option<std::time::Instant>,
    forced: bool,
}

impl PositionThrottle {
    pub const INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

    #[must_use]
    pub const fn new() -> Self {
        Self {
            last: None,
            forced: false,
        }
    }

    pub const fn force_next(&mut self) {
        self.forced = true;
    }

    pub fn due(&mut self, now: std::time::Instant, force: bool) -> bool {
        let forced = force || self.forced;
        let elapsed_ok = self.last.is_none_or(|last| now - last >= Self::INTERVAL);
        if !forced && !elapsed_ok {
            return false;
        }
        self.forced = false;
        self.last = Some(now);
        true
    }
}

mod harness {
    use crossbeam_channel::{Receiver, Sender, bounded};
    use parking_lot::Mutex;
    use std::sync::Once;

    use crate::types::{PlaybackEvent, PlaybackEventKind};

    #[derive(Copy, Clone, PartialEq, Eq)]
    pub enum Phase {
        Playing,
        Paused,
        Stopped,
    }

    pub fn map_kind_to_phase(kind: PlaybackEventKind) -> Phase {
        match kind {
            PlaybackEventKind::Started => Phase::Playing,
            PlaybackEventKind::Paused | PlaybackEventKind::TrackLoaded => Phase::Paused,
            PlaybackEventKind::Finished
            | PlaybackEventKind::Canceled
            | PlaybackEventKind::Error => Phase::Stopped,
            _ => Phase::Stopped,
        }
    }

    pub trait QueuedSink {
        fn init(&mut self);
        fn deliver(&mut self, ev: &PlaybackEvent);
        fn teardown(&mut self);
    }

    const EVENT_QUEUE_CAP: usize = 256;

    static EVENT_TX: Mutex<Option<Sender<PlaybackEvent>>> = Mutex::new(None);

    static REGISTER_SINK: Once = Once::new();

    fn on_event(ev: &PlaybackEvent) {
        if let Some(tx) = EVENT_TX.lock().as_ref() {
            let _ = tx.try_send(ev.clone());
        }
    }

    pub fn run_sink<S, F>(thread_name: &str, build: F)
    where
        S: QueuedSink,
        F: FnOnce() -> S + Send + 'static,
    {
        let (tx, rx) = bounded(EVENT_QUEUE_CAP);
        {
            let mut slot = EVENT_TX.lock();
            if slot.is_some() {
                return;
            }
            *slot = Some(tx);
        }

        REGISTER_SINK.call_once(|| crate::ffi::register_event_sink(Box::new(on_event)));

        if let Err(e) = std::thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || consumer_thread(rx, build))
        {
            drop(EVENT_TX.lock().take());
            eprintln!("[playback] failed to spawn media-sink thread: {e}");
        }
    }

    pub fn stop() {
        drop(EVENT_TX.lock().take());
    }

    fn consumer_thread<S: QueuedSink>(rx: Receiver<PlaybackEvent>, build: impl FnOnce() -> S) {
        let mut sink = build();
        sink.init();

        while let Ok(ev) = rx.recv() {
            sink.deliver(&ev);
            for ev in rx.try_iter() {
                sink.deliver(&ev);
            }
        }

        sink.teardown();
    }
}

pub use harness::{Phase, QueuedSink, map_kind_to_phase, run_sink, stop};
