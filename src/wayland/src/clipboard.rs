//! Wayland clipboard (CLIPBOARD selection) read path via ext-data-control-v1.
//!
//! Why not wl_data_device on the main display: wl_data_device is focus-bound,
//! and the main jellyfin wl_display competes with XWayland's clipboard bridge
//! on the same seat which CEF (running as an X11 ozone client) relies on for
//! Ctrl+V. ext-data-control-v1 is focus-independent, designed for clipboard
//! managers. Mirrors mpv's clipboard-wayland.c: dedicated wl_display_connect,
//! dedicated worker thread, no shared globals with the main display.

use nix::fcntl::OFlag;
use parking_lot::Mutex;
use std::io::{ErrorKind, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use calloop::generic::Generic;
use calloop::ping::PingSource;
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, Readiness};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_seat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self as dc_device, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self as dc_offer, ExtDataControlOfferV1},
};

const MIME_TEXT_PLAIN_UTF8: &str = "text/plain;charset=utf-8";
const MIME_TEXT_PLAIN: &str = "text/plain";
const MIME_UTF8_STRING: &str = "UTF8_STRING";
const MIME_STRING: &str = "STRING";
const MIME_TEXT: &str = "TEXT";

#[derive(Default, Clone)]
struct OfferMimes {
    text_plain_utf8: bool,
    text_plain: bool,
    utf8_string: bool,
    string: bool,
    text: bool,
}

impl OfferMimes {
    fn best(&self) -> Option<&'static str> {
        if self.text_plain_utf8 {
            Some(MIME_TEXT_PLAIN_UTF8)
        } else if self.text_plain {
            Some(MIME_TEXT_PLAIN)
        } else if self.utf8_string {
            Some(MIME_UTF8_STRING)
        } else if self.string {
            Some(MIME_STRING)
        } else if self.text {
            Some(MIME_TEXT)
        } else {
            None
        }
    }
    fn observe(&mut self, mime: &str) {
        match mime {
            MIME_TEXT_PLAIN_UTF8 => self.text_plain_utf8 = true,
            MIME_TEXT_PLAIN => self.text_plain = true,
            MIME_UTF8_STRING => self.utf8_string = true,
            MIME_STRING => self.string = true,
            MIME_TEXT => self.text = true,
            _ => {}
        }
    }
}

struct PendingCb {
    cb: Box<dyn FnOnce(&str) + Send>,
}

struct Shared {
    queued: Mutex<Vec<PendingCb>>,
    stop: AtomicBool,
    ping: calloop::ping::Ping,
}

struct State {
    registry_state: RegistryState,
    // Held to keep the Wayland proxies alive for the lifetime of the worker.
    #[allow(dead_code)]
    seat: Option<wl_seat::WlSeat>,
    #[allow(dead_code)]
    mgr: Option<ExtDataControlManagerV1>,
    device: Option<ExtDataControlDeviceV1>,
    // Pending offers keyed by offer object id: the proxy plus its mime set,
    // built up between data_offer and the selection event that takes it.
    // The proxy is held so unclaimed offers can be destroyed — dropping a
    // wayland-client handle does not send the wire `destroy`, so without
    // this the compositor-side offer objects leak.
    pending_offers: std::collections::HashMap<u32, (ExtDataControlOfferV1, OfferMimes)>,
    // Currently active selection offer + its mime set.
    current_offer: Option<(ExtDataControlOfferV1, OfferMimes)>,
}

pub struct JfnClipboardWayland {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![];
}

delegate_registry!(State);

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: dc_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            dc_device::Event::DataOffer { id } => {
                let key = id.id().protocol_id();
                state
                    .pending_offers
                    .insert(key, (id, OfferMimes::default()));
            }
            dc_device::Event::Selection { id } => {
                if let Some((prev, _)) = state.current_offer.take() {
                    prev.destroy();
                }
                let claimed = id.as_ref().map(|o| o.id().protocol_id());
                // Destroy every pending offer except the one being claimed.
                state.pending_offers.retain(|&k, (proxy, _)| {
                    if Some(k) == claimed {
                        true
                    } else {
                        proxy.destroy();
                        false
                    }
                });
                if let Some(offer) = id {
                    let key = offer.id().protocol_id();
                    match state.pending_offers.remove(&key) {
                        // Keep the stored proxy; the event's handle is a
                        // duplicate reference to the same object — drop it.
                        Some((proxy, mimes)) => {
                            drop(offer);
                            state.current_offer = Some((proxy, mimes));
                        }
                        None => state.current_offer = Some((offer, OfferMimes::default())),
                    }
                }
            }
            dc_device::Event::Finished => {
                for (_, (proxy, _)) in state.pending_offers.drain() {
                    proxy.destroy();
                }
                if let Some((cur, _)) = state.current_offer.take() {
                    cur.destroy();
                }
                if let Some(dev) = state.device.take() {
                    dev.destroy();
                }
            }
            dc_device::Event::PrimarySelection { id: Some(offer) } => {
                // Primary selection unused — destroy the offer. Use the
                // stored proxy if present so we don't leave a stale handle.
                match state.pending_offers.remove(&offer.id().protocol_id()) {
                    Some((proxy, _)) => {
                        drop(offer);
                        proxy.destroy();
                    }
                    None => offer.destroy(),
                }
            }
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        dc_device::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
        dc_device::EVT_PRIMARY_SELECTION_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: dc_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let dc_offer::Event::Offer { mime_type } = event {
            let key = offer.id().protocol_id();
            if let Some((_, mimes)) = state.pending_offers.get_mut(&key) {
                mimes.observe(&mime_type);
            } else if let Some((cur, mimes)) = state.current_offer.as_mut()
                && cur.id().protocol_id() == key
            {
                mimes.observe(&mime_type);
            }
        }
    }
}

fn fire(pending: PendingCb, text: &[u8]) {
    let s = std::str::from_utf8(text).unwrap_or("");
    (pending.cb)(s);
}

fn start_receive(state: &mut State, conn: &Connection) -> Option<OwnedFd> {
    let (offer, mimes) = state.current_offer.as_ref()?;
    let mime = mimes.best()?;
    let (read_end, write_end) = nix::unistd::pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).ok()?;
    offer.receive(mime.to_owned(), write_end.as_fd());
    let _ = conn.flush();
    drop(write_end);
    Some(read_end)
}

struct Worker {
    shared: Arc<Shared>,
    conn: Connection,
    state: State,
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
        let Some(fd) = start_receive(&mut self.state, &self.conn) else {
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

fn run_clipboard_loop(
    shared: Arc<Shared>,
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
    wake: PingSource,
) {
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
        conn: conn.clone(),
        state,
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
    let inserted = handle.insert_source(
        WaylandSource::new(conn, queue),
        |_, queue, worker: &mut Worker| queue.dispatch_pending(&mut worker.state),
    );
    if let Err(e) = inserted {
        tracing::error!(target: "Main", "clipboard: wayland source: {e}");
        worker.drain_pending();
        return;
    }
    // `run` calls its callback only after a dispatch, so promote once here or a
    // request queued before the loop started would wait for the first event.
    worker.promote_next();
    if let Err(e) = event_loop.run(None, &mut worker, Worker::promote_next) {
        tracing::error!(target: "Main", "clipboard: event loop: {e}");
    }
    worker.drain_pending();
}

fn init_impl() -> Option<JfnClipboardWayland> {
    let conn = Connection::connect_to_env().ok()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).ok()?;
    let qh = queue.handle();

    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=8, ()).ok()?;
    let mgr: ExtDataControlManagerV1 = globals.bind(&qh, 1..=1, ()).ok()?;
    let device = mgr.get_data_device(&seat, &qh, ());

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        seat: Some(seat),
        mgr: Some(mgr),
        device: Some(device),
        pending_offers: Default::default(),
        current_offer: None,
    };
    queue.roundtrip(&mut state).ok()?;

    let (ping, wake) = calloop::ping::make_ping().ok()?;
    let shared = Arc::new(Shared {
        queued: Mutex::new(Vec::new()),
        stop: AtomicBool::new(false),
        ping,
    });
    let shared_w = shared.clone();
    let worker = thread::spawn(move || run_clipboard_loop(shared_w, conn, queue, state, wake));
    Some(JfnClipboardWayland {
        shared,
        worker: Some(worker),
    })
}

/// The wayland lifecycle drives init/cleanup; the read path goes through the
/// runtime's slot.
pub struct Clipboard {
    inner: Mutex<Option<Box<JfnClipboardWayland>>>,
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
        if let Some(c) = init_impl() {
            *g = Some(Box::new(c));
        }
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
        let Some(mut boxed) = self.inner.lock().take() else {
            return;
        };
        boxed.shared.stop.store(true, Ordering::Relaxed);
        boxed.shared.ping.ping();
        // The worker drains every still-queued callback after its loop returns,
        // so joining here is what guarantees each one ran exactly once.
        if let Some(w) = boxed.worker.take() {
            let _ = w.join();
        }
    }
}
