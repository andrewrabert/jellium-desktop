//! CEF-facing per-surface ops. Every structure change is expressed as desired
//! state plus a [`GeometryCommand`] enqueued to the geometry thread (the sole
//! structure writer); pixel presents route to the surface's [`OverlayActor`].
//!
//! None of these entry points configures, maps, or sizes an overlay window —
//! that authority lives entirely in [`crate::geometry`].

use std::ffi::c_int;

use jfn_gpu_paint::SharedTexture;

use crate::overlay_actor::OverlayActor;
use crate::registry::{GeometryCommand, SurfaceId, SurfaceRecord, enqueue, registry};

pub use jfn_platform_abi::JfnRect;

use jfn_playback::shutdown::jfn_shutting_down;

/// Reserve a surface id and its content actor, then ask the geometry thread to
/// create the window. Returns synchronously; the window lands shortly after and
/// the actor buffers/drops frames until it does.
pub fn alloc_surface() -> SurfaceId {
    let actor = OverlayActor::new(true);
    let id = registry().lock().insert(SurfaceRecord {
        actor,
        visible: true,
    });
    enqueue(GeometryCommand::Create { id });
    id
}

/// Stop the content actor, invalidate the id, then ask the geometry thread to
/// destroy the window. Order: (1) remove from the registry (invalidates the
/// public id), (2) stop+join the actor (frees content resources), (3) enqueue
/// structure teardown on the geometry owner.
pub fn free_surface(id: SurfaceId) {
    let record = registry().lock().remove(id);
    if let Some(record) = record {
        record.actor.shutdown();
    }
    enqueue(GeometryCommand::Destroy { id });
}

/// Present a CEF `OnAcceleratedPaint` dmabuf frame. Only reached on the dmabuf
/// tier. The frame is dropped if a resize is mid-flight at the old size (gate),
/// so the last good frame holds until CEF relays out.
pub fn surface_present_shared(id: SurfaceId, frame: SharedTexture) -> bool {
    if jfn_shutting_down() {
        return false;
    }
    // Gate on the visible size; the coded size can be padded.
    let visible = frame.visible();
    let gate_size = (visible.w, visible.h);
    if crate::x11_state::GATE
        .lock()
        .main_present_decision(gate_size)
        == jfn_compositor_core::transition::PresentDecision::Reject
    {
        return false;
    }

    let g = registry().lock();
    let Some(record) = g.get(id) else {
        return false;
    };
    if !record.visible {
        return false;
    }
    record.actor.present_shared(frame)
}

pub fn surface_present_software(
    id: SurfaceId,
    dirty: &[JfnRect],
    pixels: &[u8],
    w: c_int,
    h: c_int,
) -> bool {
    if jfn_shutting_down() {
        return false;
    }

    let g = registry().lock();
    let Some(record) = g.get(id) else {
        return false;
    };
    if !record.visible {
        return false;
    }
    record.actor.present_software(dirty, pixels, w, h)
}

/// CEF content dims are NON-authoritative: overlay size, gate extent, and
/// swapchain target all derive from parent geometry on the geometry thread.
/// This entry point deliberately does nothing structural.
pub fn surface_resize(_id: SurfaceId, _pw: c_int, _ph: c_int) {}

pub fn surface_set_visible(id: SurfaceId, visible: bool) {
    {
        let mut g = registry().lock();
        let Some(record) = g.get_mut(id) else {
            return;
        };
        if record.visible == visible {
            return;
        }
        record.visible = visible;
        record.actor.set_visible(visible);
    }
    // The FSM (on the geometry thread) owns map/unmap; it folds this flag.
    enqueue(GeometryCommand::SetVisible { id, visible });
}

/// Stack `ordered[0..]` above the app top-level, bottom to top.
pub fn restack(ordered: &[SurfaceId]) {
    enqueue(GeometryCommand::SetOrder {
        ids: ordered.to_vec(),
    });
}
