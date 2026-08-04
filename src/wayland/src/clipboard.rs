//! Wayland clipboard (CLIPBOARD selection) read path via wl-clipboard-rs.
//!
//! Why not wl_data_device on the main display: wl_data_device is focus-bound,
//! and the main jellyfin wl_display competes with XWayland's clipboard bridge
//! on the same seat which CEF (running as an X11 ozone client) relies on for
//! Ctrl+V. wl-clipboard-rs speaks ext-data-control-v1 (falling back to
//! wlr-data-control-v1), which is focus-independent, and opens its own
//! wl_display per read — no shared globals with the main display.

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use parking_lot::Mutex;
use std::io::{ErrorKind, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use calloop::generic::Generic;
use calloop::ping::PingSource;
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, Readiness};
use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};
use wl_clipboard_rs::utils::is_primary_selection_supported;

struct PendingCb {
    cb: Box<dyn FnOnce(&str) + Send>,
}

struct Shared {
    queued: Mutex<Vec<PendingCb>>,
    stop: AtomicBool,
    ping: calloop::ping::Ping,
}

struct Handle {
    shared: Arc<Shared>,
    thread: JoinHandle<()>,
}

fn fire(pending: PendingCb, text: &[u8]) {
    let s = std::str::from_utf8(text).unwrap_or("");
    (pending.cb)(s);
}

fn start_receive() -> Option<OwnedFd> {
    let (reader, _mime) =
        get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text).ok()?;
    let fd = OwnedFd::from(reader);
    fcntl(&fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).ok()?;
    Some(fd)
}

fn probe_supported() -> bool {
    is_primary_selection_supported().is_ok()
}

struct Worker {
    shared: Arc<Shared>,
    signal: LoopSignal,
    loop_handle: LoopHandle<'static, Worker>,
    active: Option<(PendingCb, Vec<u8>)>,
}

impl Worker {
    fn promote_next(&mut self) {
        if self.active.is_some() {
            return;
        }
        let next = {
            let mut q = self.shared.queued.lock();
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        };
        let Some(cb) = next else {
            return;
        };
        let Some(fd) = start_receive() else {
            fire(cb, &[]);
            self.drain_queued();
            return;
        };
        let inserted = self.loop_handle.insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            |readiness, fd, worker: &mut Worker| Ok(worker.on_receive_ready(readiness, fd.as_fd())),
        );
        if let Err(e) = inserted {
            tracing::warn!(target: "Main", "clipboard: receive source: {e}");
            fire(cb, &[]);
            return;
        }
        self.active = Some((cb, Vec::new()));
    }

    fn on_receive_ready(&mut self, readiness: Readiness, fd: BorrowedFd<'_>) -> PostAction {
        let Some((_, buf)) = self.active.as_mut() else {
            return PostAction::Remove;
        };
        let mut done = readiness.error;
        if readiness.readable {
            let mut tmp = [0u8; 4096];
            let mut file = unsafe { std::fs::File::from_raw_fd(fd.as_raw_fd()) };
            loop {
                match file.read(&mut tmp) {
                    Ok(0) => {
                        done = true;
                        break;
                    }
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => {
                        done = true;
                        break;
                    }
                }
            }
            // Don't let File's drop close the fd — the source's OwnedFd owns it.
            let _ = file.into_raw_fd();
        }
        if !done {
            return PostAction::Continue;
        }
        if let Some((cb, buf)) = self.active.take() {
            fire(cb, &buf);
        }
        PostAction::Remove
    }

    fn drain_pending(&mut self) {
        if let Some((cb, _)) = self.active.take() {
            fire(cb, &[]);
        }
        self.drain_queued();
    }

    fn drain_queued(&mut self) {
        // Drain into a local first so the queue lock isn't held across the
        // callbacks, which may re-enter `read_text_async`.
        let drained: Vec<PendingCb> = std::mem::take(&mut *self.shared.queued.lock());
        for cb in drained {
            fire(cb, &[]);
        }
    }
}

fn run_clipboard_loop(shared: Arc<Shared>, wake: PingSource) {
    let mut event_loop: EventLoop<'static, Worker> = match EventLoop::try_new() {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(target: "Main", "clipboard: event loop: {e}");
            return;
        }
    };
    let handle = event_loop.handle();
    let mut worker = Worker {
        shared,
        signal: event_loop.get_signal(),
        loop_handle: handle.clone(),
        active: None,
    };
    if let Err(e) = handle.insert_source(wake, |(), (), worker: &mut Worker| {
        if worker.shared.stop.load(Ordering::Relaxed) {
            worker.signal.stop();
        }
    }) {
        tracing::error!(target: "Main", "clipboard: wake source: {e}");
        worker.drain_pending();
        return;
    }
    // `run` calls its callback only after a dispatch, so promote once here or a
    // request queued before the loop started would wait for the first wake.
    worker.promote_next();
    if let Err(e) = event_loop.run(None, &mut worker, Worker::promote_next) {
        tracing::error!(target: "Main", "clipboard: event loop: {e}");
    }
    worker.drain_pending();
}

/// The wayland lifecycle drives init/cleanup; the read path goes through the
/// runtime's slot.
pub struct Clipboard {
    inner: Mutex<Option<Handle>>,
}

impl Clipboard {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    pub fn init(&self) {
        let mut g = self.inner.lock();
        if g.is_some() {
            return;
        }
        if !probe_supported() {
            return;
        }
        let Ok((ping, wake)) = calloop::ping::make_ping() else {
            return;
        };
        let shared = Arc::new(Shared {
            queued: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            ping,
        });
        let shared_w = shared.clone();
        let thread = thread::spawn(move || run_clipboard_loop(shared_w, wake));
        *g = Some(Handle { shared, thread });
    }

    pub fn available(&self) -> bool {
        self.inner.lock().is_some()
    }

    pub fn read_text_async(&self, cb: Box<dyn FnOnce(&str) + Send>) {
        let g = self.inner.lock();
        let Some(c) = g.as_ref() else {
            // No clipboard: deliver an empty read so the caller's promise resolves.
            cb("");
            return;
        };
        {
            let mut q = c.shared.queued.lock();
            q.push(PendingCb { cb });
        }
        c.shared.ping.ping();
    }

    pub fn cleanup(&self) {
        let Some(handle) = self.inner.lock().take() else {
            return;
        };
        handle.shared.stop.store(true, Ordering::Relaxed);
        handle.shared.ping.ping();
        // The worker drains every still-queued callback after its loop returns,
        // so joining here is what guarantees each one ran exactly once.
        let _ = handle.thread.join();
    }
}
