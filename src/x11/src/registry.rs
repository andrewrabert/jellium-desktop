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

pub(crate) struct StructureSurface {
    window: Window,
}

impl StructureSurface {
    pub(crate) fn window(&self) -> Window {
        self.window
    }

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

    pub(crate) fn restack_above(&self, conn: &RustConnection, sibling: Window) {
        let aux = ConfigureWindowAux::new()
            .sibling(sibling)
            .stack_mode(StackMode::ABOVE);
        let _ = conn.configure_window(self.window, &aux);
    }

    pub(crate) fn raise(&self, conn: &RustConnection) {
        let aux = ConfigureWindowAux::new().stack_mode(StackMode::ABOVE);
        let _ = conn.configure_window(self.window, &aux);
    }

    pub(crate) fn set_override_redirect(&self, conn: &RustConnection, v: bool) {
        let _ = conn.unmap_window(self.window);
        let aux = ChangeWindowAttributesAux::new().override_redirect(u32::from(v));
        let _ = conn.change_window_attributes(self.window, &aux);
    }

    pub(crate) fn destroy(self, conn: &RustConnection) {
        let _ = conn.destroy_window(self.window);
    }
}

pub(crate) struct ContentSurface {
    window: Window,
    gc: Gcontext,
}

unsafe impl Send for ContentSurface {}

impl ContentSurface {
    pub(crate) fn window(&self) -> Window {
        self.window
    }

    pub(crate) fn gc(&self) -> Gcontext {
        self.gc
    }

    pub(crate) fn free_gc(&self, conn: &RustConnection) {
        let _ = conn.free_gc(self.gc);
    }
}

pub(crate) fn split_capabilities(
    window: Window,
    gc: Gcontext,
) -> (StructureSurface, ContentSurface) {
    (StructureSurface { window }, ContentSurface { window, gc })
}

pub(crate) struct SurfaceRecord {
    pub(crate) actor: OverlayActor,
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

pub(crate) enum GeometryCommand {
    Create {
        id: SurfaceId,
        initial: Visibility,
    },
    Destroy {
        id: SurfaceId,
    },
    SetVisibility {
        id: SurfaceId,
        visibility: Visibility,
    },
    SetOrder {
        ids: Vec<SurfaceId>,
    },
    SetTopInset {
        id: SurfaceId,
        top_physical: c_int,
    },
    SetExternal {
        id: SurfaceId,
    },
}

static QUEUE: OnceLock<Sender<GeometryCommand>> = OnceLock::new();
static QUEUE_RX: OnceLock<Receiver<GeometryCommand>> = OnceLock::new();

struct Fence {
    seq: Mutex<Seq>,
    applied: Condvar,
}

struct Seq {
    next: u64,
    applied: u64,
}

static FENCE: Fence = Fence {
    seq: Mutex::new(Seq {
        next: 0,
        applied: 0,
    }),
    applied: Condvar::new(),
};

pub(crate) fn install_command_channel() {
    let (tx, rx) = unbounded();
    let _ = QUEUE.set(tx);
    let _ = QUEUE_RX.set(rx);
}

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

pub(crate) fn publish_applied(mark: u64) {
    let mut seq = FENCE.seq.lock();
    if mark > seq.applied {
        seq.applied = mark;
        FENCE.applied.notify_all();
    }
}

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
        assert_ne!(a, b);
        let stale = SurfaceId::from_handle(a_handle);
        assert!(sm.get(stale).is_none());
        assert_eq!(sm.get(b), Some(&2));
    }

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
