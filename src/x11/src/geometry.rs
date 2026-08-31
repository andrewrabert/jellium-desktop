//! X11 geometry thread: the sole writer of ALL overlay + video-host structure.
//!
//! It owns every [`StructureSurface`], consumes the [`GeometryCommand`] queue
//! (create/destroy/visibility/restack), and is the sole sizer of the overlays
//! and the video host. It publishes the parent's live geometry as an immutable
//! [`ParentSnapshot`] so all other readers are lock-free.
//!
//! Structure (create/size/place/map/restack) runs on the geometry connection;
//! content (pixel upload) runs on the content connection inside each surface's
//! [`crate::overlay_actor::OverlayActor`]. No overlay window ever has two
//! writers.

use std::collections::HashMap;
use std::sync::Arc;

use calloop::{EventLoop, LoopSignal};
use parking_lot::Mutex;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::shape::{ConnectionExt as _, SK as ShapeSk, SO as ShapeSo};
use x11rb::protocol::sync::{ConnectionExt as _, Int64};
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageData, ClientMessageEvent, ClipOrdering,
    ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, PropMode,
    Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use jfn_platform_abi::Visibility;
use jfn_playback::shutdown::jfn_shutdown_initiate;
use jfn_wake_event::{Drain, WakeEvent, WakeSource};

use crate::conn_source::X11Source;
use crate::input::x11_shutdown_waker;
use crate::overlay_fsm::{self, Effect, Geom, OverlayState};
use crate::registry::{
    GeometryCommand, StructureSurface, SurfaceId, drain_commands, publish_applied, registry,
    split_capabilities,
};
use crate::x11_state::{HostServices, PaintServices, ParentSnapshot};

pub struct Handle {
    join: Option<std::thread::JoinHandle<()>>,
}

impl Handle {
    pub fn join(&mut self) {
        if let Some(ev) = x11_shutdown_waker() {
            ev.signal();
        }
        if let Some(j) = self.join.take()
            && let Err(e) = j.join()
        {
            eprintln!("[x11] geometry thread panicked: {e:?}");
        }
    }
}

static G: Mutex<Option<Handle>> = Mutex::new(None);

/// Keeps the top-level's connection open past the geometry thread's exit: the
/// server destroys the top-level and all its children — including mpv's
/// embedded sub-window — the moment this connection closes, so it must outlive
/// `mpv_terminate_destroy`. Dropped in `post_window_cleanup`.
static CONN_HOLD: Mutex<Option<Arc<RustConnection>>> = Mutex::new(None);

pub fn drop_toplevel_connection() {
    *CONN_HOLD.lock() = None;
}

/// The geometry thread's wake source for command-queue drains and re-mirrors.
fn x11_geometry_resync_waker() -> Option<&'static WakeEvent> {
    use std::sync::OnceLock;
    static EV: OnceLock<Option<&'static WakeEvent>> = OnceLock::new();
    *EV.get_or_init(|| Some(Box::leak(Box::new(WakeEvent::new()?))))
}

pub fn request_resync() {
    if let Some(ev) = x11_geometry_resync_waker() {
        ev.signal();
    }
}

/// App-side fullscreen setter: mirror the requested state onto the WM-managed
/// top-level, then trigger a reconcile.
pub fn set_parent_fullscreen(fs: bool) {
    apply_toplevel_fullscreen(fs);
    request_resync();
}

/// Paints `rgb` behind the app window and the video plane and clears both, so
/// the strip a resize exposes carries the theme colour rather than the stale
/// texture beneath it.
///
/// Both windows are created at the root's depth, so `rgb` is the pixel value
/// directly.
pub fn set_theme_color(rgb: u32) {
    let Some(conn) = crate::x11_state::x11rb_conn() else {
        return;
    };
    let Some(host) = crate::x11_state::host() else {
        return;
    };
    let aux = ChangeWindowAttributesAux::new().background_pixel(rgb & 0x00FF_FFFF);
    for window in [host.toplevel, host.video_host] {
        if window == 0 {
            continue;
        }
        let _ = conn.change_window_attributes(window, &aux);
        // A zero width/height clears to the window's far edge.
        let _ = conn.clear_area(false, window, 0, 0, 0, 0);
    }
    let _ = conn.flush();
}

fn apply_toplevel_fullscreen(fs: bool) {
    let Some(conn) = crate::x11_state::x11rb_conn() else {
        return;
    };
    let Some(host) = crate::x11_state::host() else {
        return;
    };
    if host.toplevel == 0 {
        return;
    }
    // data: [action, prop1, prop2, source, 0]; action ADD=1 / REMOVE=0, source=app.
    let ev = ClientMessageEvent::new(
        32,
        host.toplevel,
        host.atoms.net_wm_state,
        ClientMessageData::from([u32::from(fs), host.atoms.net_wm_state_fullscreen, 0, 1, 0]),
    );
    let _ = conn.send_event(
        false,
        host.root,
        EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
        ev,
    );
    let _ = conn.flush();
}

// `parent` is the WM-managed app top-level; `video_host` is the app-owned child
// mpv embeds into (`--wid`). `conn` must be the connection that *created*
// `parent` — the WM delivers its `WM_DELETE` only to the creating client.
pub fn start(
    conn: Arc<RustConnection>,
    parent: u32,
    video_host: u32,
    root: u32,
    boot: ParentSnapshot,
) {
    *CONN_HOLD.lock() = Some(conn.clone());
    crate::registry::install_command_channel();
    let join = match std::thread::Builder::new()
        .name("jfn-x11-geometry".into())
        .spawn(move || geometry_thread_body(conn, parent, video_host, root, boot))
    {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[x11] failed to spawn geometry thread: {e}");
            return;
        }
    };
    *G.lock() = Some(Handle { join: Some(join) });
}

pub fn cleanup() {
    let mut g = G.lock();
    if let Some(h) = g.as_mut() {
        h.join();
    }
    *g = None;
}

// ===================================================================
// Geometry-thread working state (owned; no lock)
// ===================================================================

struct GeoWork {
    parent_x: i32,
    parent_y: i32,
    pw: i32,
    ph: i32,
    fullscreen: bool,
    maximized: bool,
    scale: jfn_platform_abi::Scale,
    structures: HashMap<SurfaceId, StructureSurface>,
    fsm: HashMap<SurfaceId, OverlayState>,
    /// The one visibility value each surface has; this map is its owner.
    visibility: HashMap<SurfaceId, Visibility>,
    /// Bottom-to-top overlay z-order.
    order: Vec<SurfaceId>,
    /// `_NET_WM_SYNC_REQUEST` counter, or 0 when the protocol was not advertised.
    sync_counter: u32,
    sync_pending: Option<(i32, u32)>,
    sync_armed: bool,
}

impl GeoWork {
    fn new(scale: jfn_platform_abi::Scale, snap: &ParentSnapshot) -> Self {
        Self {
            parent_x: snap.origin_x,
            parent_y: snap.origin_y,
            pw: snap.width,
            ph: snap.height,
            fullscreen: snap.fullscreen,
            maximized: snap.maximized,
            scale,
            structures: HashMap::new(),
            fsm: HashMap::new(),
            visibility: HashMap::new(),
            order: Vec::new(),
            sync_counter: crate::x11_state::host().map_or(0, |h| h.sync_counter),
            sync_pending: None,
            sync_armed: false,
        }
    }

    fn latch_sync(&mut self, hi: i32, lo: u32) {
        if self.sync_counter == 0 {
            return;
        }
        self.sync_pending = Some((hi, lo));
        self.sync_armed = false;
    }

    /// The counter write tells the WM our configures for this resize are done,
    /// so it must be queued behind them on the same connection.
    fn commit_resize(&mut self, conn: &RustConnection) {
        let _ = conn.flush();
        if !self.sync_armed {
            return;
        }
        if let Some((hi, lo)) = self.sync_pending.take() {
            let _ = conn.sync_set_counter(self.sync_counter, Int64 { hi, lo });
            let _ = conn.flush();
        }
    }

    fn publish(&self) {
        crate::x11_state::publish_parent(ParentSnapshot {
            origin_x: self.parent_x,
            origin_y: self.parent_y,
            width: self.pw,
            height: self.ph,
            fullscreen: self.fullscreen,
            maximized: self.maximized,
            scale: self.scale,
        });
    }

    /// Republish the live overlay window ids (bottom-to-top) for the cursor
    /// thread.
    fn publish_windows(&self) {
        let windows: Vec<u32> = self
            .order
            .iter()
            .filter_map(|id| self.structures.get(id).map(StructureSurface::window))
            .collect();
        crate::x11_state::publish_overlay_windows(windows);
    }
}

// ===================================================================
// Overlay window creation (structure module — the only place overlay
// ConfigureWindow / create is permitted)
// ===================================================================

/// `at` is the parent's root-relative geometry the overlay is born into.
fn create_overlay_window(
    conn: &RustConnection,
    host: &HostServices,
    paint: &PaintServices,
    fullscreen: bool,
    at: Geom,
) -> Option<u32> {
    let (x, y, w, h) = at;
    let win = conn.generate_id().ok()?;
    let aux = CreateWindowAux::new()
        .background_pixel(0)
        .border_pixel(0)
        // Managed transient when windowed; unmanaged only when born into
        // fullscreen, where the WM would otherwise strut-clamp it.
        .override_redirect(u32::from(fullscreen))
        .event_mask(EventMask::EXPOSURE)
        .colormap(paint.colormap);
    conn.create_window(
        paint.argb_depth,
        win,
        host.root,
        x as i16,
        y as i16,
        w.max(1) as u16,
        h.max(1) as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        paint.argb_visual,
        &aux,
    )
    .ok()?;

    // Tie the overlay to the app top-level so the WM raises/lowers/covers them
    // together. It stays a separate top-level (not a child): sibling children
    // don't alpha-blend over the video on X11.
    let _ = conn.change_property32(
        PropMode::REPLACE,
        win,
        u32::from(AtomEnum::WM_TRANSIENT_FOR),
        u32::from(AtomEnum::WINDOW),
        &[host.toplevel],
    );
    let _ = conn.change_property32(
        PropMode::REPLACE,
        win,
        host.atoms.net_wm_window_type,
        u32::from(AtomEnum::ATOM),
        &[host.atoms.net_wm_window_type_normal],
    );
    let _ = conn.change_property32(
        PropMode::REPLACE,
        win,
        host.atoms.net_wm_state,
        u32::from(AtomEnum::ATOM),
        &[
            host.atoms.net_wm_state_skip_taskbar,
            host.atoms.net_wm_state_skip_pager,
        ],
    );
    // Motif hints: flags=MWM_HINTS_DECORATIONS, decorations=0.
    let _ = conn.change_property32(
        PropMode::REPLACE,
        win,
        host.atoms.motif_wm_hints,
        host.atoms.motif_wm_hints,
        &[2_u32, 0, 0, 0, 0],
    );
    // WM_HINTS: InputHint set, input=false; focus should stay on mpv.
    let _ = conn.change_property32(
        PropMode::REPLACE,
        win,
        u32::from(AtomEnum::WM_HINTS),
        u32::from(AtomEnum::WM_HINTS),
        &[1_u32, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    let _ = conn.change_property32(
        PropMode::REPLACE,
        win,
        host.atoms.wm_protocols,
        u32::from(AtomEnum::ATOM),
        &[host.atoms.wm_delete_window],
    );
    let _ = conn.flush();
    Some(win)
}

/// Create the content GC on the content connection for `win`.
fn create_content_gc(win: u32) -> Option<u32> {
    let conn = crate::x11_state::x11rb_conn()?;
    let gc = conn.generate_id().ok()?;
    let _ = conn.create_gc(gc, win, &CreateGCAux::new());
    let _ = conn.flush();
    Some(gc)
}

// ===================================================================
// Command processing
// ===================================================================

fn handle_create(conn: &RustConnection, work: &mut GeoWork, id: SurfaceId, initial: Visibility) {
    let (Some(host), Some(paint)) = (crate::x11_state::host(), crate::x11_state::paint()) else {
        return;
    };
    let Some(win) = create_overlay_window(
        conn,
        host,
        paint,
        work.fullscreen,
        (work.parent_x, work.parent_y, work.pw, work.ph),
    ) else {
        return;
    };
    // Round-trip so the window exists server-side before the content connection
    // and the input connection reference it.
    if let Ok(cookie) = conn.get_input_focus() {
        let _ = cookie.reply();
    }
    let Some(gc) = create_content_gc(win) else {
        let _ = conn.destroy_window(win);
        let _ = conn.flush();
        return;
    };
    let external = {
        let mut g = registry().lock();
        match g.get_mut(id) {
            Some(record) => {
                record.window = Some(win);
                record.external
            }
            None => false,
        }
    };
    if external {
        set_empty_input_region(conn, win);
    } else {
        crate::input::grab_overlay_input(win);
    }

    let (structure, content) = split_capabilities(win, gc);
    work.structures.insert(id, structure);
    work.visibility.insert(id, initial);
    // Born unmapped: the FSM maps it on the next reconcile (and sets
    // override_redirect stacking if fullscreen).
    work.fsm.insert(
        id,
        OverlayState {
            mapped: false,
            unmanaged: work.fullscreen,
        },
    );
    if !work.order.contains(&id) {
        work.order.push(id);
    }
    if let Some(record) = registry().lock().get(id) {
        record.actor.attach_content(content);
    }
}

fn handle_destroy(conn: &RustConnection, work: &mut GeoWork, id: SurfaceId) {
    work.order.retain(|x| *x != id);
    work.fsm.remove(&id);
    work.visibility.remove(&id);
    if let Some(structure) = work.structures.remove(&id) {
        structure.unmap(conn);
        structure.destroy(conn);
        let _ = conn.flush();
    }
}

fn handle_set_order(conn: &RustConnection, work: &mut GeoWork, ids: Vec<SurfaceId>) {
    // Keep only ids we still own; preserve the requested order.
    let mut new_order: Vec<SurfaceId> = ids
        .into_iter()
        .filter(|id| work.structures.contains_key(id))
        .collect();
    // Append any owned surface the caller omitted (defensive).
    for id in &work.order {
        if !new_order.contains(id) {
            new_order.push(*id);
        }
    }
    work.order = new_order;
    // Apply the z-order once, on this reorder — not every reconcile (which would
    // feed our own ConfigureNotify back into a restack loop). Stack bottom-to-top
    // above the app top-level.
    let Some(toplevel) = crate::x11_state::host().map(|h| h.toplevel) else {
        return;
    };
    let mut prev = toplevel;
    for id in &work.order {
        if let Some(structure) = work.structures.get(id) {
            structure.restack_above(conn, prev);
            prev = structure.window();
        }
    }
    let _ = conn.flush();
}

/// Drain and apply queued structure commands. Returns whether anything changed
/// (so the caller reconciles + re-asserts stacking).
fn process_commands(conn: &RustConnection, work: &mut GeoWork) -> u64 {
    let (cmds, mark) = drain_commands();
    if cmds.is_empty() {
        return mark;
    }
    for cmd in cmds {
        match cmd {
            GeometryCommand::Create { id, initial } => handle_create(conn, work, id, initial),
            GeometryCommand::Destroy { id } => handle_destroy(conn, work, id),
            GeometryCommand::SetVisibility { id, visibility } => {
                work.visibility.insert(id, visibility);
            }
            GeometryCommand::SetOrder { ids } => handle_set_order(conn, work, ids),
            GeometryCommand::SetExternal { id } => {
                let window = registry().lock().get(id).and_then(|r| r.window);
                if let Some(window) = window {
                    set_empty_input_region(conn, window);
                }
            }
            GeometryCommand::SetTopInset { id, top_physical } => {
                let changed = {
                    let mut g = registry().lock();
                    match g.get_mut(id) {
                        Some(record) if record.top_physical != top_physical => {
                            record.top_physical = top_physical;
                            true
                        }
                        _ => false,
                    }
                };
                if !changed {
                    continue;
                }
            }
        }
    }
    work.publish_windows();
    mark
}

// ===================================================================
// Watch / query helpers
// ===================================================================

fn find_frame(conn: &RustConnection, mut w: Window, root: Window) -> Window {
    loop {
        let Ok(cookie) = conn.query_tree(w) else {
            return w;
        };
        let Ok(reply) = cookie.reply() else {
            return w;
        };
        let parent = reply.parent;
        if parent == 0 || parent == root {
            return w;
        }
        w = parent;
    }
}

fn watch_window(conn: &RustConnection, window: Window, mask: EventMask) {
    let aux = ChangeWindowAttributesAux::new().event_mask(mask);
    let _ = conn.change_window_attributes(window, &aux);
}

fn watch_compositor(conn: &RustConnection, root: Window) {
    let Some(host) = crate::x11_state::host() else {
        return;
    };
    if !matches!(
        conn.xfixes_query_version(5, 0).map(|c| c.reply()),
        Ok(Ok(_))
    ) {
        return;
    }
    let Ok(Ok(atom)) = conn
        .intern_atom(
            false,
            crate::lifecycle::cm_atom_name(host.screen_num).as_bytes(),
        )
        .map(|c| c.reply())
    else {
        return;
    };
    let mask = SelectionEventMask::SET_SELECTION_OWNER
        | SelectionEventMask::SELECTION_WINDOW_DESTROY
        | SelectionEventMask::SELECTION_CLIENT_CLOSE;
    let _ = conn.xfixes_select_selection_input(root, atom.atom, mask);
}

fn query_geometry(conn: &RustConnection, window: Window, root: Window) -> Option<Geom> {
    let geo = conn.get_geometry(window).ok()?.reply().ok()?;
    let trans = conn
        .translate_coordinates(window, root, 0, 0)
        .ok()?
        .reply()
        .ok()?;
    Some((
        trans.dst_x as i32,
        trans.dst_y as i32,
        geo.width as i32,
        geo.height as i32,
    ))
}

/// Read the top-level's `_NET_WM_STATE`: (fullscreen, maximized-both-axes).
fn read_wm_state(conn: &RustConnection, win: Window) -> (bool, bool) {
    let Some(host) = crate::x11_state::host() else {
        return (false, false);
    };
    let a = &host.atoms;
    if let Ok(Ok(reply)) = conn
        .get_property(false, win, a.net_wm_state, AtomEnum::ATOM, 0, 64)
        .map(|c| c.reply())
        && let Some(vals) = reply.value32()
    {
        let (mut fs, mut mv, mut mh) = (false, false, false);
        for atom in vals {
            fs |= atom == a.net_wm_state_fullscreen;
            mv |= atom == a.net_wm_state_maximized_vert;
            mh |= atom == a.net_wm_state_maximized_horz;
        }
        return (fs, mv && mh);
    }
    (false, false)
}

fn geometric_fullscreen(conn: &RustConnection, root: Window, geom: Geom) -> bool {
    if let Ok(Ok(rgeo)) = conn.get_geometry(root).map(|c| c.reply()) {
        return geom.2 >= rgeo.width as i32 && geom.3 >= rgeo.height as i32;
    }
    false
}

/// An empty XShape input region: the window accepts no pointer events of its
/// own, so everything falls through to the app top-level below it.
fn set_empty_input_region(conn: &RustConnection, window: Window) {
    let _ = conn.shape_rectangles(
        ShapeSo::SET,
        ShapeSk::INPUT,
        ClipOrdering::UNSORTED,
        window,
        0,
        0,
        &[],
    );
    let _ = conn.flush();
}

fn overlay_mapped(conn: &RustConnection, win: Window) -> Option<bool> {
    let r = conn.get_window_attributes(win).ok()?.reply().ok()?;
    Some(r.map_state != x11rb::protocol::xproto::MapState::UNMAPPED)
}

/// Apply the FSM effects for one overlay. `Effect::Place` reasserts position +
/// size together (the geometry thread is the sole sizer).
fn apply_overlay_effects(
    conn: &RustConnection,
    structure: &StructureSurface,
    effects: &[Effect],
    parent_geom: Geom,
    top_physical: i32,
    external: bool,
) {
    let (px, py, pw, ph) = parent_geom;
    let top = top_physical.max(0);
    for e in effects {
        match *e {
            Effect::Place => structure.place_and_size(conn, px, py + top, pw, ph - top),
            Effect::SetOverrideRedirect(v) => structure.set_override_redirect(conn, v),
            Effect::MapAndRaise => {
                structure.map(conn);
                if external {
                    set_empty_input_region(conn, structure.window());
                } else {
                    // The passive button grab may not survive the remap — re-grab.
                    crate::input::grab_overlay_input(structure.window());
                }
                structure.raise(conn);
            }
            Effect::Unmap => structure.unmap(conn),
        }
    }
}

fn activate_parent(conn: &RustConnection, root: Window, parent: Window) {
    let Some(host) = crate::x11_state::host() else {
        return;
    };
    let ev = ClientMessageEvent::new(
        32,
        parent,
        host.atoms.net_active_window,
        ClientMessageData::from([2, 0, 0, 0, 0]),
    );
    let _ = conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
        ev,
    );
    let _ = conn.flush();
}

// ===================================================================
// Reconcile
// ===================================================================

impl GeoLoop {
    /// Snapshot parent truth once, size the video host + every overlay from it
    /// in one flushed batch. A pending of `Restack` or above re-raises an
    /// unmanaged overlay over mpv after events that can restack the parent.
    fn reconcile(&mut self, pending: Pending) {
        let reassert_stack = pending >= Pending::Restack;
        let conn = &*self.conn;
        let work = &mut self.work;
        let (parent, video_host, embed, root, parent_mapped) = (
            self.parent,
            self.video_host,
            self.embed,
            self.root,
            self.parent_mapped,
        );
        let Some(parent_geom) = query_geometry(conn, parent, root) else {
            return;
        };
        let (state_fs, parent_max) = read_wm_state(conn, parent);
        let parent_fs = state_fs || geometric_fullscreen(conn, root, parent_geom);

        // The video host is a child, so it fills the client area in local coords
        // (0,0). Publish before the ConfigureWindow reaches the server so the proxy
        // forwards mpv only the ConfigureNotify matching the published size.
        let (fill_w, fill_h) = (parent_geom.2.max(1), parent_geom.3.max(1));
        crate::mpv_proxy::publish_host_geometry(fill_w as u16, fill_h as u16);
        let fill = ConfigureWindowAux::new()
            .x(0)
            .y(0)
            .width(fill_w as u32)
            .height(fill_h as u32);
        let _ = conn.configure_window(video_host, &fill);
        if let Some(embed) = embed {
            let _ = conn.configure_window(embed, &fill);
        }

        let changed = (work.parent_x, work.parent_y, work.pw, work.ph)
            != (parent_geom.0, parent_geom.1, parent_geom.2, parent_geom.3)
            || work.fullscreen != parent_fs
            || work.maximized != parent_max;
        work.parent_x = parent_geom.0;
        work.parent_y = parent_geom.1;
        work.pw = parent_geom.2;
        work.ph = parent_geom.3;
        work.fullscreen = parent_fs;
        work.maximized = parent_max;
        work.publish();
        if changed {
            jfn_platform_abi::notify_window_changed();
        }

        let reg = registry();
        let ids: Vec<SurfaceId> = work.order.clone();
        for id in ids {
            let Some(structure) = work.structures.get(&id) else {
                continue;
            };
            let window = structure.window();
            let observed = query_geometry(conn, window, root);
            if observed.is_some() {
                watch_window(conn, window, EventMask::STRUCTURE_NOTIFY);
            }
            let observed_mapped = overlay_mapped(conn, window);

            let shown = work
                .visibility
                .get(&id)
                .copied()
                .unwrap_or(Visibility::Hidden)
                .is_shown();
            let (top_physical, external) = {
                let g = reg.lock();
                let Some(record) = g.get(id) else {
                    continue;
                };
                let top = record.top_physical.max(0);
                // Feed the actor the authoritative swapchain target in lockstep.
                record.actor.resize(parent_geom.2, parent_geom.3 - top);
                (top, record.external)
            };

            let mut state = work.fsm.get(&id).copied().unwrap_or(OverlayState {
                mapped: false,
                unmanaged: parent_fs,
            });
            let inputs = overlay_fsm::Inputs {
                parent_geom,
                parent_fullscreen: parent_fs,
                want_visible: shown && parent_mapped,
                observed,
                observed_mapped,
            };
            let effects = overlay_fsm::step(&mut state, &inputs);
            apply_overlay_effects(
                conn,
                structure,
                &effects,
                parent_geom,
                top_physical,
                external,
            );
            if reassert_stack && state.unmanaged && state.mapped {
                structure.raise(conn);
            }
            work.fsm.insert(id, state);
        }
        work.commit_resize(conn);
    }
}

fn is_wm_delete(e: &ClientMessageEvent) -> bool {
    let Some(host) = crate::x11_state::host() else {
        return false;
    };
    e.type_ == host.atoms.wm_protocols && e.data.as_data32()[0] == host.atoms.wm_delete_window
}

/// Data layout: `[protocol, timestamp, lo, hi, _]`.
fn parse_sync_request(e: &ClientMessageEvent) -> Option<(i32, u32)> {
    let host = crate::x11_state::host()?;
    if host.sync_counter == 0 || host.atoms.net_wm_sync_request == 0 {
        return None;
    }
    let data = e.data.as_data32();
    if e.type_ != host.atoms.wm_protocols || data[0] != host.atoms.net_wm_sync_request {
        return None;
    }
    Some((data[3] as i32, data[2]))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Pending {
    Idle,
    Reconcile,
    Restack,
    Refocus,
}

enum Phase {
    Running(Pending),
    Stopping,
}

struct GeoLoop {
    conn: Arc<RustConnection>,
    work: GeoWork,
    parent: Window,
    video_host: Window,
    root: Window,
    frame: Window,
    embed: Option<Window>,
    parent_mapped: bool,
    phase: Phase,
    /// Highest command ticket drained but not yet applied + round-tripped.
    fence_mark: u64,
    signal: LoopSignal,
}

impl GeoLoop {
    fn new(
        conn: Arc<RustConnection>,
        parent: Window,
        video_host: Window,
        root: Window,
        signal: LoopSignal,
        boot: ParentSnapshot,
    ) -> GeoLoop {
        let watch_mask = EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE;
        watch_window(&conn, parent, watch_mask);
        watch_window(&conn, video_host, EventMask::SUBSTRUCTURE_NOTIFY);
        let frame = find_frame(&conn, parent, root);
        if frame != parent {
            watch_window(&conn, frame, watch_mask);
        }
        watch_window(&conn, root, EventMask::PROPERTY_CHANGE);
        watch_compositor(&conn, root);
        let _ = conn.flush();

        let work = GeoWork::new(boot.scale, &boot);

        GeoLoop {
            conn,
            work,
            parent,
            video_host,
            root,
            frame,
            embed: None,
            parent_mapped: true,
            phase: Phase::Running(Pending::Restack),
            fence_mark: 0,
            signal,
        }
    }

    fn settle(&mut self) {
        let Phase::Running(pending) = self.phase else {
            return;
        };
        if pending == Pending::Idle {
            return;
        }
        self.phase = Phase::Running(Pending::Idle);
        self.reconcile(pending);
        if pending >= Pending::Refocus {
            activate_parent(&self.conn, self.root, self.parent);
        }
        self.release_fence();
    }

    /// Round-trip the applied structure changes, then release everyone waiting
    /// on a command that landed in this pass.
    fn release_fence(&mut self) {
        let mark = std::mem::take(&mut self.fence_mark);
        if mark == 0 {
            return;
        }
        let _ = self.conn.flush();
        if let Ok(cookie) = self.conn.get_input_focus() {
            let _ = cookie.reply();
        }
        publish_applied(mark);
    }

    fn on_event(&mut self, ev: Event) {
        let pending = handle_event(self, ev);
        self.raise_to(pending);
    }

    fn raise_to(&mut self, pending: Pending) {
        if let Phase::Running(cur) = self.phase {
            self.phase = Phase::Running(cur.max(pending));
        }
    }

    fn on_resync(&mut self) {
        let mark = process_commands(&self.conn, &mut self.work);
        self.fence_mark = self.fence_mark.max(mark);
        self.raise_to(Pending::Restack);
    }

    fn shutdown(&mut self) {
        let _ = self.conn.unmap_window(self.parent);
        let _ = self.conn.flush();
        self.phase = Phase::Stopping;
        self.signal.stop();
        // Nothing will apply another command; strand no waiter.
        publish_applied(u64::MAX);
    }

    /// Republishes on any change; [`Scale`] compares exactly, so there is no
    /// tolerance to pick.
    fn refresh_display_scale(&mut self) -> Pending {
        let scale = crate::scale::query_display_scale();
        if self.work.scale == scale {
            return Pending::Idle;
        }
        self.work.scale = scale;
        self.work.publish();
        tracing::info!(target: "Platform", "display scale changed: {scale}");
        jfn_platform_abi::notify_window_changed();
        Pending::Reconcile
    }
}

impl Drop for GeoLoop {
    fn drop(&mut self) {
        for structure in self.work.structures.values() {
            structure.unmap(&self.conn);
        }
        let _ = self.conn.flush();
        publish_applied(u64::MAX);
        jfn_playback::lifecycle::jfn_lifecycle_set_visible(false);
    }
}

fn handle_event(state: &mut GeoLoop, ev: Event) -> Pending {
    let is_parentish = |w: Window| w == state.parent || w == state.frame;
    match ev {
        Event::CreateNotify(e) => {
            if e.parent == state.video_host {
                state.embed = Some(e.window);
                Pending::Reconcile
            } else {
                Pending::Idle
            }
        }
        Event::ConfigureNotify(e) => {
            if is_parentish(e.window) {
                if e.window == state.parent {
                    state.work.sync_armed = true;
                }
                Pending::Restack
            } else {
                Pending::Reconcile
            }
        }
        Event::CirculateNotify(e) => {
            if is_parentish(e.window) {
                Pending::Restack
            } else {
                Pending::Reconcile
            }
        }
        Event::PropertyNotify(e) => {
            if e.window == state.parent {
                Pending::Restack
            } else if e.window == state.root && e.atom == u32::from(AtomEnum::RESOURCE_MANAGER) {
                state.refresh_display_scale()
            } else {
                Pending::Idle
            }
        }
        Event::ReparentNotify(e) => {
            if e.window == state.parent {
                let new_frame = find_frame(&state.conn, state.parent, state.root);
                if new_frame != state.parent {
                    watch_window(
                        &state.conn,
                        new_frame,
                        EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE,
                    );
                }
                state.frame = new_frame;
                let _ = state.conn.flush();
                Pending::Restack
            } else {
                Pending::Idle
            }
        }
        Event::MapNotify(e) => {
            if e.window == state.parent {
                state.parent_mapped = true;
                jfn_playback::lifecycle::jfn_lifecycle_set_visible(true);
                Pending::Refocus
            } else {
                Pending::Idle
            }
        }
        Event::UnmapNotify(e) => {
            if e.window == state.parent {
                state.parent_mapped = false;
                jfn_playback::lifecycle::jfn_lifecycle_set_visible(false);
            }
            Pending::Reconcile
        }
        Event::DestroyNotify(e) => {
            if e.window == state.parent {
                jfn_shutdown_initiate();
            }
            if Some(e.window) == state.embed {
                state.embed = None;
            }
            Pending::Idle
        }
        Event::ClientMessage(e) => {
            if e.window == state.parent && is_wm_delete(&e) {
                jfn_shutdown_initiate();
            } else if e.window == state.parent
                && let Some((hi, lo)) = parse_sync_request(&e)
            {
                state.work.latch_sync(hi, lo);
            }
            Pending::Idle
        }
        Event::XfixesSelectionNotify(e) => {
            if e.owner != x11rb::NONE {
                tracing::debug!(target: "Platform", "{}", crate::lifecycle::COMPOSITOR_DETECTED_MSG);
                Pending::Restack
            } else {
                tracing::error!(target: "Platform", "{}", crate::lifecycle::COMPOSITOR_NOT_DETECTED_MSG);
                Pending::Idle
            }
        }
        _ => Pending::Idle,
    }
}

fn geometry_thread_body(
    conn: Arc<RustConnection>,
    parent: Window,
    video_host: Window,
    root: Window,
    boot: ParentSnapshot,
) {
    let mut event_loop: EventLoop<'_, GeoLoop> = match EventLoop::try_new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("[x11] failed to create geometry event loop: {e}");
            return;
        }
    };
    let signal = event_loop.get_signal();
    let mut state = GeoLoop::new(conn.clone(), parent, video_host, root, signal, boot);
    let handle = event_loop.handle();

    if let Err(e) = handle.insert_source(X11Source::new(conn), |ev, (), state: &mut GeoLoop| {
        state.on_event(ev);
    }) {
        eprintln!("[x11] failed to register x11 event source: {e}");
        return;
    }

    if let Some(ev) = x11_shutdown_waker() {
        // `Drain::Never`: `input.rs` waits on the same eventfd, and
        // level-triggered-undrained is what lets both threads see one signal.
        let res = handle.insert_source(
            WakeSource::new(ev.fd(), Drain::Never),
            |(), (), state: &mut GeoLoop| state.shutdown(),
        );
        if let Err(e) = res {
            eprintln!("[x11] failed to register shutdown source: {e}");
            return;
        }
    }

    if let Some(ev) = x11_geometry_resync_waker() {
        let res = handle.insert_source(
            WakeSource::new(ev.fd(), Drain::BeforeCallback),
            |(), (), state: &mut GeoLoop| state.on_resync(),
        );
        if let Err(e) = res {
            eprintln!("[x11] failed to register resync source: {e}");
            return;
        }
    }

    state.settle();
    if let Err(e) = event_loop.run(None, &mut state, GeoLoop::settle) {
        eprintln!("[x11] geometry event loop exited: {e}");
    }
}
