//! X11 geometry thread: the sole writer of ALL overlay + video-host structure.
//!
//! It owns every [`StructureSurface`], consumes the [`GeometryCommand`] queue
//! (create/destroy/visibility/restack), and is the sole sizer of the overlays
//! and the video host. It blocks in `poll(-1)` with no timer: running
//! timer-free is safe because every change class emits an event on a window we
//! watch — `STRUCTURE_NOTIFY | PROPERTY_CHANGE` on the parent and its frame,
//! `STRUCTURE_NOTIFY` on each overlay (so a WM clamp / our own configure
//! re-triggers a reconcile), and `PROPERTY_CHANGE` on the root for
//! RESOURCE_MANAGER (Xft.dpi) updates. It publishes the parent's live geometry
//! as an immutable [`ParentSnapshot`] so all other readers are lock-free.
//!
//! Structure (create/size/place/map/restack) runs on the geometry connection;
//! content (pixel upload) runs on the content connection inside each surface's
//! [`crate::overlay_actor::OverlayActor`]. No overlay window ever has two
//! writers.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::Arc;

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use parking_lot::Mutex;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageData, ClientMessageEvent, ConfigureWindowAux,
    ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, PropMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use jfn_playback::shutdown::jfn_shutdown_initiate;
use jfn_wake_event::WakeEvent;

use crate::input::x11_shutdown_waker;
use crate::overlay_fsm::{self, Effect, Geom, OverlayState};
use crate::registry::{
    GeometryCommand, StructureSurface, SurfaceId, drain_commands, registry, split_capabilities,
};
use crate::resize_sync::{self, ResizeSync, SyncCounterAck, SyncState};
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

/// The geometry/top-level connection, for off-thread ack of a resize-sync counter.
pub(crate) fn toplevel_conn() -> Option<Arc<RustConnection>> {
    CONN_HOLD.lock().clone()
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
pub fn start(conn: Arc<RustConnection>, parent: u32, video_host: u32, root: u32) {
    *CONN_HOLD.lock() = Some(conn.clone());
    crate::registry::install_command_channel();
    let join = match std::thread::Builder::new()
        .name("jfn-x11-geometry".into())
        .spawn(move || geometry_thread_body(conn, parent, video_host, root))
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
    scale: f32,
    structures: HashMap<SurfaceId, StructureSurface>,
    fsm: HashMap<SurfaceId, OverlayState>,
    /// Bottom-to-top overlay z-order.
    order: Vec<SurfaceId>,
    /// `_NET_WM_SYNC_REQUEST` counter, or 0 when the protocol was not advertised.
    sync_counter: u32,
    /// Each overlay's last-known size, the ground truth resize-sync completion is
    /// read from. Sole writer: the overlay's own `ConfigureNotify` (seeded at
    /// create). Keyed by [`SurfaceId`]; a destroyed overlay drops out entirely.
    sizes: HashMap<SurfaceId, (i32, i32)>,
    /// The outstanding resize-sync obligation, if any.
    resize_sync: Option<ResizeSync>,
}

impl GeoWork {
    fn new(scale: f32, snap: &ParentSnapshot) -> Self {
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
            order: Vec::new(),
            sync_counter: crate::x11_state::host().map_or(0, |h| h.sync_counter),
            sizes: HashMap::new(),
            resize_sync: None,
        }
    }

    /// Latch a resize-sync request, superseding any prior obligation (a newer
    /// resize replaces the old one). `target` starts `None`: the parent's resize
    /// `ConfigureNotify` names it, gating the ack against pre-resize geometry.
    fn latch_sync(&mut self, hi: i32, lo: u32) {
        if self.sync_counter == 0 {
            return;
        }
        self.resize_sync = Some(ResizeSync {
            signal: Box::new(SyncCounterAck {
                counter: self.sync_counter,
                hi,
                lo,
            }),
            target: None,
        });
    }

    /// An overlay participates in resize-sync completion exactly while it is
    /// mapped (shown). Hidden/withdrawn overlays are unmapped, so they never gate.
    fn participating(&self, id: SurfaceId) -> bool {
        self.fsm.get(&id).is_some_and(|s| s.mapped)
    }

    /// Ack the outstanding resize once every participating overlay has reached the
    /// resolved target. Reads owned sizes each call, so a no-op place (no
    /// `ConfigureNotify`) that already sits at target settles on the spot instead
    /// of waiting forever.
    fn drive_resize_sync(&mut self) -> SyncState {
        let target = self.resize_sync.as_ref().and_then(|rs| rs.target);
        let settled = target.is_some_and(|t| {
            resize_sync::all_settled(
                self.order
                    .iter()
                    .map(|id| (self.participating(*id), self.sizes.get(id).copied())),
                t,
            )
        });
        resize_sync::drive(&mut self.resize_sync, settled)
    }

    fn id_for_window(&self, window: Window) -> Option<SurfaceId> {
        self.structures
            .iter()
            .find(|(_, s)| s.window() == window)
            .map(|(id, _)| *id)
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

#[allow(clippy::too_many_arguments)]
fn create_overlay_window(
    conn: &RustConnection,
    host: &HostServices,
    paint: &PaintServices,
    fullscreen: bool,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Option<u32> {
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

fn handle_create(conn: &RustConnection, work: &mut GeoWork, id: SurfaceId) {
    let (Some(host), Some(paint)) = (crate::x11_state::host(), crate::x11_state::paint()) else {
        return;
    };
    let Some(win) = create_overlay_window(
        conn,
        host,
        paint,
        work.fullscreen,
        work.parent_x,
        work.parent_y,
        work.pw,
        work.ph,
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
    crate::input::grab_overlay_input(win);

    let (structure, content) = split_capabilities(win, gc);
    work.structures.insert(id, structure);
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
    // Seed the size fact with the create geometry; its own ConfigureNotify is the
    // only writer thereafter.
    work.sizes.insert(id, (work.pw, work.ph));
    if let Some(record) = registry().lock().get(id) {
        record.actor.attach_content(content);
    }
}

fn handle_destroy(conn: &RustConnection, work: &mut GeoWork, id: SurfaceId) {
    work.order.retain(|x| *x != id);
    work.fsm.remove(&id);
    work.sizes.remove(&id);
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
fn process_commands(conn: &RustConnection, work: &mut GeoWork) -> bool {
    let cmds = drain_commands();
    if cmds.is_empty() {
        return false;
    }
    for cmd in cmds {
        match cmd {
            GeometryCommand::Create { id } => handle_create(conn, work, id),
            GeometryCommand::Destroy { id } => handle_destroy(conn, work, id),
            GeometryCommand::SetVisible { id, visible } => {
                // Redundant with the CEF-thread write, but keeps the geometry
                // owner's view authoritative; reconcile reads this flag.
                if let Some(record) = registry().lock().get_mut(id) {
                    record.visible = visible;
                }
            }
            GeometryCommand::SetOrder { ids } => handle_set_order(conn, work, ids),
        }
    }
    work.publish_windows();
    true
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
) {
    let (px, py, pw, ph) = parent_geom;
    for e in effects {
        match *e {
            Effect::Place => structure.place_and_size(conn, px, py, pw, ph),
            Effect::SetOverrideRedirect(v) => structure.set_override_redirect(conn, v),
            Effect::MapAndRaise => {
                structure.map(conn);
                // The passive button grab may not survive the remap — re-grab.
                crate::input::grab_overlay_input(structure.window());
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

/// Snapshot parent truth once, size the video host + every overlay from it in
/// one flushed batch. `reassert_stack` re-raises an unmanaged overlay over mpv
/// after events that can restack the parent.
#[allow(clippy::too_many_arguments)]
fn reconcile(
    conn: &RustConnection,
    work: &mut GeoWork,
    parent: Window,
    video_host: Window,
    embed: Option<Window>,
    root: Window,
    parent_mapped: bool,
    reassert_stack: bool,
) {
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

    // Size + place every overlay from the one snapshot. Hold the registry lock
    // for the loop so present calls serialize behind it; it's brief — X
    // requests are async, mailbox pokes are cheap.
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

        let visible = {
            let g = reg.lock();
            let Some(record) = g.get(id) else {
                continue;
            };
            // Feed the actor the authoritative swapchain target in lockstep.
            record.actor.resize(parent_geom.2, parent_geom.3);
            record.visible
        };

        let mut state = work.fsm.get(&id).copied().unwrap_or(OverlayState {
            mapped: false,
            unmanaged: parent_fs,
        });
        let inputs = overlay_fsm::Inputs {
            parent_geom,
            parent_fullscreen: parent_fs,
            want_visible: visible && parent_mapped,
            observed,
            observed_mapped,
        };
        let effects = overlay_fsm::step(&mut state, &inputs);
        apply_overlay_effects(conn, structure, &effects, parent_geom);
        if reassert_stack && state.unmanaged && state.mapped {
            structure.raise(conn);
        }
        work.fsm.insert(id, state);
    }
    let _ = conn.flush();
    let _ = work.drive_resize_sync();
}

fn hide_overlays(conn: &RustConnection, work: &GeoWork) {
    for structure in work.structures.values() {
        structure.unmap(conn);
    }
    let _ = conn.flush();
    jfn_playback::lifecycle::jfn_lifecycle_set_visible(false);
}

enum Trigger {
    Ignore,
    External,
    Overlay,
    ParentMap,
    ParentUnmap,
}

fn geometry_thread_body(
    conn: Arc<RustConnection>,
    parent: Window,
    video_host: Window,
    root: Window,
) {
    let watch_mask = EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE;
    watch_window(&conn, parent, watch_mask);
    watch_window(&conn, video_host, EventMask::SUBSTRUCTURE_NOTIFY);
    let mut frame = find_frame(&conn, parent, root);
    if frame != parent {
        watch_window(&conn, frame, watch_mask);
    }
    watch_window(&conn, root, EventMask::PROPERTY_CHANGE);
    watch_compositor(&conn, root);
    let _ = conn.flush();

    let scale = crate::x11_state::parent_snapshot().scale;
    let snap = crate::x11_state::parent_snapshot();
    let mut work = GeoWork::new(scale, &snap);

    let x11_fd = unsafe { BorrowedFd::borrow_raw(conn.stream().as_raw_fd()) };
    let shutdown_fd = x11_shutdown_waker().map(|ev| unsafe { BorrowedFd::borrow_raw(ev.fd()) });
    let resync_fd =
        x11_geometry_resync_waker().map(|ev| unsafe { BorrowedFd::borrow_raw(ev.fd()) });

    let mut fds = vec![PollFd::new(x11_fd, PollFlags::POLLIN)];
    let shutdown_idx = shutdown_fd.map(|fd| {
        fds.push(PollFd::new(fd, PollFlags::POLLIN));
        fds.len() - 1
    });
    let resync_idx = resync_fd.map(|fd| {
        fds.push(PollFd::new(fd, PollFlags::POLLIN));
        fds.len() - 1
    });

    let mut parent_mapped = true;
    let mut embed: Option<Window> = None;
    reconcile(
        &conn,
        &mut work,
        parent,
        video_host,
        embed,
        root,
        parent_mapped,
        true,
    );

    loop {
        match poll(&mut fds, PollTimeout::NONE) {
            Err(Errno::EINTR) => continue,
            Err(_) => break,
            Ok(_) => {}
        }
        let revents = |idx: Option<usize>| {
            idx.and_then(|i| fds[i].revents())
                .unwrap_or(PollFlags::empty())
        };

        if revents(shutdown_idx).contains(PollFlags::POLLIN) {
            let _ = conn.unmap_window(parent);
            let _ = conn.flush();
            hide_overlays(&conn, &work);
            break;
        }
        if revents(Some(0))
            .intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL)
        {
            hide_overlays(&conn, &work);
            break;
        }

        let mut wake = false;
        let mut reassert = false;
        let mut activate = false;
        if revents(resync_idx).contains(PollFlags::POLLIN) {
            if let Some(ev) = x11_geometry_resync_waker() {
                ev.drain();
            }
            // Drain the structure-command queue; a create/destroy/restack or an
            // mpv fullscreen toggle can change parent stacking, so always
            // re-assert overlay stacking after a resync.
            process_commands(&conn, &mut work);
            wake = true;
            reassert = true;
        }

        while let Ok(Some(ev)) = conn.poll_for_event() {
            match handle_event(
                &conn, parent, video_host, root, &mut frame, &mut embed, &mut work, ev,
            ) {
                Trigger::Ignore => {}
                Trigger::External => {
                    wake = true;
                    reassert = true;
                }
                Trigger::Overlay => wake = true,
                Trigger::ParentMap => {
                    parent_mapped = true;
                    jfn_playback::lifecycle::jfn_lifecycle_set_visible(true);
                    wake = true;
                    reassert = true;
                    activate = true;
                }
                Trigger::ParentUnmap => {
                    parent_mapped = false;
                    jfn_playback::lifecycle::jfn_lifecycle_set_visible(false);
                    wake = true;
                }
            }
        }

        if wake {
            reconcile(
                &conn,
                &mut work,
                parent,
                video_host,
                embed,
                root,
                parent_mapped,
                reassert,
            );
            if activate {
                activate_parent(&conn, root, parent);
            }
        }
    }
}

fn is_wm_delete(e: &ClientMessageEvent) -> bool {
    let Some(host) = crate::x11_state::host() else {
        return false;
    };
    e.type_ == host.atoms.wm_protocols && e.data.as_data32()[0] == host.atoms.wm_delete_window
}

/// Parse a `_NET_WM_SYNC_REQUEST` client message into its requested counter
/// value `(hi, lo)`. Data layout: `[protocol, timestamp, lo, hi, _]`.
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

/// Re-probe the app-owned display scale after an Xft.dpi change. Returns
/// `External` when the scale actually changed.
fn refresh_display_scale(work: &mut GeoWork) -> Trigger {
    let scale = crate::scale::query_display_scale().unwrap_or(1.0);
    if (work.scale - scale).abs() > f32::EPSILON {
        work.scale = scale;
        work.publish();
        tracing::info!(target: "Platform", "display scale changed: {scale}");
        jfn_platform_abi::notify_window_changed();
        Trigger::External
    } else {
        Trigger::Ignore
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_event(
    conn: &RustConnection,
    parent: Window,
    video_host: Window,
    root: Window,
    frame: &mut Window,
    embed: &mut Option<Window>,
    work: &mut GeoWork,
    ev: Event,
) -> Trigger {
    let is_parentish = |w: Window| w == parent || w == *frame;
    match ev {
        Event::CreateNotify(e) => {
            if e.parent == video_host {
                *embed = Some(e.window);
                Trigger::Overlay
            } else {
                Trigger::Ignore
            }
        }
        Event::ConfigureNotify(e) => {
            if is_parentish(e.window) {
                if e.window == parent
                    && let Some(rs) = work.resize_sync.as_mut()
                {
                    // The resize the sync request preceded has landed; its size is
                    // the target overlays must reach before we ack.
                    rs.target = Some((e.width as i32, e.height as i32));
                }
                Trigger::External
            } else {
                // Sole writer of the overlay's size fact.
                if let Some(id) = work.id_for_window(e.window) {
                    work.sizes.insert(id, (e.width as i32, e.height as i32));
                }
                Trigger::Overlay
            }
        }
        Event::CirculateNotify(e) => {
            if is_parentish(e.window) {
                Trigger::External
            } else {
                Trigger::Overlay
            }
        }
        Event::PropertyNotify(e) => {
            if e.window == parent {
                Trigger::External
            } else if e.window == root && e.atom == u32::from(AtomEnum::RESOURCE_MANAGER) {
                refresh_display_scale(work)
            } else {
                Trigger::Ignore
            }
        }
        Event::ReparentNotify(e) => {
            if e.window == parent {
                let new_frame = find_frame(conn, parent, root);
                if new_frame != parent {
                    watch_window(
                        conn,
                        new_frame,
                        EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE,
                    );
                }
                *frame = new_frame;
                let _ = conn.flush();
                return Trigger::External;
            }
            Trigger::Ignore
        }
        Event::MapNotify(e) => {
            if e.window == parent {
                Trigger::ParentMap
            } else {
                Trigger::Ignore
            }
        }
        Event::UnmapNotify(e) => {
            if e.window == parent {
                Trigger::ParentUnmap
            } else {
                Trigger::Overlay
            }
        }
        Event::DestroyNotify(e) => {
            if e.window == parent {
                jfn_shutdown_initiate();
            }
            if Some(e.window) == *embed {
                *embed = None;
            }
            Trigger::Ignore
        }
        Event::ClientMessage(e) => {
            if e.window == parent && is_wm_delete(&e) {
                jfn_shutdown_initiate();
            } else if e.window == parent
                && let Some((hi, lo)) = parse_sync_request(&e)
            {
                work.latch_sync(hi, lo);
            }
            Trigger::Ignore
        }
        Event::XfixesSelectionNotify(e) => {
            if e.owner != x11rb::NONE {
                tracing::debug!(target: "Platform", "{}", crate::lifecycle::COMPOSITOR_DETECTED_MSG);
                Trigger::External
            } else {
                tracing::error!(target: "Platform", "{}", crate::lifecycle::COMPOSITOR_NOT_DETECTED_MSG);
                Trigger::Ignore
            }
        }
        _ => Trigger::Ignore,
    }
}
