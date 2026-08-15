use std::cell::RefCell;
use std::ffi::CString;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::atomic::Ordering;

use crossbeam_channel::Sender;
use error_reporter::Report;
use wl_proxy::baseline::Baseline;
use wl_proxy::client::Client;
use wl_proxy::object::{ConcreteObject, Object, ObjectCoreApi, ObjectError, ObjectRcUtils};
use wl_proxy::protocols::ObjectInterface;
use wl_proxy::protocols::fractional_scale_v1::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wl_proxy::protocols::wayland::wl_callback::{WlCallback, WlCallbackHandler};
use wl_proxy::protocols::wayland::wl_compositor::WlCompositor;
use wl_proxy::protocols::wayland::wl_display::{WlDisplay, WlDisplayHandler};
use wl_proxy::protocols::wayland::wl_keyboard::{
    WlKeyboard, WlKeyboardHandler, WlKeyboardKeyState,
};
use wl_proxy::protocols::wayland::wl_pointer::{WlPointer, WlPointerButtonState, WlPointerHandler};
use wl_proxy::protocols::wayland::wl_region::WlRegion;
use wl_proxy::protocols::wayland::wl_registry::{WlRegistry, WlRegistryHandler};
use wl_proxy::protocols::wayland::wl_seat::{WlSeat, WlSeatHandler};
use wl_proxy::protocols::wayland::wl_subcompositor::WlSubcompositor;
use wl_proxy::protocols::wayland::wl_subsurface::WlSubsurface;
use wl_proxy::protocols::wayland::wl_surface::{WlSurface, WlSurfaceHandler};
use wl_proxy::protocols::wayland::wl_touch::WlTouch;
use wl_proxy::protocols::xdg_shell::xdg_surface::{XdgSurface, XdgSurfaceHandler};
use wl_proxy::protocols::xdg_shell::xdg_toplevel::XdgToplevel;
use wl_proxy::protocols::xdg_shell::xdg_wm_base::{XdgWmBase, XdgWmBaseHandler};
use wl_proxy::state::State;

use crate::runtime::WlRuntime;

use super::mpv::MpvWorker;
use super::{FracScaleMgrH, NoopClient, log_send};

pub(super) struct AppStartup {
    pub(super) app_fd: OwnedFd,
    pub(super) display_name: CString,
}

struct AppShell {
    display: Option<Rc<WlDisplay>>,
    client: Option<Rc<Client>>,
    compositor: Option<Rc<WlCompositor>>,
    subcompositor: Option<Rc<WlSubcompositor>>,
    wm_base: Option<Rc<XdgWmBase>>,
    globals_ready: bool,
    roundtrip_started: bool,
    host_root_surface: Option<Rc<WlSurface>>,
    host_root_xdg_surface: Option<Rc<XdgSurface>>,
    spliced: bool,
    mpv_client: Option<Rc<Client>>,
    mpv_subsurface: Option<SyncSubsurface>,
}

/// A subsurface kept permanently in Wayland synchronized mode: its buffer,
/// viewport and position apply atomically on the parent surface's commit. The
/// raw object never escapes and no `set_desync` is exposed, so a desynchronized
/// video layer — one that could present a size the window does not have — is
/// unrepresentable.
struct SyncSubsurface(Rc<WlSubsurface>);

impl SyncSubsurface {
    fn create(
        subcompositor: &Rc<WlSubcompositor>,
        surface: &Rc<WlSurface>,
        parent: &Rc<WlSurface>,
    ) -> Result<Self, ObjectError> {
        let sub = subcompositor.create_child::<WlSubsurface>();
        // Born synchronized (the protocol default); never desynced.
        subcompositor.try_send_get_subsurface(&sub, surface, parent)?;
        Ok(Self(sub))
    }

    fn set_position(&self, x: i32, y: i32) -> Result<(), ObjectError> {
        self.0.try_send_set_position(x, y)
    }

    fn place_above(&self, sibling: &Rc<WlSurface>) -> Result<(), ObjectError> {
        self.0.try_send_place_above(sibling)
    }
}

impl AppShell {
    const fn new() -> Self {
        Self {
            display: None,
            client: None,
            compositor: None,
            subcompositor: None,
            wm_base: None,
            globals_ready: false,
            roundtrip_started: false,
            host_root_surface: None,
            host_root_xdg_surface: None,
            spliced: false,
            mpv_client: None,
            mpv_subsurface: None,
        }
    }
}

#[derive(Clone)]
struct AppCtx {
    rt: &'static WlRuntime,
    shell: Rc<RefCell<AppShell>>,
}

impl AppCtx {
    fn new(rt: &'static WlRuntime) -> Self {
        Self {
            rt,
            shell: Rc::new(RefCell::new(AppShell::new())),
        }
    }

    fn with_shell<R>(&self, f: impl FnOnce(&mut AppShell) -> R) -> R {
        f(&mut self.shell.borrow_mut())
    }
}

struct AppMpv {
    ctx: AppCtx,
    client: Option<Rc<Client>>,
    worker: Option<MpvWorker>,
}

impl AppMpv {
    fn new(ctx: AppCtx, client: Rc<Client>, worker: MpvWorker) -> Self {
        Self {
            ctx,
            client: Some(client),
            worker: Some(worker),
        }
    }
}

impl Drop for AppMpv {
    fn drop(&mut self) {
        // The worker exits only once its end of the bridge sees EOF, so every
        // reference to the mpv client has to go before the join below.
        let (shell_client, subsurface) = self
            .ctx
            .with_shell(|sh| (sh.mpv_client.take(), sh.mpv_subsurface.take()));
        if let Some(client) = self.client.as_ref() {
            client.disconnect();
        }
        drop(shell_client);
        drop(subsurface);
        drop(self.client.take());
        if let Some(mut worker) = self.worker.take() {
            worker.join();
        }
    }
}

pub(super) fn run_app_state(
    rt: &'static WlRuntime,
    tx: Sender<Result<AppStartup, String>>,
    upstream: Option<String>,
) {
    let mut builder = State::builder(Baseline::ALL_OF_THEM).with_log_prefix("jfn-app");
    if let Some(name) = &upstream {
        builder = builder.with_server_display_name(name);
    }
    let state = match builder.build() {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(Err(format!("S_app build: {}", Report::new(e))));
            return;
        }
    };

    let (client_a, app_fd) = match state.connect() {
        Ok(ca) => ca,
        Err(e) => {
            let _ = tx.send(Err(format!("S_app connect: {}", Report::new(e))));
            return;
        }
    };
    let ctx = AppCtx::new(rt);
    client_a.set_handler(NoopClient);
    client_a
        .display()
        .set_handler(AppDisplayH { ctx: ctx.clone() });
    ctx.with_shell(|sh| sh.client = Some(client_a.clone()));

    let (mpv_bridge, app_bridge) = match socketpair_cloexec() {
        Ok(pair) => pair,
        Err(e) => {
            let _ = tx.send(Err(format!("S_app mpv bridge: {e}")));
            return;
        }
    };
    let client_m = match state.add_client(&Rc::new(app_bridge)) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Err(format!("S_app add mpv bridge: {}", Report::new(e))));
            return;
        }
    };
    client_m.set_handler(NoopClient);
    client_m.display().set_handler(ForwardDisplayH);
    ctx.with_shell(|sh| sh.mpv_client = Some(client_m.clone()));

    let (mpv_worker, display_name) = match MpvWorker::spawn(rt, mpv_bridge) {
        Ok(worker) => worker,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let _app_mpv = AppMpv::new(ctx.clone(), client_m, mpv_worker);
    if tx
        .send(Ok(AppStartup {
            app_fd,
            display_name,
        }))
        .is_err()
    {
        return;
    }
    drop(tx);

    while state.is_not_destroyed() {
        match state.dispatch(None) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("proxy: S_app dispatch: {}", Report::new(e));
                return;
            }
        }
        ensure_root(&ctx);
        maybe_build_root(&ctx);
        apply_pin_video_bottom(&ctx);
    }
}

fn socketpair_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
    Ok(socketpair(
        AddressFamily::Unix,
        SockType::Stream,
        None,
        SockFlag::SOCK_CLOEXEC,
    )?)
}

struct ForwardDisplayH;
impl WlDisplayHandler for ForwardDisplayH {
    fn handle_get_registry(&mut self, slf: &Rc<WlDisplay>, registry: &Rc<WlRegistry>) {
        log_send(
            "wl_display.get_registry",
            slf.try_send_get_registry(registry),
        );
    }
}

struct AppDisplayH {
    ctx: AppCtx,
}
impl WlDisplayHandler for AppDisplayH {
    fn handle_get_registry(&mut self, slf: &Rc<WlDisplay>, registry: &Rc<WlRegistry>) {
        self.ctx.with_shell(|sh| {
            if sh.display.is_none() {
                sh.display = Some(slf.clone());
            }
        });
        registry.set_handler(AppRegistryH {
            ctx: self.ctx.clone(),
        });
        log_send(
            "wl_display.get_registry",
            slf.try_send_get_registry(registry),
        );
    }
}

struct AppRegistryH {
    ctx: AppCtx,
}
impl WlRegistryHandler for AppRegistryH {
    fn handle_bind(&mut self, slf: &Rc<WlRegistry>, name: u32, id: Rc<dyn Object>) {
        match id.interface() {
            XdgWmBase::INTERFACE => {
                id.downcast::<XdgWmBase>().set_handler(AppWmBaseH {
                    ctx: self.ctx.clone(),
                });
            }
            WpFractionalScaleManagerV1::INTERFACE => {
                id.downcast::<WpFractionalScaleManagerV1>()
                    .set_handler(FracScaleMgrH);
            }
            WlSeat::INTERFACE => {
                id.downcast::<WlSeat>().set_handler(ForwardSeatH);
            }
            _ => {}
        }
        log_send("wl_registry.bind", slf.try_send_bind(name, id));
    }
}

struct ForwardSeatH;
impl WlSeatHandler for ForwardSeatH {
    fn handle_get_pointer(&mut self, slf: &Rc<WlSeat>, id: &Rc<WlPointer>) {
        id.set_handler(PointerH);
        log_send("wl_seat.get_pointer", slf.try_send_get_pointer(id));
    }
    fn handle_get_keyboard(&mut self, slf: &Rc<WlSeat>, id: &Rc<WlKeyboard>) {
        id.set_handler(KeyboardH);
        log_send("wl_seat.get_keyboard", slf.try_send_get_keyboard(id));
    }
    fn handle_get_touch(&mut self, slf: &Rc<WlSeat>, id: &Rc<WlTouch>) {
        log_send("wl_seat.get_touch", slf.try_send_get_touch(id));
    }
}

struct KeyboardH;
impl WlKeyboardHandler for KeyboardH {
    fn handle_key(
        &mut self,
        slf: &Rc<WlKeyboard>,
        serial: u32,
        time: u32,
        key: u32,
        state: WlKeyboardKeyState,
    ) {
        log_send(
            "wl_keyboard.key",
            slf.try_send_key(serial, time, key, state),
        );
    }
}

struct PointerH;
impl WlPointerHandler for PointerH {
    fn handle_button(
        &mut self,
        slf: &Rc<WlPointer>,
        serial: u32,
        time: u32,
        button: u32,
        state: WlPointerButtonState,
    ) {
        log_send(
            "wl_pointer.button",
            slf.try_send_button(serial, time, button, state),
        );
    }
}

struct AppWmBaseH {
    ctx: AppCtx,
}
impl XdgWmBaseHandler for AppWmBaseH {
    fn handle_get_xdg_surface(
        &mut self,
        slf: &Rc<XdgWmBase>,
        id: &Rc<XdgSurface>,
        surface: &Rc<WlSurface>,
    ) {
        id.set_handler(AppXdgSurfaceH {
            ctx: self.ctx.clone(),
            surface: surface.clone(),
        });
        log_send(
            "xdg_wm_base.get_xdg_surface",
            slf.try_send_get_xdg_surface(id, surface),
        );
    }
}

struct AppXdgSurfaceH {
    ctx: AppCtx,
    surface: Rc<WlSurface>,
}
impl XdgSurfaceHandler for AppXdgSurfaceH {
    fn handle_get_toplevel(&mut self, slf: &Rc<XdgSurface>, id: &Rc<XdgToplevel>) {
        tracing::info!(target: "MpvProxy", "get_toplevel: capturing app root surface");
        self.ctx.with_shell(|sh| {
            sh.host_root_surface = Some(self.surface.clone());
            sh.host_root_xdg_surface = Some(slf.clone());
        });
        log_send("xdg_surface.get_toplevel", slf.try_send_get_toplevel(id));
    }
}

fn ensure_root(ctx: &AppCtx) {
    let (started, display) = ctx.with_shell(|sh| (sh.roundtrip_started, sh.display.clone()));
    if started {
        return;
    }
    let Some(display) = display else {
        return;
    };
    ctx.with_shell(|sh| sh.roundtrip_started = true);
    let registry = display.create_child::<WlRegistry>();
    registry.set_handler(ProxyRegistryH { ctx: ctx.clone() });
    if let Err(e) = display.try_send_get_registry(&registry) {
        tracing::error!(target: "MpvProxy", "ensure_root get_registry: {}", Report::new(&e));
    }
    let sync = display.create_child::<WlCallback>();
    sync.set_handler(RoundtripCb { ctx: ctx.clone() });
    if let Err(e) = display.try_send_sync(&sync) {
        tracing::error!(target: "MpvProxy", "ensure_root sync: {}", Report::new(&e));
    }
}

fn maybe_build_root(ctx: &AppCtx) {
    let (ready, spliced, have_host_root) =
        ctx.with_shell(|sh| (sh.globals_ready, sh.spliced, sh.host_root_surface.is_some()));
    if !ready || spliced || !have_host_root {
        return;
    }
    if let Some(mpv) = find_mpv_surface(ctx) {
        splice_mpv_under_host_root(ctx, mpv);
    }
}

/// Serves a pin request raised by the host's stack owner. Before the splice
/// there is nothing to place, so the request is put back and the splice — which
/// pins as part of establishing the subsurface — serves it instead.
fn apply_pin_video_bottom(ctx: &AppCtx) {
    if !ctx.rt.proxy().take_pin_video_bottom() {
        return;
    }
    let pinned = ctx.with_shell(|sh| {
        let (Some(sub), Some(root)) = (sh.mpv_subsurface.as_ref(), sh.host_root_surface.as_ref())
        else {
            return false;
        };
        if let Err(e) = sub.place_above(root) {
            tracing::error!(target: "MpvProxy", "pin video place_above: {}", Report::new(&e));
        }
        // Subsurface placement is parent-double-buffered, so without a commit
        // the new order never applies.
        if let Err(e) = root.try_send_commit() {
            tracing::error!(target: "MpvProxy", "pin video root commit: {}", Report::new(&e));
        }
        true
    });
    if !pinned {
        ctx.rt.proxy().pin_video_bottom();
    }
}

fn find_mpv_surface(ctx: &AppCtx) -> Option<Rc<WlSurface>> {
    let vid = ctx.rt.proxy().mpv_video_surface_id.load(Ordering::Acquire);
    if vid == 0 {
        return None;
    }
    let client = ctx.with_shell(|sh| sh.mpv_client.clone())?;
    let mut objs = Vec::new();
    client.objects(&mut objs);
    objs.into_iter().find_map(|o| {
        let s = o.try_downcast::<WlSurface>()?;
        (s.client_id() == Some(vid)).then_some(s)
    })
}

fn splice_mpv_under_host_root(ctx: &AppCtx, mpv_surface: Rc<WlSurface>) {
    let objs = ctx.with_shell(|sh| {
        if sh.spliced {
            return None;
        }
        Some((
            sh.compositor.clone()?,
            sh.subcompositor.clone()?,
            sh.host_root_surface.clone()?,
        ))
    });
    let Some((compositor, subcompositor, host_root)) = objs else {
        return;
    };

    // Gating call: without the subsurface role nothing below applies, so on
    // failure bail without marking spliced — maybe_build_root retries next tick.
    // The subsurface is born synchronized and is never desynced, so mpv's buffer
    // applies atomically with the host root's geometry.
    let sub = match SyncSubsurface::create(&subcompositor, &mpv_surface, &host_root) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(target: "MpvProxy", "splice get_subsurface: {}", Report::new(&e));
            return;
        }
    };
    if let Err(e) = sub.set_position(0, 0) {
        tracing::error!(target: "MpvProxy", "splice set_position: {}", Report::new(&e));
    }
    // Pin mpv to the bottom of the root's subsurface stack (place_above the
    // parent = lowest sibling position). The CEF overlay is a sibling subsurface
    // on a different client, so creation order can't keep it above the video.
    if let Err(e) = sub.place_above(&host_root) {
        tracing::error!(target: "MpvProxy", "splice place_above: {}", Report::new(&e));
    }

    // mpv's synchronized subsurface only displays when the root commits. Each mpv
    // commit requests a present from the single root-commit owner, so every video
    // frame applies in one transaction with the window's current geometry.
    mpv_surface.set_handler(ChildPresentH { rt: ctx.rt });

    let region = compositor.create_child::<WlRegion>();
    if let Err(e) = compositor.try_send_create_region(&region) {
        tracing::error!(target: "MpvProxy", "splice create_region: {}", Report::new(&e));
    }
    if let Err(e) = mpv_surface.try_send_set_input_region(Some(&region)) {
        tracing::error!(target: "MpvProxy", "splice set_input_region: {}", Report::new(&e));
    }
    if let Err(e) = region.try_send_destroy() {
        tracing::error!(target: "MpvProxy", "splice region destroy: {}", Report::new(&e));
    }

    // Adding the subsurface only takes effect on the parent's next commit; force
    // one now so a late splice (after the app already mapped) still becomes
    // visible.
    if let Err(e) = host_root.try_send_commit() {
        tracing::error!(target: "MpvProxy", "splice root commit: {}", Report::new(&e));
    }

    ctx.with_shell(|sh| {
        sh.mpv_subsurface = Some(sub);
        sh.spliced = true;
    });
    tracing::info!(target: "MpvProxy", "spliced mpv under host-root surface");
}

struct ProxyRegistryH {
    ctx: AppCtx,
}
impl WlRegistryHandler for ProxyRegistryH {
    fn handle_global(
        &mut self,
        slf: &Rc<WlRegistry>,
        name: u32,
        interface: ObjectInterface,
        version: u32,
    ) {
        let state = slf.state();
        match interface {
            WlCompositor::INTERFACE => {
                let o = state.create_object::<WlCompositor>(version.min(6));
                log_send("wl_registry.bind", slf.try_send_bind(name, o.clone()));
                self.ctx.with_shell(|sh| sh.compositor = Some(o));
            }
            WlSubcompositor::INTERFACE => {
                let o = state.create_object::<WlSubcompositor>(version.min(1));
                log_send("wl_registry.bind", slf.try_send_bind(name, o.clone()));
                self.ctx.with_shell(|sh| sh.subcompositor = Some(o));
            }
            XdgWmBase::INTERFACE => {
                let o = state.create_object::<XdgWmBase>(version.min(6));
                o.set_handler(ProxyWmBaseH);
                log_send("wl_registry.bind", slf.try_send_bind(name, o.clone()));
                self.ctx.with_shell(|sh| sh.wm_base = Some(o));
            }
            _ => {}
        }
    }
}

struct ProxyWmBaseH;
impl XdgWmBaseHandler for ProxyWmBaseH {
    fn handle_ping(&mut self, slf: &Rc<XdgWmBase>, serial: u32) {
        // The compositor pings our own wm_base; mpv can't pong it, so we must.
        log_send("xdg_wm_base.pong", slf.try_send_pong(serial));
    }
}

struct RoundtripCb {
    ctx: AppCtx,
}
impl WlCallbackHandler for RoundtripCb {
    fn handle_done(&mut self, _slf: &Rc<WlCallback>, _data: u32) {
        let ok = self.ctx.with_shell(|sh| {
            sh.globals_ready = true;
            sh.compositor.is_some() && sh.subcompositor.is_some() && sh.wm_base.is_some()
        });
        if !ok {
            eprintln!(
                "proxy: missing globals for splice (need compositor, subcompositor, xdg_wm_base)"
            );
        }
    }
}

/// Installed on mpv's video surface: mpv's commit caches its (synchronized)
/// buffer, then a present is requested from the single root-commit owner
/// (`root_window`), which applies it atomically with the window geometry. mpv
/// never commits the root itself — the owner is the sole root committer.
struct ChildPresentH {
    rt: &'static WlRuntime,
}
impl WlSurfaceHandler for ChildPresentH {
    fn handle_commit(&mut self, slf: &Rc<WlSurface>) {
        log_send("wl_surface.commit", slf.try_send_commit());
        self.rt.root().request_present();
    }
}
