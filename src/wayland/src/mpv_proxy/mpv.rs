use std::cell::RefCell;
use std::ffi::{CString, NulError};
use std::io;
use std::mem::size_of;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction};
use crossbeam_channel::{Sender, bounded};
use error_reporter::Report;
use tempfile::TempDir;
use wl_proxy::baseline::Baseline;
use wl_proxy::object::{ConcreteObject, Object, ObjectCoreApi, ObjectRcUtils};
use wl_proxy::protocols::fractional_scale_v1::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wl_proxy::protocols::viewporter::wp_viewport::{WpViewport, WpViewportHandler};
use wl_proxy::protocols::viewporter::wp_viewporter::{WpViewporter, WpViewporterHandler};
use wl_proxy::protocols::wayland::wl_display::{WlDisplay, WlDisplayHandler};
use wl_proxy::protocols::wayland::wl_keyboard::WlKeyboard;
use wl_proxy::protocols::wayland::wl_output::WlOutput;
use wl_proxy::protocols::wayland::wl_pointer::WlPointer;
use wl_proxy::protocols::wayland::wl_registry::{WlRegistry, WlRegistryHandler};
use wl_proxy::protocols::wayland::wl_seat::{WlSeat, WlSeatHandler};
use wl_proxy::protocols::wayland::wl_surface::WlSurface;
use wl_proxy::protocols::wayland::wl_touch::WlTouch;
use wl_proxy::protocols::xdg_shell::xdg_surface::{XdgSurface, XdgSurfaceHandler};
use wl_proxy::protocols::xdg_shell::xdg_toplevel::{
    XdgToplevel, XdgToplevelHandler, XdgToplevelState,
};
use wl_proxy::protocols::xdg_shell::xdg_wm_base::{XdgWmBase, XdgWmBaseHandler};
use wl_proxy::state::{State, StateError};

use crate::runtime::WlRuntime;
use crate::window_state::WindowSize;

use super::socket::ProxySocket;
use super::{FracScaleMgrH, NoopClient, log_send};

pub(super) struct MpvStartup {
    pub(super) display_name: CString,
    pub(super) socket_dir: TempDir,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum MpvWorkerError {
    #[error("mpv thread spawn failed")]
    Spawn(#[source] io::Error),
    #[error("mpv thread panicked")]
    Panicked,
    #[error("mpv thread exited before publishing startup state")]
    StartupUnpublished,
    #[error("S_mpv build: {0}")]
    Build(String),
    #[error("S_mpv socket")]
    Socket(#[source] io::Error),
    #[error("display name has NUL")]
    DisplayName(#[source] NulError),
    #[error("S_mpv ping")]
    Ping(#[source] io::Error),
    #[error("S_mpv event loop")]
    EventLoop(#[source] calloop::Error),
    #[error("S_mpv {name} source")]
    Source {
        name: &'static str,
        #[source]
        source: calloop::Error,
    },
    #[error("S_mpv accept")]
    Accept(#[source] io::Error),
    #[error("S_mpv add client: {0}")]
    AddClient(String),
    #[error("S_mpv before_poll: {0}")]
    BeforePoll(String),
    #[error("S_mpv dispatch: {0}")]
    Dispatch(String),
}

/// `StateError` is not `Send`, so it crosses the thread boundary as its report.
fn render(e: StateError) -> String {
    Report::new(e).to_string()
}

pub(super) struct MpvWorker {
    thread: Option<JoinHandle<Result<(), MpvWorkerError>>>,
}

impl MpvWorker {
    pub(super) fn spawn(
        rt: &'static WlRuntime,
        bridge: OwnedFd,
    ) -> Result<(Self, MpvStartup), MpvWorkerError> {
        let (tx, rx) = bounded::<MpvStartup>(1);
        let thread = thread::Builder::new()
            .name("proxy-mpv".into())
            .spawn(move || run_mpv_state(rt, &tx, bridge))
            .map_err(MpvWorkerError::Spawn)?;
        let mut worker = Self {
            thread: Some(thread),
        };
        // A closed channel means the worker returned early; its result says why.
        match rx.recv() {
            Ok(startup) => Ok((worker, startup)),
            Err(_) => Err(worker
                .join()
                .err()
                .unwrap_or(MpvWorkerError::StartupUnpublished)),
        }
    }

    pub(super) fn join(&mut self) -> Result<(), MpvWorkerError> {
        match self.thread.take() {
            Some(thread) => thread.join().map_err(|_| MpvWorkerError::Panicked)?,
            None => Ok(()),
        }
    }
}

struct MpvShell {
    configurator: Option<MpvConfigurator>,
    serial: u32,
}

/// mpv's xdg objects, present only once mpv has created its toplevel. Holding
/// both together makes "emit a configure without a toplevel" unrepresentable:
/// a configure can only be sent through an `MpvConfigurator`, which exists only
/// after `get_toplevel`.
#[derive(Clone)]
struct MpvConfigurator {
    toplevel: Rc<XdgToplevel>,
    xdg_surface: Rc<XdgSurface>,
}

impl MpvConfigurator {
    fn configure(&self, size: WindowSize, serial: u32, states: &[u8]) {
        if let Err(e) = self.toplevel.try_send_configure(size.w(), size.h(), states) {
            tracing::error!(target: "MpvProxy", "synth toplevel configure: {}", Report::new(&e));
        }
        if let Err(e) = self.xdg_surface.try_send_configure(serial) {
            tracing::error!(target: "MpvProxy", "synth xdg_surface configure: {}", Report::new(&e));
        }
    }
}

impl MpvShell {
    const fn new() -> Self {
        Self {
            configurator: None,
            serial: 0,
        }
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }
}

#[derive(Clone)]
struct MpvCtx {
    rt: &'static WlRuntime,
    shell: Rc<RefCell<MpvShell>>,
}

impl MpvCtx {
    fn new(rt: &'static WlRuntime) -> Self {
        Self {
            rt,
            shell: Rc::new(RefCell::new(MpvShell::new())),
        }
    }

    fn with_shell<R>(&self, f: impl FnOnce(&mut MpvShell) -> R) -> R {
        f(&mut self.shell.borrow_mut())
    }
}

fn run_mpv_state(
    rt: &'static WlRuntime,
    tx: &Sender<MpvStartup>,
    bridge: OwnedFd,
) -> Result<(), MpvWorkerError> {
    let state = State::builder(Baseline::ALL_OF_THEM)
        .with_log_prefix("jfn-mpv")
        .with_server_fd(&Rc::new(bridge))
        .build()
        .map_err(|e| MpvWorkerError::Build(render(e)))?;
    let socket = ProxySocket::bind().map_err(MpvWorkerError::Socket)?;
    let display_name =
        CString::new(socket.path().as_os_str().as_bytes()).map_err(MpvWorkerError::DisplayName)?;
    let ProxySocket {
        listener,
        dir: socket_dir,
    } = socket;
    let ctx = MpvCtx::new(rt);

    let (wake, wake_source) = calloop::ping::make_ping().map_err(MpvWorkerError::Ping)?;
    let mut event_loop = EventLoop::<MpvLoop>::try_new().map_err(MpvWorkerError::EventLoop)?;
    let handle = event_loop.handle();
    handle
        .insert_source(wake_source, |(), (), _: &mut MpvLoop| {})
        .map_err(|e| MpvWorkerError::Source {
            name: "wake",
            source: e.error,
        })?;
    let poll_source = Generic::new(state.poll_fd().clone(), Interest::READ, Mode::Level);
    handle
        .insert_source(poll_source, |_, _, mpv: &mut MpvLoop| {
            let dispatched = mpv.dispatch_available();
            mpv.stop_on_failure(dispatched);
            Ok(PostAction::Continue)
        })
        .map_err(|e| MpvWorkerError::Source {
            name: "poll",
            source: e.error,
        })?;
    let listener_source = Generic::new(listener, Interest::READ, Mode::Level);
    handle
        .insert_source(listener_source, |_, listener, mpv: &mut MpvLoop| {
            let accepted = mpv.accept_clients(listener);
            mpv.stop_on_failure(accepted);
            Ok(PostAction::Continue)
        })
        .map_err(|e| MpvWorkerError::Source {
            name: "listener",
            source: e.error,
        })?;

    *rt.proxy().mpv_wake.lock() = Some(wake);
    struct WakeGuard(&'static WlRuntime);
    impl Drop for WakeGuard {
        fn drop(&mut self) {
            self.0.proxy().mpv_wake.lock().take();
        }
    }
    let _wake_guard = WakeGuard(rt);

    let startup = MpvStartup {
        display_name,
        socket_dir,
    };
    tx.send(startup)
        .map_err(|_| MpvWorkerError::StartupUnpublished)?;

    let mut mpv = MpvLoop {
        state,
        ctx,
        seen_gen: 0,
        signal: event_loop.get_signal(),
        failure: None,
    };
    // `run` calls its callback only after a dispatch, so settle once here or a
    // size published before the loop started would wait for the first event.
    mpv.settle();
    event_loop
        .run(None, &mut mpv, MpvLoop::settle)
        .map_err(MpvWorkerError::EventLoop)?;
    mpv.failure.map_or(Ok(()), Err)
}

struct MpvLoop {
    state: Rc<State>,
    ctx: MpvCtx,
    seen_gen: u32,
    signal: LoopSignal,
    /// calloop logs and drops what a source callback returns, so the failure
    /// that stopped the loop is carried out of `run` here.
    failure: Option<MpvWorkerError>,
}

impl MpvLoop {
    fn stop_on_failure(&mut self, result: Result<(), MpvWorkerError>) {
        if let Err(e) = result {
            self.failure.get_or_insert(e);
            self.signal.stop();
        }
    }

    fn settle(&mut self) {
        // Reconcile before every sleep, not only after dispatch: a size raised
        // before the wake was published delivers no ping, so servicing it here is
        // the only thing that keeps it from sleeping out an otherwise-idle
        // connection's infinite timeout.
        apply_window_size_mpv(&self.ctx, &mut self.seen_gen);
        let prepared = self
            .state
            .before_poll()
            .map_err(|e| MpvWorkerError::BeforePoll(render(e)));
        self.stop_on_failure(prepared);
        if !self.state.is_not_destroyed() {
            self.signal.stop();
        }
    }

    fn accept_clients(&self, listener: &UnixListener) -> Result<(), MpvWorkerError> {
        loop {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(MpvWorkerError::Accept(e)),
            };
            // `add_client` does not invoke `StateHandler::new_client`.
            let client = self
                .state
                .add_client(&Rc::new(OwnedFd::from(stream)))
                .map_err(|e| MpvWorkerError::AddClient(render(e)))?;
            client.set_handler(NoopClient);
            client.display().set_handler(MpvDisplayH {
                ctx: self.ctx.clone(),
            });
        }
    }

    fn dispatch_available(&self) -> Result<(), MpvWorkerError> {
        self.state
            .dispatch_available()
            .map(drop)
            .map_err(|e| MpvWorkerError::Dispatch(render(e)))
    }
}

fn apply_window_size_mpv(ctx: &MpvCtx, seen_gen: &mut u32) {
    let Some(published) = ctx.rt.proxy().window_size_since(*seen_gen) else {
        return;
    };
    // Advance the marker only once a configure is actually emitted; before mpv's
    // toplevel exists this defers (retried each tick), it never guesses.
    if emit_mpv_configure(ctx, published.size) {
        *seen_gen = published.generation;
    }
}

/// MAXIMIZED puts mpv in `locked_size`, so mpv holds the size we hand it instead
/// of re-deriving its geometry from the video.
const LOCKED_STATES: [u8; size_of::<u32>()] = XdgToplevelState::MAXIMIZED.0.to_ne_bytes();

fn emit_mpv_configure(ctx: &MpvCtx, size: WindowSize) -> bool {
    let emit = ctx.with_shell(|sh| {
        let cfg = sh.configurator.clone()?;
        Some((cfg, sh.next_serial()))
    });
    match emit {
        Some((cfg, serial)) => {
            cfg.configure(size, serial, &LOCKED_STATES);
            true
        }
        None => false,
    }
}

struct MpvDisplayH {
    ctx: MpvCtx,
}
impl WlDisplayHandler for MpvDisplayH {
    fn handle_get_registry(&mut self, slf: &Rc<WlDisplay>, registry: &Rc<WlRegistry>) {
        registry.set_handler(MpvRegistryH {
            ctx: self.ctx.clone(),
        });
        log_send(
            "wl_display.get_registry",
            slf.try_send_get_registry(registry),
        );
    }
}

struct MpvRegistryH {
    ctx: MpvCtx,
}
impl WlRegistryHandler for MpvRegistryH {
    fn handle_bind(&mut self, slf: &Rc<WlRegistry>, name: u32, id: Rc<dyn Object>) {
        match id.interface() {
            XdgWmBase::INTERFACE => {
                id.downcast::<XdgWmBase>().set_handler(MpvWmBaseH {
                    ctx: self.ctx.clone(),
                });
            }
            WpFractionalScaleManagerV1::INTERFACE => {
                id.downcast::<WpFractionalScaleManagerV1>()
                    .set_handler(FracScaleMgrH);
            }
            WlSeat::INTERFACE => {
                id.downcast::<WlSeat>().set_handler(BlockSeatH);
            }
            WpViewporter::INTERFACE => {
                id.downcast::<WpViewporter>().set_handler(ClientViewporterH);
            }
            _ => {}
        }
        log_send("wl_registry.bind", slf.try_send_bind(name, id));
    }
}

struct ClientViewporterH;
impl WpViewporterHandler for ClientViewporterH {
    fn handle_get_viewport(
        &mut self,
        slf: &Rc<WpViewporter>,
        id: &Rc<WpViewport>,
        surface: &Rc<WlSurface>,
    ) {
        id.set_handler(ClientViewportH);
        log_send(
            "wp_viewporter.get_viewport",
            slf.try_send_get_viewport(id, surface),
        );
    }
}

struct ClientViewportH;
impl WpViewportHandler for ClientViewportH {
    fn handle_set_destination(&mut self, slf: &Rc<WpViewport>, width: i32, height: i32) {
        // Virtualizing mpv's shell means it can size a viewport before it has a
        // real geometry, emitting a transient set_destination(0,0) — an instant
        // protocol error that would kill the shared connection. Drop non-positive
        // destinations (the unset form is -1,-1); mpv re-sizes once it has
        // geometry from our synthesized configure.
        let unset = width == -1 && height == -1;
        if !unset && (width <= 0 || height <= 0) {
            return;
        }
        // Forward mpv's own destination rect unchanged: mpv is pinned to our
        // window size by the locked-state configure and letterboxes internally,
        // so overriding this rect with the window size stretches the video.
        log_send(
            "wp_viewport.set_destination",
            slf.try_send_set_destination(width, height),
        );
    }
}

struct BlockSeatH;
impl WlSeatHandler for BlockSeatH {
    fn handle_get_pointer(&mut self, _slf: &Rc<WlSeat>, id: &Rc<WlPointer>) {
        id.set_forward_to_server(false);
    }
    fn handle_get_keyboard(&mut self, _slf: &Rc<WlSeat>, id: &Rc<WlKeyboard>) {
        id.set_forward_to_server(false);
    }
    fn handle_get_touch(&mut self, _slf: &Rc<WlSeat>, id: &Rc<WlTouch>) {
        id.set_forward_to_server(false);
    }
}

struct MpvWmBaseH {
    ctx: MpvCtx,
}
impl XdgWmBaseHandler for MpvWmBaseH {
    fn handle_get_xdg_surface(
        &mut self,
        _slf: &Rc<XdgWmBase>,
        id: &Rc<XdgSurface>,
        surface: &Rc<WlSurface>,
    ) {
        if let Some(sid) = surface.server_id() {
            self.ctx
                .rt
                .proxy()
                .mpv_video_surface_id
                .store(sid, Ordering::Release);
        }
        tracing::info!(
            target: "MpvProxy",
            "get_xdg_surface: demoting mpv surface server_id={:?}",
            surface.server_id()
        );
        // mpv's surface must stay role-free upstream so we can give it the
        // subsurface role; never forward get_xdg_surface.
        id.set_forward_to_server(false);
        id.set_handler(MpvSurfaceH {
            ctx: self.ctx.clone(),
        });
    }
}

struct MpvSurfaceH {
    ctx: MpvCtx,
}
impl XdgSurfaceHandler for MpvSurfaceH {
    fn handle_get_toplevel(&mut self, slf: &Rc<XdgSurface>, id: &Rc<XdgToplevel>) {
        id.set_forward_to_server(false);
        id.set_handler(MpvToplevelH {
            ctx: self.ctx.clone(),
        });
        self.ctx.with_shell(|sh| {
            sh.configurator = Some(MpvConfigurator {
                toplevel: id.clone(),
                xdg_surface: slf.clone(),
            });
        });
        // Configure now if the window size is already known (mpv connected after
        // the host was configured — the common case); otherwise the size feed
        // emits once the host publishes. mpv only ever receives real geometry.
        if let Some(size) = self.ctx.rt.proxy().window_size() {
            emit_mpv_configure(&self.ctx, size);
        }
    }
}

/// mpv blocks waiting for a configure in reply to its state-change request (e.g.
/// unset_maximized from VOCTRL_SET_UNFS_WINDOW_SIZE). We never honor the request
/// — the host owns geometry — but must answer it, or mpv stalls on the dropped
/// message.
fn reassert_mpv_state(ctx: &MpvCtx) {
    if let Some(size) = ctx.rt.proxy().window_size() {
        emit_mpv_configure(ctx, size);
    }
}

struct MpvToplevelH {
    ctx: MpvCtx,
}
impl XdgToplevelHandler for MpvToplevelH {
    fn handle_set_maximized(&mut self, _slf: &Rc<XdgToplevel>) {
        reassert_mpv_state(&self.ctx);
    }
    fn handle_unset_maximized(&mut self, _slf: &Rc<XdgToplevel>) {
        reassert_mpv_state(&self.ctx);
    }
    fn handle_set_fullscreen(&mut self, _slf: &Rc<XdgToplevel>, _output: Option<&Rc<WlOutput>>) {
        reassert_mpv_state(&self.ctx);
    }
    fn handle_unset_fullscreen(&mut self, _slf: &Rc<XdgToplevel>) {
        reassert_mpv_state(&self.ctx);
    }
}
