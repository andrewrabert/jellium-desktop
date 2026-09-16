use std::ffi::c_int;

use jfn_platform_abi::{Ack, Content, PaintFrame, Presented, Visibility, VisibilityCommit};

use crate::overlay_actor::OverlayActor;
use crate::registry::{GeometryCommand, SurfaceId, SurfaceRecord, enqueue, registry, wait_applied};

pub use jfn_platform_abi::JfnRect;

use jfn_playback::shutdown::jfn_shutting_down;

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

pub fn free_surface(id: SurfaceId) {
    let record = registry().lock().remove(id);
    if let Some(record) = record {
        record.actor.shutdown();
    }
    let _ = enqueue(GeometryCommand::Destroy { id });
}

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

fn presentable(content: &Content<'_>) -> bool {
    match content {
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

pub fn surface_set_top_inset(id: SurfaceId, top_physical: c_int) {
    let _ = enqueue(GeometryCommand::SetTopInset { id, top_physical });
}

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

pub fn set_visibility(id: SurfaceId, visibility: Visibility) -> VisibilityCommit {
    let ack = match enqueue(GeometryCommand::SetVisibility { id, visibility }) {
        Some(ticket) => Ack::deferred(Box::new(move || wait_applied(ticket))),
        None => Ack::immediate(),
    };
    VisibilityCommit::issued(visibility, ack)
}

pub fn apply_stack(ordered: &[SurfaceId]) {
    let _ = enqueue(GeometryCommand::SetOrder {
        ids: ordered.to_vec(),
    });
}
