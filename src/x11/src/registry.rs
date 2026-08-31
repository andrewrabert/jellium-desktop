//! Owned surface arena, capability newtypes, and the geometry-command queue.
//!
//! # Ownership
//!
//! Each overlay's window is created once and immediately split into two
//! capability handles that never travel together again:
//!
//! - [`StructureSurface`] — place / size / map / restack / override-redirect /
//!   destroy. Held only by the geometry thread (see [`crate::geometry`]); it is
//!   the sole writer of overlay structure. Its ops run on the geometry
//!   connection.
//! - [`ContentSurface`] — pixel upload only (SHM PutImage / GPU present). Moved
//!   into the surface's [`OverlayActor`]; it CANNOT configure geometry.
//!
//! The shared [`SurfaceRegistry`] is a generational arena keyed by
//! [`SurfaceId`]; a freed slot's id can never resolve to its reused successor
//! (slotmap generation check). CEF-facing ops update desired/content state and
//! enqueue a [`GeometryCommand`]; the geometry thread is the sole consumer.

use std::ffi::c_int;
use std::sync::OnceLock;

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::{Condvar, Mutex};
use slotmap::{Key, KeyData, SlotMap, new_key_type};
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt as _, Gcontext, StackMode, Window,
};
use x11rb::rust_connection::RustConnection;

use jfn_platform_abi::{SurfaceHandle, Visibility};

use crate::overlay_actor::OverlayActor;

new_key_type! {
    /// Opaque generational id for one overlay surface. Packs into the ABI
    /// [`SurfaceHandle`] and survives round-trips through CEF's `void*` slot.
    pub struct SurfaceId;
}

impl SurfaceId {
    pub fn to_handle(self) -> SurfaceHandle {
        SurfaceHandle::from_id(self.data().as_ffi())
    }

    pub fn from_handle(h: SurfaceHandle) -> Self {
        Self::from(KeyData::from_ffi(h.id()))
    }
}

/// Structure capability over one overlay window. Held by the geometry thread;
/// every method runs on the geometry connection. This is the ONLY type that may
/// issue `configure_window` / map / unmap against an overlay.
pub(crate) struct StructureSurface {
    window: Window,
}

impl StructureSurface {
    pub(crate) fn window(&self) -> Window {
        self.window
    }

    /// Place + size in one request so the overlay never lands at a mismatched
    /// intermediate rect between two separate configures.
    pub(crate) fn place_and_size(&self, conn: &RustConnection, x: i32, y: i32, w: i32, h: i32) {
        let aux = ConfigureWindowAux::new()
            .x(x)
            .y(y)
            .width(w.max(1) as u32)
            .height(h.max(1) as u32);
        let _ = conn.configure_window(self.window, &aux);
    }

    pub(crate) fn map(&self, conn: &RustConnection) {
        let _ = conn.map_window(self.window);
    }

    pub(crate) fn unmap(&self, conn: &RustConnection) {
        let _ = conn.unmap_window(self.window);
    }

    /// Stack this window immediately above `sibling`.
    pub(crate) fn restack_above(&self, conn: &RustConnection, sibling: Window) {
        let aux = ConfigureWindowAux::new()
            .sibling(sibling)
            .stack_mode(StackMode::ABOVE);
        let _ = conn.configure_window(self.window, &aux);
    }

    /// Raise to the top of the stack (no sibling).
    pub(crate) fn raise(&self, conn: &RustConnection) {
        let aux = ConfigureWindowAux::new().stack_mode(StackMode::ABOVE);
        let _ = conn.configure_window(self.window, &aux);
    }

    pub(crate) fn set_override_redirect(&self, conn: &RustConnection, v: bool) {
        let _ = conn.unmap_window(self.window);
        let aux = ChangeWindowAttributesAux::new().override_redirect(u32::from(v));
        let _ = conn.change_window_attributes(self.window, &aux);
    }

    /// Destroy the window. Consumes the handle so structure teardown happens
    /// exactly once.
    pub(crate) fn destroy(self, conn: &RustConnection) {
        let _ = conn.destroy_window(self.window);
    }
}

/// Content capability over one overlay window: the window + its GC, usable for
/// pixel upload only. Exposes the raw ids the SHM/GPU present paths need but no
/// structure op — the sole-writer grep guard (see the test at the bottom of
/// [`crate::geometry`]) asserts no `configure_window` reaches a content module.
pub(crate) struct ContentSurface {
    window: Window,
    gc: Gcontext,
}

// Raw server ids; the surface is moved onto the actor thread.
unsafe impl Send for ContentSurface {}

impl ContentSurface {
    pub(crate) fn window(&self) -> Window {
        self.window
    }

    pub(crate) fn gc(&self) -> Gcontext {
        self.gc
    }

    /// Free the GC on the content connection. Called from the actor's teardown.
    pub(crate) fn free_gc(&self, conn: &RustConnection) {
        let _ = conn.free_gc(self.gc);
    }
}

/// Build both capability handles from a freshly-created window + GC. This is
/// the single split point; no caller retains the raw `(window, gc)` pair.
pub(crate) fn split_capabilities(
    window: Window,
    gc: Gcontext,
) -> (StructureSurface, ContentSurface) {
    (StructureSurface { window }, ContentSurface { window, gc })
}

/// Per-surface shared record. Holds the content actor; structure state —
/// including the surface's one visibility value — lives on the geometry thread.
pub(crate) struct SurfaceRecord {
    pub(crate) actor: OverlayActor,
    /// Set by `Platform::surface_window_target`. The geometry thread gives an
    /// external window an empty XShape input region and issues no `GrabButton`
    /// on it, and its actor drops every present.
    pub(crate) external: bool,
    pub(crate) window: Option<Window>,
    pub(crate) target_ready: Vec<Box<dyn FnOnce() + Send>>,
    pub(crate) top_physical: i32,
}

pub(crate) struct SurfaceRegistry {
    surfaces: SlotMap<SurfaceId, SurfaceRecord>,
}

impl SurfaceRegistry {
    fn new() -> Self {
        Self {
            surfaces: SlotMap::with_key(),
        }
    }

    pub(crate) fn insert(&mut self, record: SurfaceRecord) -> SurfaceId {
        self.surfaces.insert(record)
    }

    pub(crate) fn get(&self, id: SurfaceId) -> Option<&SurfaceRecord> {
        self.surfaces.get(id)
    }

    pub(crate) fn get_mut(&mut self, id: SurfaceId) -> Option<&mut SurfaceRecord> {
        self.surfaces.get_mut(id)
    }

    /// Remove and return the record, invalidating the public id.
    pub(crate) fn remove(&mut self, id: SurfaceId) -> Option<SurfaceRecord> {
        self.surfaces.remove(id)
    }

    pub(crate) fn drain(&mut self) -> impl Iterator<Item = (SurfaceId, SurfaceRecord)> + '_ {
        self.surfaces.drain()
    }
}

static REGISTRY: OnceLock<Mutex<SurfaceRegistry>> = OnceLock::new();

pub(crate) fn registry() -> &'static Mutex<SurfaceRegistry> {
    REGISTRY.get_or_init(|| Mutex::new(SurfaceRegistry::new()))
}

/// Commands the geometry thread executes as the sole structure writer. Every
/// CEF-facing structure op enqueues one and wakes the geometry thread; there is
/// no timer.
pub(crate) enum GeometryCommand {
    /// Create the window for a reserved id and hand its [`ContentSurface`] to
    /// the actor. The surface is born at `initial`.
    Create { id: SurfaceId, initial: Visibility },
    /// Destroy the window for a freed id (its actor is already stopped).
    Destroy { id: SurfaceId },
    /// Set the surface's visibility (the FSM folds it into map/unmap).
    SetVisibility {
        id: SurfaceId,
        visibility: Visibility,
    },
    /// Replace the bottom-to-top overlay z-order.
    SetOrder { ids: Vec<SurfaceId> },
    /// Reserve `top_physical` pixels at the top of the window for the shell
    /// overlay; the surface is placed and sized below them.
    SetTopInset { id: SurfaceId, top_physical: c_int },
    /// Mark a surface externally presented: empty input region, no grab.
    SetExternal { id: SurfaceId },
}

static QUEUE: OnceLock<Sender<GeometryCommand>> = OnceLock::new();
static QUEUE_RX: OnceLock<Receiver<GeometryCommand>> = OnceLock::new();

/// Serializes ticket minting against the geometry thread's drain so that every
/// command counted by a drain's mark was already sent when the drain ran.
struct Fence {
    seq: Mutex<Seq>,
    applied: Condvar,
}

struct Seq {
    /// Tickets minted so far.
    next: u64,
    /// Highest ticket the geometry thread has applied and round-tripped.
    applied: u64,
}

static FENCE: Fence = Fence {
    seq: Mutex::new(Seq {
        next: 0,
        applied: 0,
    }),
    applied: Condvar::new(),
};

/// Install the command channel. Called once as the geometry thread starts.
pub(crate) fn install_command_channel() {
    let (tx, rx) = unbounded();
    let _ = QUEUE.set(tx);
    let _ = QUEUE_RX.set(rx);
}

/// Enqueue a command and wake the geometry thread, returning the ticket
/// [`wait_applied`] waits on. `None` before the channel exists (pre-boot) or
/// after teardown, where nothing will ever apply the command.
pub(crate) fn enqueue(cmd: GeometryCommand) -> Option<u64> {
    let tx = QUEUE.get()?;
    let ticket = {
        let mut seq = FENCE.seq.lock();
        seq.next += 1;
        if tx.send(cmd).is_err() {
            return None;
        }
        seq.next
    };
    crate::geometry::request_resync();
    Some(ticket)
}

/// Drain all pending commands, with the mark covering every ticket minted so
/// far. Called only by the geometry thread on wake.
pub(crate) fn drain_commands() -> (Vec<GeometryCommand>, u64) {
    let Some(rx) = QUEUE_RX.get() else {
        return (Vec::new(), 0);
    };
    let seq = FENCE.seq.lock();
    let mark = seq.next;
    let cmds: Vec<GeometryCommand> = rx.try_iter().collect();
    drop(seq);
    (cmds, mark)
}

/// Announce that every ticket up to `mark` has been applied and round-tripped.
/// Called only by the geometry thread.
pub(crate) fn publish_applied(mark: u64) {
    let mut seq = FENCE.seq.lock();
    if mark > seq.applied {
        seq.applied = mark;
        FENCE.applied.notify_all();
    }
}

/// Block until the geometry thread has applied `ticket`.
pub(crate) fn wait_applied(ticket: u64) {
    let mut seq = FENCE.seq.lock();
    while seq.applied < ticket {
        FENCE.applied.wait(&mut seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_id_handle_round_trips() {
        let mut sm: SlotMap<SurfaceId, u32> = SlotMap::with_key();
        let id = sm.insert(7);
        let handle = id.to_handle();
        assert!(!handle.is_none());
        assert_eq!(SurfaceId::from_handle(handle), id);
    }

    #[test]
    fn freed_handle_cannot_reach_successor() {
        let mut sm: SlotMap<SurfaceId, u32> = SlotMap::with_key();
        let a = sm.insert(1);
        let a_handle = a.to_handle();
        sm.remove(a);
        let b = sm.insert(2);
        // Reused slot, distinct generation → distinct id.
        assert_ne!(a, b);
        // A's stale handle resolves to nothing, never to B.
        let stale = SurfaceId::from_handle(a_handle);
        assert!(sm.get(stale).is_none());
        assert_eq!(sm.get(b), Some(&2));
    }

    // Sole-writer guard: content modules must never issue `configure_window`
    // against an overlay. Only the structure owner (this module's
    // `StructureSurface`) and the geometry thread may. A textual guard is enough
    // to catch an accidental configure creeping back into a content path.
    #[test]
    fn content_modules_do_not_configure_overlays() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        for file in ["surface.rs", "overlay_actor.rs"] {
            let path = format!("{dir}/{file}");
            let src = std::fs::read_to_string(&path).unwrap_or_default();
            assert!(
                !src.contains("configure_window"),
                "{file} must not configure overlay windows (structure is owned by the geometry thread)"
            );
        }
    }
}
