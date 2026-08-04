use std::cell::RefCell;
use std::ffi::CString;
use std::mem::size_of;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction};
use error_reporter::Report;
use wl_proxy::baseline::Baseline;
use wl_proxy::client::Client;
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
use wl_proxy::state::State;

use crate::runtime::WlRuntime;
use crate::window_state::WindowSize;

use super::{FracScaleMgrH, NoopClient, log_send};

pub(super) struct MpvWorker {
    thread: Option<JoinHandle<()>>,
}

impl MpvWorker {
    pub(super) fn spawn(
        rt: &'static WlRuntime,
        bridge: OwnedFd,
    ) -> Result<(Self, CString), String> {
        let (tx, rx) = mpsc::sync_channel::<Result<CString, String>>(1);
        let thread = thread::Builder::new()
            .name("proxy-mpv".into())
            .spawn(move || run_mpv_state(rt, tx, bridge))
            .map_err(|e| format!("mpv thread spawn failed: {e}"))?;

        match rx.recv() {
            Ok(Ok(display_name)) => Ok((
                Self {
                    thread: Some(thread),
                },
                display_name,
            )),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(message)
            }
            Err(_) => {
                let _ = thread.join();
                Err("mpv thread exited before sending display name".to_owned())
            }
        }
    }

    pub(super) fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
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
    tx: mpsc::SyncSender<Result<CString, String>>,
    bridge: OwnedFd,
) {
    let state = match State::builder(Baseline::ALL_OF_THEM)
        .with_log_prefix("jfn-mpv")
        .with_server_fd(&Rc::new(bridge))
        .build()
    {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(Err(format!("S_mpv build: {}", Report::new(e))));
            return;
        }
    };
    let acceptor = match state.create_acceptor(1000) {
        Ok(a) => a,
        Err(e) => {
            let _ = tx.send(Err(format!("S_mpv acceptor: {}", Report::new(e))));
            return;
        }
    };
    let name = match CString::new(acceptor.display()) {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(Err(format!("display name has NUL: {e}")));
            return;
        }
    };
    let ctx = MpvCtx::new(rt);
    state.set_handler(MpvShimStateH { ctx: ctx.clone() });

    let (wake, wake_source) = match calloop::ping::make_ping() {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Err(format!("S_mpv ping: {e}")));
            return;
        }
    };
    let mut event_loop = match EventLoop::<MpvLoop>::try_new() {
        Ok(l) => l,
        Err(e) => {
            let _ = tx.send(Err(format!("S_mpv event loop: {e}")));
            return;
        }
    };
    let handle = event_loop.handle();
    if let Err(e) = handle.insert_source(wake_source, |(), (), _: &mut MpvLoop| {}) {
        let _ = tx.send(Err(format!("S_mpv wake source: {e}")));
        return;
    }
    let poll_source = Generic::new(state.poll_fd().clone(), Interest::READ, Mode::Level);
    let inserted = handle.insert_source(poll_source, |_, _, mpv: &mut MpvLoop| {
        mpv.dispatch_available();
        Ok(PostAction::Continue)
    });
    if let Err(e) = inserted {
        let _ = tx.send(Err(format!("S_mpv poll source: {e}")));
        return;
    }

    *rt.proxy().mpv_wake.lock() = Some(wake);
    struct WakeGuard(&'static WlRuntime);
    impl Drop for WakeGuard {
        fn drop(&mut self) {
            self.0.proxy().mpv_wake.lock().take();
        }
    }
    let _wake_guard = WakeGuard(rt);

    if tx.send(Ok(name)).is_err() {
        return;
    }
    drop(tx);

    let mut mpv = MpvLoop {
        state,
        ctx,
        seen_gen: 0,
        signal: event_loop.get_signal(),
    };
    // `run` calls its callback only after a dispatch, so settle once here or a
    // size published before the loop started would wait for the first event.
    mpv.settle();
    if let Err(e) = event_loop.run(None, &mut mpv, MpvLoop::settle) {
        eprintln!("proxy: S_mpv event loop: {e}");
    }
}

struct MpvLoop {
    state: Rc<State>,
    ctx: MpvCtx,
    seen_gen: u32,
    signal: LoopSignal,
}

impl MpvLoop {
    fn settle(&mut self) {
        // Reconcile before every sleep, not only after dispatch: a size raised
        // before the wake was published delivers no ping, so servicing it here is
        // the only thing that keeps it from sleeping out an otherwise-idle
        // connection's infinite timeout.
        apply_window_size_mpv(&self.ctx, &mut self.seen_gen);
        if let Err(e) = self.state.before_poll() {
            eprintln!("proxy: S_mpv before_poll: {}", Report::new(e));
            self.signal.stop();
            return;
        }
        if !self.state.is_not_destroyed() {
            self.signal.stop();
        }
    }

    fn dispatch_available(&mut self) {
        if let Err(e) = self.state.dispatch_available() {
            eprintln!("proxy: S_mpv dispatch: {}", Report::new(e));
            self.signal.stop();
        }
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

/// Emit a configure to mpv iff its toplevel exists, returning whether one was
/// sent. A configure can only be built from an `MpvConfigurator`, so a
/// toplevel-less emit is unrepresentable rather than guarded.
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

struct MpvShimStateH {
    ctx: MpvCtx,
}
impl wl_proxy::state::StateHandler for MpvShimStateH {
    fn new_client(&mut self, client: &Rc<Client>) {
        client.set_handler(NoopClient);
        client.display().set_handler(MpvDisplayH {
            ctx: self.ctx.clone(),
        });
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
