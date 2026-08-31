//! CEF-facing per-surface ops. Every structure change is expressed as desired
//! state plus a [`GeometryCommand`] enqueued to the geometry thread (the sole
//! structure writer); pixel presents route to the surface's [`OverlayActor`].
//!
//! None of these entry points configures, maps, or sizes an overlay window —
//! that authority lives entirely in [`crate::geometry`].

use std::ffi::c_int;

use jfn_platform_abi::{Ack, Content, PaintFrame, Presented, Visibility, VisibilityCommit};

use crate::overlay_actor::OverlayActor;
use crate::registry::{GeometryCommand, SurfaceId, SurfaceRecord, enqueue, registry, wait_applied};

pub use jfn_platform_abi::JfnRect;

use jfn_playback::shutdown::jfn_shutting_down;

/// Reserve a surface id and its content actor, then ask the geometry thread to
/// create the window at `initial`. Returns synchronously; the window lands
/// shortly after and the actor drops frames until it does.
pub fn alloc_surface(initial: Visibility) -> SurfaceId {
    let actor = OverlayActor::new();
    let id = registry().lock().insert(SurfaceRecord {
        actor,
        external: false,
        window: None,
        target_ready: Vec::new(),
        top_physical: 0,
    });
    let _ = enqueue(GeometryCommand::Create { id, initial });
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
    let _ = enqueue(GeometryCommand::Destroy { id });
}

/// Hand `frame` to this surface's content actor, or give it back when the
/// surface has no commit stream for it: it is gone, externally presented, the
/// process is shutting down, an accelerated frame is at a size the resize gate
/// rejects, or a software frame's pixels do not cover its size.
pub fn present<'a>(id: SurfaceId, frame: PaintFrame<'a>) -> Result<Presented, PaintFrame<'a>> {
    if jfn_shutting_down() {
        return Err(frame);
    }
    if !presentable(frame.content()) {
        return Err(frame);
    }

    let g = registry().lock();
    let Some(record) = g.get(id) else {
        return Err(frame);
    };
    if record.external {
        return Err(frame);
    }
    // Taken before the frame is consumed: the actor holds a frame the swapchain
    // could not take, and only the producer named here is owed its successor.
    let source = frame.source();
    Ok(frame.present(|content| {
        match content {
            Content::Accelerated(texture) => record.actor.present_shared(texture, source),
            Content::Software {
                size,
                pixels,
                dirty,
            } => record
                .actor
                .present_software(dirty, pixels, size.w, size.h, source),
        }
        Presented::issued()
    }))
}

/// Whether the content can reach the screen at all, checked before the frame is
/// consumed so a rejection can hand it back.
fn presentable(content: &Content<'_>) -> bool {
    match content {
        // Gate on the visible size; the coded size can be padded.
        Content::Accelerated(texture) => {
            let visible = texture.visible();
            crate::x11_state::GATE
                .lock()
                .main_present_decision((visible.w, visible.h))
                != jfn_compositor_core::transition::PresentDecision::Reject
        }
        Content::Software {
            size,
            pixels,
            dirty: _,
        } => {
            if size.w <= 0 || size.h <= 0 {
                return false;
            }
            let stride = (size.w as usize).saturating_mul(4);
            (size.h as usize)
                .checked_mul(stride)
                .is_some_and(|len| pixels.len() >= len)
        }
    }
}

/// Reserve `top_physical` pixels at the top of the window for the shell
/// overlay. The geometry thread applies it on the next reconcile.
pub fn surface_set_top_inset(id: SurfaceId, top_physical: c_int) {
    let _ = enqueue(GeometryCommand::SetTopInset { id, top_physical });
}

/// The swapchain target for `id`, or `None` until the geometry thread has
/// created its window.
///
/// The first call marks the surface external: the geometry thread then gives it
/// an empty XShape input region, issues no `GrabButton` on it, and its actor
/// drops every present.
pub fn window_target(id: SurfaceId) -> Option<jfn_gpu_paint::WindowTarget> {
    let (first, window) = {
        let mut g = registry().lock();
        let record = g.get_mut(id)?;
        let first = !record.external;
        record.external = true;
        (first, record.window)
    };
    if first {
        let _ = enqueue(GeometryCommand::SetExternal { id });
    }
    let window = window?;
    let connection = crate::x11_state::raw_xcb_connection()?;
    let paint = crate::x11_state::paint()?;
    Some(jfn_gpu_paint::WindowTarget::Xcb {
        connection,
        window,
        screen: crate::x11_state::host().map_or(0, |h| h.screen_num),
        visual: paint.argb_visual,
    })
}

/// Registers under the same lock used to publish the native window.
pub fn on_target_ready(id: SurfaceId, ready: Box<dyn FnOnce() + Send>) {
    {
        let mut g = registry().lock();
        if let Some(record) = g.get_mut(id)
            && record.window.is_none()
        {
            record.target_ready.push(ready);
            return;
        }
    }
    ready();
}

/// Enqueue the map/unmap; the returned commit blocks until the geometry thread
/// (the sole owner of map state) has applied it and its server round trip has
/// returned.
pub fn set_visibility(id: SurfaceId, visibility: Visibility) -> VisibilityCommit {
    let ack = match enqueue(GeometryCommand::SetVisibility { id, visibility }) {
        Some(ticket) => Ack::deferred(Box::new(move || wait_applied(ticket))),
        // No geometry thread to apply it: nothing will ever acknowledge.
        None => Ack::immediate(),
    };
    VisibilityCommit::issued(visibility, ack)
}

/// Stack `ordered[0..]` above the app top-level, bottom to top.
pub fn apply_stack(ordered: &[SurfaceId]) {
    let _ = enqueue(GeometryCommand::SetOrder {
        ids: ordered.to_vec(),
    });
}
