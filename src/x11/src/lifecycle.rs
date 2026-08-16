//! X11 host-window creation, init/cleanup/clamp, and helpers for atom
//! interning, ARGB visual discovery, parent geometry queries, and overlay
//! repositioning.

use parking_lot::Mutex;
use x11rb::connection::Connection as X11rbConnection;
use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
use x11rb::protocol::shm::ConnectionExt as X11rbShmConnection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as X11rbXprotoConnection, CreateWindowAux, EventMask, PropMode, Screen,
    VisualClass, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as X11rbWrapperConnection;

use jfn_platform_abi::BootGeometry;

use crate::x11_state::{
    Atoms, HostServices, PaintServices, ParentSnapshot, X11RB_CONN, host, set_host_services,
    set_paint_services,
};

/// Must match `StartupWMClass` in `net.nullsum.JelliumDesktop.desktop` so the
/// DE resolves the window to that desktop file for the taskbar icon.
const WM_CLASS_VALUE: &[u8] = b"net.nullsum.JelliumDesktop\0net.nullsum.JelliumDesktop\0";
const APP_TITLE: &[u8] = b"Jellium Desktop";

/// Advertise the app top-level's identity and, when the full XSync handshake can
/// be established, the `_NET_WM_SYNC_REQUEST` protocol. Returns the created sync
/// counter id, or 0 if sync could not be set up (then the protocol is NOT
/// advertised — a WM must never wait on a counter we would never set).
fn set_toplevel_identity(conn: &RustConnection, win: u32, atoms: &Atoms) -> u32 {
    let _ = conn.change_property8(
        PropMode::REPLACE,
        win,
        u32::from(AtomEnum::WM_CLASS),
        u32::from(AtomEnum::STRING),
        WM_CLASS_VALUE,
    );
    let _ = conn.change_property8(
        PropMode::REPLACE,
        win,
        u32::from(AtomEnum::WM_NAME),
        u32::from(AtomEnum::STRING),
        APP_TITLE,
    );
    let utf8 = intern_atom(conn, b"UTF8_STRING");
    let net_wm_name = intern_atom(conn, b"_NET_WM_NAME");
    if utf8 != 0 && net_wm_name != 0 {
        let _ = conn.change_property8(PropMode::REPLACE, win, net_wm_name, utf8, APP_TITLE);
    }

    let sync_counter = setup_sync_counter(conn, win, atoms);

    // Keep WM_DELETE_WINDOW; add _NET_WM_SYNC_REQUEST only when the counter is
    // real (all-or-nothing).
    let mut protocols = vec![atoms.wm_delete_window];
    if sync_counter != 0 && atoms.net_wm_sync_request != 0 {
        protocols.push(atoms.net_wm_sync_request);
    }
    let _ = conn.change_property32(
        PropMode::REPLACE,
        win,
        atoms.wm_protocols,
        u32::from(AtomEnum::ATOM),
        &protocols,
    );
    let _ = conn.change_property32(
        PropMode::REPLACE,
        win,
        atoms.net_wm_window_type,
        u32::from(AtomEnum::ATOM),
        &[atoms.net_wm_window_type_normal],
    );
    let _ = conn.flush();
    sync_counter
}

/// Create the resize-sync XSync counter and set `_NET_WM_SYNC_REQUEST_COUNTER`
/// on the top-level. Returns 0 on any failure so the caller withholds the
/// protocol advertisement.
fn setup_sync_counter(conn: &RustConnection, win: u32, atoms: &Atoms) -> u32 {
    use x11rb::protocol::sync::{ConnectionExt as _, Int64};

    if atoms.net_wm_sync_request_counter == 0 {
        return 0;
    }
    let sync_ok = conn
        .sync_initialize(3, 0)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some();
    if !sync_ok {
        return 0;
    }
    let Ok(counter) = conn.generate_id() else {
        return 0;
    };
    let created = conn
        .sync_create_counter(counter, Int64 { hi: 0, lo: 0 })
        .ok()
        .and_then(|c| c.check().ok())
        .is_some();
    if !created {
        return 0;
    }
    let advertised = conn
        .change_property32(
            PropMode::REPLACE,
            win,
            atoms.net_wm_sync_request_counter,
            atoms.cardinal,
            &[counter],
        )
        .ok()
        .and_then(|c| c.check().ok())
        .is_some();
    if !advertised {
        let _ = conn.sync_destroy_counter(counter);
        return 0;
    }
    counter
}

/// Find a 32-bit TrueColor visual.
fn find_argb_visual(screen: &Screen) -> Option<u32> {
    screen
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.class == VisualClass::TRUE_COLOR)
        .map(|v| v.visual_id)
}

fn intern_atom(conn: &RustConnection, name: &[u8]) -> u32 {
    conn.intern_atom(false, name)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|r| r.atom)
        .unwrap_or(0)
}

static BOOT_GEOMETRY: Mutex<Option<BootGeometry>> = Mutex::new(None);

pub(crate) fn set_boot_geometry(g: BootGeometry) {
    *BOOT_GEOMETRY.lock() = Some(g);
}

fn intern_atoms(conn: &RustConnection) -> Atoms {
    Atoms {
        net_wm_window_type: intern_atom(conn, b"_NET_WM_WINDOW_TYPE"),
        net_wm_window_type_normal: intern_atom(conn, b"_NET_WM_WINDOW_TYPE_NORMAL"),
        net_wm_state: intern_atom(conn, b"_NET_WM_STATE"),
        net_wm_state_skip_taskbar: intern_atom(conn, b"_NET_WM_STATE_SKIP_TASKBAR"),
        net_wm_state_skip_pager: intern_atom(conn, b"_NET_WM_STATE_SKIP_PAGER"),
        net_wm_state_fullscreen: intern_atom(conn, b"_NET_WM_STATE_FULLSCREEN"),
        net_wm_state_maximized_vert: intern_atom(conn, b"_NET_WM_STATE_MAXIMIZED_VERT"),
        net_wm_state_maximized_horz: intern_atom(conn, b"_NET_WM_STATE_MAXIMIZED_HORZ"),
        wm_protocols: intern_atom(conn, b"WM_PROTOCOLS"),
        wm_delete_window: intern_atom(conn, b"WM_DELETE_WINDOW"),
        net_wm_sync_request: intern_atom(conn, b"_NET_WM_SYNC_REQUEST"),
        net_wm_sync_request_counter: intern_atom(conn, b"_NET_WM_SYNC_REQUEST_COUNTER"),
        cardinal: u32::from(AtomEnum::CARDINAL),
        motif_wm_hints: intern_atom(conn, b"_MOTIF_WM_HINTS"),
        net_active_window: intern_atom(conn, b"_NET_ACTIVE_WINDOW"),
        clipboard: intern_atom(conn, b"CLIPBOARD"),
        primary: u32::from(AtomEnum::PRIMARY),
        targets: intern_atom(conn, b"TARGETS"),
        timestamp: intern_atom(conn, b"TIMESTAMP"),
        utf8_string: intern_atom(conn, b"UTF8_STRING"),
        text: intern_atom(conn, b"TEXT"),
        text_plain_utf8: intern_atom(conn, b"text/plain;charset=utf-8"),
        incr: intern_atom(conn, b"INCR"),
        jfn_selection: intern_atom(conn, b"JFN_SELECTION"),
    }
}

/// Create the app-owned WM toplevel and the video-host child mpv embeds into
/// (`--wid`), at the boot geometry, and start the geometry thread. Runs
/// before mpv init — while the proxy has `DISPLAY` repointed — so every
/// connection here targets the real display explicitly. Idempotent.
pub(crate) fn ensure_host_window() -> bool {
    if host().is_some() {
        return true;
    }
    let Some(boot) = *BOOT_GEOMETRY.lock() else {
        tracing::error!(
            target: "Main",
            "x11: no boot geometry; WindowOwner::apply_boot_geometry must seed it before the host window is created"
        );
        return false;
    };

    // The top-level is created on the connection the geometry thread owns and
    // polls: the WM delivers `WM_DELETE` (empty-mask SendEvent) only to the
    // creating client, so that client must be the one watching for the close.
    let display = crate::mpv_proxy::real_display();
    let (geo_conn, screen_num) = match RustConnection::connect(display.as_deref()) {
        Ok((conn, screen_num)) => (std::sync::Arc::new(conn), screen_num as i32),
        Err(e) => {
            eprintln!("[x11] failed to connect top-level/geometry connection: {e:?}");
            return false;
        }
    };
    let Some(screen) = geo_conn.setup().roots.get(screen_num as usize) else {
        eprintln!("[x11] no screen at index {screen_num}");
        return false;
    };
    let root = screen.root;
    let black_pixel = screen.black_pixel;
    let atoms = intern_atoms(&geo_conn);
    let net_wm_state = atoms.net_wm_state;
    let scale = crate::scale::query_display_scale();

    let (boot_w, boot_h) = (boot.physical().w.max(1), boot.physical().h.max(1));
    let (boot_x, boot_y) = boot.position().map_or((0, 0), |p| (p.x, p.y));
    let maximized = boot.maximized();

    let Ok(toplevel) = geo_conn.generate_id() else {
        eprintln!("[x11] failed to allocate top-level window id");
        return false;
    };
    let win_aux = CreateWindowAux::new()
        .background_pixel(black_pixel)
        .event_mask(EventMask::EXPOSURE);
    if geo_conn
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            toplevel,
            root,
            boot_x as i16,
            boot_y as i16,
            boot_w as u16,
            boot_h as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &win_aux,
        )
        .is_err()
    {
        eprintln!("[x11] failed to create top-level window");
        return false;
    }
    let sync_counter = set_toplevel_identity(&geo_conn, toplevel, &atoms);
    if boot.position().is_some() {
        // User-specified hints make the WM honor the restored position
        // instead of applying its own placement policy.
        let mut hints = WmSizeHints::new();
        hints.position = Some((WmSizeHintsSpecification::UserSpecified, boot_x, boot_y));
        hints.size = Some((WmSizeHintsSpecification::UserSpecified, boot_w, boot_h));
        let _ = hints.set_normal_hints(geo_conn.as_ref(), toplevel);
    }

    let Ok(video_host) = geo_conn.generate_id() else {
        eprintln!("[x11] failed to allocate video-host window id");
        return false;
    };
    let host_aux = CreateWindowAux::new().background_pixel(black_pixel);
    if geo_conn
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            video_host,
            toplevel,
            0,
            0,
            boot_w as u16,
            boot_h as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &host_aux,
        )
        .is_err()
    {
        eprintln!("[x11] failed to create video-host window");
        return false;
    }

    if maximized {
        // Pre-map EWMH: the WM reads the initial `_NET_WM_STATE` when it maps
        // the window; client messages only apply to already-mapped windows.
        let _ = geo_conn.change_property32(
            PropMode::REPLACE,
            toplevel,
            atoms.net_wm_state,
            u32::from(AtomEnum::ATOM),
            &[
                atoms.net_wm_state_maximized_vert,
                atoms.net_wm_state_maximized_horz,
            ],
        );
    }
    let _ = geo_conn.map_window(video_host);
    let _ = geo_conn.map_window(toplevel);
    let _ = geo_conn.flush();

    let (parent_x, parent_y, pw, ph) = query_parent_geometry_x11rb(&geo_conn, toplevel, root)
        .unwrap_or((boot_x, boot_y, boot_w, boot_h));

    if !set_host_services(HostServices {
        screen_num,
        root,
        toplevel,
        video_host,
        atoms,
        sync_counter,
    }) {
        eprintln!("[x11] host services already initialized");
        return false;
    }
    let boot_snapshot = ParentSnapshot {
        origin_x: parent_x,
        origin_y: parent_y,
        width: pw,
        height: ph,
        fullscreen: false,
        maximized: false,
        scale,
    };
    crate::x11_state::publish_parent(boot_snapshot);

    let xfixes_opcode = geo_conn
        .query_extension(x11rb::protocol::xfixes::X11_EXTENSION_NAME.as_bytes())
        .ok()
        .and_then(|c| c.reply().ok())
        .filter(|r| r.present)
        .map(|r| r.major_opcode);
    crate::mpv_proxy::set_embed_context(
        video_host,
        net_wm_state,
        xfixes_opcode,
        pw.max(1) as u16,
        ph.max(1) as u16,
    );

    crate::geometry::start(geo_conn, toplevel, video_host, root, boot_snapshot);
    eprintln!("[x11] host window created (toplevel=0x{toplevel:x} video_host=0x{video_host:x})");
    true
}

pub(crate) const COMPOSITOR_NOT_DETECTED_MSG: &str =
    "X11 compositing manager not detected. CEF overlays will not be transparent";
pub(crate) const COMPOSITOR_DETECTED_MSG: &str = "X11 compositing manager detected";

pub(crate) fn cm_atom_name(screen_num: i32) -> String {
    format!("_NET_WM_CM_S{screen_num}")
}

fn compositor_present(conn: &RustConnection, screen_num: i32) -> bool {
    let atom = intern_atom(conn, cm_atom_name(screen_num).as_bytes());
    match conn.get_selection_owner(atom).map(|c| c.reply()) {
        Ok(Ok(reply)) => reply.owner != x11rb::NONE,
        _ => true,
    }
}

pub(crate) fn query_parent_geometry_x11rb(
    conn: &RustConnection,
    parent: u32,
    root: u32,
) -> Option<(i32, i32, i32, i32)> {
    let geo = conn.get_geometry(parent).ok()?.reply().ok()?;
    let trans = conn
        .translate_coordinates(parent, root, 0, 0)
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

/// Platform init. Opens the control/interop connections, finds the ARGB
/// visual, drains the paint tier resolved in [`crate::mpv_host`]'s `prepare`
/// into the state seeded by [`ensure_host_window`], and starts the input
/// thread. mpv is already up and embedded in the video host by the time this
/// runs.
pub fn init() -> bool {
    crate::mpv_proxy::restore_real_display();

    let Some(toplevel) = host().map(|h| h.toplevel) else {
        eprintln!("[x11] host window missing at init");
        return false;
    };

    let (x11rb_conn, screen_num) = match RustConnection::connect(None) {
        Ok((conn, screen_num)) => (std::sync::Arc::new(conn), screen_num as i32),
        Err(e) => {
            eprintln!("[x11] failed to connect x11rb control connection: {e:?}");
            return false;
        }
    };
    if let Err(e) = crate::x11_state::open_xcb_connection() {
        eprintln!("[x11] failed to connect xcb interop/input connection: {e}");
        return false;
    }

    let setup = x11rb_conn.setup();
    let Some(screen) = setup.roots.get(screen_num as usize) else {
        eprintln!("[x11] no screen at index {screen_num}");
        return false;
    };
    let root = screen.root;

    if !compositor_present(&x11rb_conn, screen_num) {
        tracing::error!(target: "Platform", "{COMPOSITOR_NOT_DETECTED_MSG}");
    }

    let argb_depth: u8 = 32;
    let Some(argb_visual) = find_argb_visual(screen) else {
        eprintln!("[x11] no 32-bit ARGB visual found");
        return false;
    };

    let Ok(colormap_id) = x11rb_conn.generate_id() else {
        eprintln!("[x11] failed to allocate colormap id");
        return false;
    };
    let colormap = colormap_id;
    if x11rb_conn
        .create_colormap(
            x11rb::protocol::xproto::ColormapAlloc::NONE,
            colormap_id,
            root,
            argb_visual,
        )
        .is_err()
    {
        eprintln!("[x11] failed to create colormap");
        return false;
    }

    // Verify MIT-SHM 1.2 (fd passing) is present.
    let shm_ok = x11rb_conn
        .shm_query_version()
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|v| (v.major_version, v.minor_version) >= (1, 2));
    if !shm_ok {
        tracing::error!("MIT-SHM 1.2 not available");
        return false;
    }

    if !set_paint_services(PaintServices {
        argb_visual,
        argb_depth,
        colormap,
    }) {
        eprintln!("[x11] paint services already initialized");
        return false;
    }

    if X11RB_CONN.set(x11rb_conn).is_err() {
        eprintln!("[x11] x11rb connection already initialized");
        return false;
    }

    crate::input_lifecycle::start(toplevel);
    crate::menu::warm();

    eprintln!("[x11] platform initialized (toplevel=0x{toplevel:x})");
    true
}

pub fn cleanup() {
    if let Some(selections) = crate::selection::selections() {
        selections.cleanup();
    }
    // Stop every surviving content actor (frees content GCs + SHM + GPU
    // resources on the content connection). Structure teardown (unmap/destroy)
    // rides on the geometry thread's shutdown + the top-level connection close.
    {
        let records: Vec<_> = crate::registry::registry()
            .lock()
            .drain()
            .map(|(_, record)| record)
            .collect();
        for record in records {
            record.actor.shutdown();
        }
    }

    jfn_linux_util::idle_inhibit::cleanup();
    crate::geometry::cleanup();
    crate::input_lifecycle::cleanup();

    if let Some(conn) = crate::x11_state::x11rb_conn() {
        if let Some(colormap) = crate::x11_state::paint().map(|p| p.colormap)
            && colormap != 0
        {
            let _ = conn.free_colormap(colormap);
        }
        let _ = conn.flush();
    }

    crate::mpv_proxy::stop();
}

/// The primary screen's pixel extent, read on a short-lived connection to the
/// real display. `None` when that connection could not be opened.
///
/// Runs before `init()`, so it opens its own connection — the mpv proxy may
/// have `DISPLAY` repointed by then.
pub fn screen_bounds() -> Option<jfn_platform_abi::geometry::Bounds> {
    let display = crate::mpv_proxy::real_display();
    let (conn, screen_num) = RustConnection::connect(display.as_deref()).ok()?;
    let root = conn.setup().roots.get(screen_num)?;
    Some(jfn_platform_abi::geometry::Bounds {
        w: i32::from(root.width_in_pixels),
        h: i32::from(root.height_in_pixels),
    })
}
