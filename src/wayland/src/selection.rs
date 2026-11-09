//! Both selections on the app's own seat, served by the input thread.
//!
//! The process holds one Wayland connection for its own display and opens
//! none for the clipboard: the seat's `wl_data_device` carries the clipboard,
//! and its `zwp_primary_selection_device_v1` the primary selection where the
//! compositor advertises the manager.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

use calloop::generic::Generic;
use calloop::ping::{Ping, PingSource};
use calloop::{Interest, Mode, PostAction, Readiness};
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use parking_lot::Mutex;

use smithay_client_toolkit::data_device_manager::DataDeviceManagerState;
use smithay_client_toolkit::data_device_manager::WritePipe;
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::{
    DataOfferHandler, DragOffer, SelectionOffer,
};
use smithay_client_toolkit::data_device_manager::data_source::{
    CopyPasteSource, DataSourceHandler,
};
use smithay_client_toolkit::primary_selection::PrimarySelectionManagerState;
use smithay_client_toolkit::primary_selection::device::{
    PrimarySelectionDevice, PrimarySelectionDeviceHandler,
};
use smithay_client_toolkit::primary_selection::selection::{
    PrimarySelectionSource, PrimarySelectionSourceHandler,
};
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_source::WlDataSource;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1;
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1;

use jfn_platform_abi::OnText;

use crate::input::State;

/// The mime types the source offers, in the order a read prefers them.
const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// The globals whose presence means another client can read this seat's
/// selections without holding focus.
const DATA_CONTROL_GLOBALS: [&str; 2] = [
    "zwlr_data_control_manager_v1",
    "ext_data_control_manager_v1",
];

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Clipboard,
    Primary,
}

enum Job {
    Read { kind: Kind, on_done: OnText },
    Write { kind: Kind, text: String },
}

/// The seat's `wl_data_device` and, where the compositor advertises the
/// manager, its `zwp_primary_selection_device_v1`.
pub(crate) struct Selections {
    jobs: Mutex<VecDeque<Job>>,
    ping: Mutex<Option<Ping>>,
    source: Mutex<Option<PingSource>>,
    primary_available: AtomicBool,
    data_control: AtomicBool,
    /// Set by [`Selections::cleanup`]; a read queued afterwards resolves with
    /// no text rather than waiting for a thread that is gone.
    closed: AtomicBool,
}

impl Selections {
    pub(crate) fn new() -> Selections {
        let (ping, source) = match calloop::ping::make_ping() {
            Ok(pair) => (Some(pair.0), Some(pair.1)),
            Err(e) => {
                tracing::error!(target: "Main", "selection: ping: {e}");
                (None, None)
            }
        };
        Selections {
            jobs: Mutex::new(VecDeque::new()),
            ping: Mutex::new(ping),
            source: Mutex::new(source),
            primary_available: AtomicBool::new(false),
            data_control: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    /// Taken once by the input thread, which serves the queue from its loop.
    pub(crate) fn take_source(&self) -> Option<PingSource> {
        self.source.lock().take()
    }

    /// Queued to the input thread; `on_done` fires there, with `None` for a
    /// selection that holds no text and for a receive that failed.
    pub(crate) fn read_text_async(&self, kind: Kind, on_done: OnText) {
        let queued = self.queue(Job::Read { kind, on_done });
        if let Some(Job::Read { on_done, .. }) = queued {
            on_done(None);
        }
    }

    /// Queued to the input thread, which offers
    /// `text/plain;charset=utf-8`, `text/plain`, `UTF8_STRING`, `STRING` and
    /// `TEXT`, citing the seat's last input serial.
    pub(crate) fn write_text(&self, kind: Kind, text: &str) {
        drop(self.queue(Job::Write {
            kind,
            text: text.to_owned(),
        }));
    }

    /// Whether the compositor advertised
    /// `zwp_primary_selection_device_manager_v1`.
    pub(crate) fn primary_available(&self) -> bool {
        self.primary_available.load(Ordering::Acquire)
    }

    /// Whether the compositor advertised `wlr-data-control-unstable-v1` or
    /// `ext-data-control-v1`, read off the app's own registry.
    pub(crate) fn data_control_advertised(&self) -> bool {
        self.data_control.load(Ordering::Acquire)
    }

    /// Resolves every queued read with no text.
    pub(crate) fn cleanup(&self) {
        self.closed.store(true, Ordering::Release);
        let jobs: Vec<Job> = self.jobs.lock().drain(..).collect();
        for job in jobs {
            if let Job::Read { on_done, .. } = job {
                on_done(None);
            }
        }
    }

    /// Queues `job` and wakes the input thread. Returns the job back when
    /// there is no thread to serve it.
    fn queue(&self, job: Job) -> Option<Job> {
        if self.closed.load(Ordering::Acquire) {
            return Some(job);
        }
        let ping = self.ping.lock().clone();
        let Some(ping) = ping else {
            return Some(job);
        };
        self.jobs.lock().push_back(job);
        ping.ping();
        None
    }

    fn take_jobs(&self) -> Vec<Job> {
        self.jobs.lock().drain(..).collect()
    }

    fn note_globals(&self, globals: &GlobalList) {
        let data_control = globals.contents().with_list(|list| {
            list.iter()
                .any(|global| DATA_CONTROL_GLOBALS.contains(&global.interface.as_str()))
        });
        self.data_control.store(data_control, Ordering::Release);
    }
}

/// The input thread's half: the devices, the sources this client owns, and the
/// reads still draining their pipes.
pub(crate) struct SelectionState {
    manager: Option<DataDeviceManagerState>,
    device: Option<DataDevice>,
    primary_manager: Option<PrimarySelectionManagerState>,
    primary_device: Option<PrimarySelectionDevice>,
    clipboard_source: Option<CopyPasteSource>,
    clipboard_text: String,
    primary_source: Option<PrimarySelectionSource>,
    primary_text: String,
    reads: Vec<Read_>,
    next_read: u64,
    writes: Vec<Write_>,
    next_write: u64,
}

struct Read_ {
    token: u64,
    on_done: Option<OnText>,
    buffer: Vec<u8>,
}

/// One selection value still draining into a requestor's pipe.
struct Write_ {
    token: u64,
    data: Vec<u8>,
    written: usize,
}

impl SelectionState {
    /// Binds both managers on the registry the input thread already opened and
    /// gets this seat's devices.
    pub(crate) fn bind(
        selections: &'static Selections,
        globals: &GlobalList,
        qh: &QueueHandle<State>,
        seat: &WlSeat,
    ) -> SelectionState {
        selections.note_globals(globals);
        let manager = DataDeviceManagerState::bind(globals, qh)
            .inspect_err(
                |e| tracing::info!(target: "Main", "selection: wl_data_device_manager: {e}"),
            )
            .ok();
        let device = manager
            .as_ref()
            .map(|manager| manager.get_data_device(qh, seat));
        let primary_manager = PrimarySelectionManagerState::bind(globals, qh).ok();
        selections
            .primary_available
            .store(primary_manager.is_some(), Ordering::Release);
        let primary_device = primary_manager
            .as_ref()
            .map(|manager| manager.get_selection_device(qh, seat));
        SelectionState {
            manager,
            device,
            primary_manager,
            primary_device,
            clipboard_source: None,
            clipboard_text: String::new(),
            primary_source: None,
            primary_text: String::new(),
            reads: Vec::new(),
            next_read: 0,
            writes: Vec::new(),
            next_write: 0,
        }
    }

    fn offered(&self, source: &WlDataSource) -> Option<&str> {
        let owned = self.clipboard_source.as_ref()?;
        (owned.inner() == source).then_some(self.clipboard_text.as_str())
    }

    fn primary_offered(&self, source: &ZwpPrimarySelectionSourceV1) -> Option<&str> {
        let owned = self.primary_source.as_ref()?;
        (owned.inner() == source).then_some(self.primary_text.as_str())
    }
}

impl State {
    /// Serves every job the other threads queued. Runs on the input thread.
    pub(crate) fn serve_selections(&mut self, qh: &QueueHandle<State>) {
        for job in self.rt.selections().take_jobs() {
            match job {
                Job::Read { kind, on_done } => self.start_read(kind, on_done),
                Job::Write { kind, text } => self.take_selection(qh, kind, &text),
            }
        }
    }

    fn take_selection(&mut self, qh: &QueueHandle<State>, kind: Kind, text: &str) {
        let serial = self.rt.seat().last_input_serial();
        match kind {
            Kind::Clipboard => {
                let (Some(manager), Some(device)) = (
                    self.selection.manager.as_ref(),
                    self.selection.device.as_ref(),
                ) else {
                    return;
                };
                let source = manager.create_copy_paste_source(qh, TEXT_MIMES);
                source.set_selection(device, serial);
                self.selection.clipboard_text = text.to_owned();
                self.selection.clipboard_source = Some(source);
            }
            Kind::Primary => {
                let (Some(manager), Some(device)) = (
                    self.selection.primary_manager.as_ref(),
                    self.selection.primary_device.as_ref(),
                ) else {
                    return;
                };
                let source = manager.create_selection_source(qh, TEXT_MIMES);
                source.set_selection(device, serial);
                self.selection.primary_text = text.to_owned();
                self.selection.primary_source = Some(source);
            }
        }
    }

    fn start_read(&mut self, kind: Kind, on_done: OnText) {
        let pipe = match kind {
            Kind::Clipboard => self
                .selection
                .device
                .as_ref()
                .and_then(|device| device.data().selection_offer())
                .and_then(|offer| receive_clipboard(&offer)),
            Kind::Primary => self
                .selection
                .primary_device
                .as_ref()
                .and_then(|device| device.data().selection_offer())
                .and_then(|offer| {
                    let mime = offer.with_mime_types(preferred_mime)?;
                    offer.receive(mime).ok()
                })
                .map(OwnedFd::from),
        };
        let Some(fd) = pipe else {
            on_done(None);
            return;
        };
        if fcntl(&fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).is_err() {
            on_done(None);
            return;
        }
        let Some(handle) = self.loop_handle.clone() else {
            on_done(None);
            return;
        };
        let token = self.selection.next_read;
        self.selection.next_read += 1;
        let inserted = handle.insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            move |readiness, fd, state: &mut State| {
                Ok(state.read_ready(token, readiness, fd.as_fd()))
            },
        );
        if let Err(e) = inserted {
            tracing::warn!(target: "Main", "selection: read source: {e}");
            on_done(None);
            return;
        }
        self.selection.reads.push(Read_ {
            token,
            on_done: Some(on_done),
            buffer: Vec::new(),
        });
    }

    fn read_ready(&mut self, token: u64, readiness: Readiness, fd: BorrowedFd<'_>) -> PostAction {
        let Some(index) = self
            .selection
            .reads
            .iter()
            .position(|read| read.token == token)
        else {
            return PostAction::Remove;
        };
        let mut done = readiness.error;
        if readiness.readable {
            let mut chunk = [0u8; 4096];
            let mut file = std::mem::ManuallyDrop::new(unsafe {
                <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(
                    std::os::fd::AsRawFd::as_raw_fd(&fd),
                )
            });
            loop {
                match file.read(&mut chunk) {
                    Ok(0) => {
                        done = true;
                        break;
                    }
                    Ok(n) => self.selection.reads[index]
                        .buffer
                        .extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => {
                        done = true;
                        break;
                    }
                }
            }
        }
        if !done {
            return PostAction::Continue;
        }
        let read = self.selection.reads.remove(index);
        resolve(read);
        PostAction::Remove
    }

    /// Queues `text` for the requestor's pipe and serves it from the event
    /// loop; the input thread never blocks on a requestor that is not reading,
    /// and a paste of this process's own selection drains its own pipe.
    fn serve_text(&mut self, text: &str, pipe: WritePipe) {
        let fd = OwnedFd::from(pipe);
        if fcntl(&fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).is_err() {
            return;
        }
        let Some(handle) = self.loop_handle.clone() else {
            return;
        };
        let token = self.selection.next_write;
        self.selection.next_write += 1;
        let inserted = handle.insert_source(
            Generic::new(fd, Interest::WRITE, Mode::Level),
            move |readiness, fd, state: &mut State| {
                Ok(state.write_ready(token, readiness, fd.as_fd()))
            },
        );
        if let Err(e) = inserted {
            tracing::warn!(target: "Main", "selection: write source: {e}");
            return;
        }
        self.selection.writes.push(Write_ {
            token,
            data: text.as_bytes().to_vec(),
            written: 0,
        });
    }

    /// Writes what the pipe will take. The source goes away once the value is
    /// delivered, the requestor is gone, or the write failed, and closing it is
    /// the end-of-value the requestor reads.
    fn write_ready(&mut self, token: u64, readiness: Readiness, fd: BorrowedFd<'_>) -> PostAction {
        let Some(index) = self
            .selection
            .writes
            .iter()
            .position(|write| write.token == token)
        else {
            return PostAction::Remove;
        };
        let mut done = readiness.error;
        if readiness.writable {
            let mut file = std::mem::ManuallyDrop::new(unsafe {
                <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(
                    std::os::fd::AsRawFd::as_raw_fd(&fd),
                )
            });
            loop {
                let write = &mut self.selection.writes[index];
                let Some(rest) = write.data.get(write.written..).filter(|r| !r.is_empty()) else {
                    done = true;
                    break;
                };
                match file.write(rest) {
                    Ok(0) => {
                        done = true;
                        break;
                    }
                    Ok(n) => write.written += n,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => {
                        done = true;
                        break;
                    }
                }
            }
        }
        if !done {
            return PostAction::Continue;
        }
        drop(self.selection.writes.remove(index));
        PostAction::Remove
    }

    /// Resolves every read still draining a pipe, for a thread that is
    /// stopping.
    pub(crate) fn drain_selection_reads(&mut self) {
        for read in self.selection.reads.drain(..) {
            if let Some(on_done) = read.on_done {
                on_done(None);
            }
        }
    }
}

fn resolve(mut read: Read_) {
    let Some(on_done) = read.on_done.take() else {
        return;
    };
    match std::str::from_utf8(&read.buffer) {
        Ok("") | Err(_) => on_done(None),
        Ok(text) => on_done(Some(text)),
    }
}

fn receive_clipboard(offer: &SelectionOffer) -> Option<OwnedFd> {
    let mime = offer.with_mime_types(preferred_mime)?;
    offer.receive(mime).ok().map(OwnedFd::from)
}

/// The first offered mime type this client understands, in preference order.
fn preferred_mime(offered: &[String]) -> Option<String> {
    TEXT_MIMES
        .into_iter()
        .find(|mime| offered.iter().any(|o| o == mime))
        .map(str::to_owned)
}

impl DataDeviceHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataDevice,
        _: f64,
        _: f64,
        _: &WlSurface,
    ) {
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataOfferHandler for State {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }
}

impl DataSourceHandler for State {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        _: String,
        fd: WritePipe,
    ) {
        let Some(text) = self.selection.offered(source).map(str::to_owned) else {
            return;
        };
        self.serve_text(&text, fd);
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        if self.selection.offered(source).is_some() {
            self.selection.clipboard_source = None;
            self.selection.clipboard_text.clear();
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }
}

impl PrimarySelectionDeviceHandler for State {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionDeviceV1,
    ) {
    }
}

impl PrimarySelectionSourceHandler for State {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
        _: String,
        pipe: WritePipe,
    ) {
        let Some(text) = self.selection.primary_offered(source).map(str::to_owned) else {
            return;
        };
        self.serve_text(&text, pipe);
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
    ) {
        if self.selection.primary_offered(source).is_some() {
            self.selection.primary_source = None;
            self.selection.primary_text.clear();
        }
    }
}

/// The primary selection the platform hands out, `Some` only where the
/// manager is advertised.
pub(crate) struct WlPrimary {
    pub(crate) rt: &'static crate::runtime::WlRuntime,
}

impl jfn_platform_abi::PrimarySelection for WlPrimary {
    fn read_text_async(&self, on_done: OnText) {
        self.rt.selections().read_text_async(Kind::Primary, on_done);
    }

    fn write_text(&self, text: &str) {
        self.rt.selections().write_text(Kind::Primary, text);
    }
}
