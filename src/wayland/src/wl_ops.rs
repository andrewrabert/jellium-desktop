//! Surface lifecycle + paint ops.
//!
//! All entry points run under the runtime's `WlState` mutex. Each
//! protocol-touching op calls `WlState::flush()` (or `conn.flush()`)
//! before returning so commits land in compositor order matching the
//! C++ original.

use jfn_gpu_paint::SharedTexture;
use jfn_platform_abi::{
    Ack, Content, PaintFrame, Presented, Unrepresentable, Visibility, VisibilityCommit,
};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

use crate::layer::{LayerSurface, SurfaceRef, ViewportState};
use crate::layer_actor::{LayerActor, LayerBackend};
use crate::runtime::WlRuntime;
use crate::wl_state::{PlatformSurface, WlState, size_in_tolerance};

fn core(rt: &WlRuntime) -> Option<parking_lot::MutexGuard<'_, WlState>> {
    rt.try_core().map(parking_lot::Mutex::lock)
}

// =====================================================================
// Lifetime helpers
// =====================================================================

/// The returned pointer is stable for the surface's lifetime; the caller owns
/// it until `free_surface`.
fn new_boxed(
    visibility: Visibility,
    placement: jfn_platform_abi::SurfacePlacement,
) -> *mut PlatformSurface {
    Box::into_raw(Box::new(PlatformSurface::new(visibility, placement)))
}

unsafe fn drop_boxed(p: *mut PlatformSurface) {
    if !p.is_null() {
        drop(unsafe { Box::from_raw(p) });
    }
}

unsafe fn surface_mut<'a>(p: *mut PlatformSurface) -> &'a mut PlatformSurface {
    unsafe { &mut *p }
}

// =====================================================================
// alloc / free / restack
// =====================================================================

pub(crate) fn alloc_surface(
    rt: &'static WlRuntime,
    initial: Visibility,
    placement: jfn_platform_abi::SurfacePlacement,
) -> *mut PlatformSurface {
    // Take the lock before allocating: bailing out afterwards would leak the box.
    let Some(mut st) = core(rt) else {
        return std::ptr::null_mut();
    };
    let ptr = new_boxed(initial, placement);
    // SAFETY: ptr is freshly heap-allocated; no aliases yet.
    let s = unsafe { surface_mut(ptr) };

    let surface = st.compositor.create_surface(&st.qh, ());

    // No input region on subsurface — keystrokes/clicks go to parent only.
    if let Some(empty) = st.empty_region() {
        surface.set_input_region(Some(empty.wl_region()));
    }

    let viewport = st.viewporter.get_viewport(&surface, &st.qh, ());

    surface.commit();
    st.flush();

    s.layer_actor = Some(build_actor(rt, &st, &surface, &viewport, s.visibility));
    s.surface = Some(SurfaceRef::new(surface, viewport));
    crate::wl_state::parent_layer(&mut st, ptr);
    if let Some(extent) = rt.window().window_extent()
        && let Err(e) = place(s, extent)
    {
        tracing::error!(target: "Main", "new surface: {e}; left unplaced");
    }

    crate::scene::dispatch(
        rt,
        &mut st,
        crate::scene::SceneEvent::LayerAdded(crate::scene::LayerId(ptr as usize)),
    );
    drop(st);
    rt.root().request_present();
    ptr
}

pub(crate) fn free_surface(rt: &'static WlRuntime, ptr: *mut PlatformSurface) {
    if ptr.is_null() {
        return;
    }

    // Shut the actor down before taking the lock: Vulkan WSI swapchain teardown
    // dispatches Wayland events, which would deadlock against the held lock.
    {
        let s = unsafe { surface_mut(ptr) };
        if let Some(actor) = s.layer_actor.take() {
            actor.shutdown();
        }
    }

    {
        let Some(mut st) = core(rt) else { return };
        // Drop from stack if still present.
        st.stack.retain(|p| *p != ptr);

        // Update the scene before tearing down wl objects: dismissing a menu
        // anchored here requires this layer's surface to still be alive.
        crate::scene::dispatch(
            rt,
            &mut st,
            crate::scene::SceneEvent::LayerRemoved(crate::scene::LayerId(ptr as usize)),
        );

        // SAFETY: stack drop above guarantees no aliases via stack;
        // caller (C++) guarantees no concurrent use of `ptr`.
        let s = unsafe { surface_mut(ptr) };
        if let Some(sub) = s.subsurface.take() {
            sub.destroy();
        }
        if let Some(surface) = s.surface.take() {
            surface.destroy();
        }
        st.flush();
    }
    unsafe { drop_boxed(ptr) };
}

/// Applies the whole order, bottom first, replacing whatever was applied before.
pub(crate) fn restack(
    rt: &'static WlRuntime,
    ordered: &[*mut PlatformSurface],
) -> Result<(), Unrepresentable> {
    let Some(mut st) = core(rt) else {
        return Ok(());
    };
    st.stack.clear();
    st.stack.extend_from_slice(ordered);
    let placed = match rt.window().window_extent() {
        Some(extent) => place_all(ordered, extent),
        None => Ok(()),
    };
    let order: Vec<crate::scene::LayerId> = ordered
        .iter()
        .filter(|p| !p.is_null())
        .map(|p| crate::scene::LayerId(*p as usize))
        .collect();
    crate::scene::dispatch(rt, &mut st, crate::scene::SceneEvent::Order(order));
    placed
}

// =====================================================================
// visibility
// =====================================================================

/// Writes the surface's one visibility value and hands back the commit carrying
/// it.
///
/// The root read loop takes this lock to dispatch the acknowledgement, so the
/// commit is returned with the lock released and awaited by the caller.
pub(crate) fn set_visibility(
    rt: &'static WlRuntime,
    ptr: *mut PlatformSurface,
    visibility: Visibility,
) -> VisibilityCommit {
    if ptr.is_null() {
        return VisibilityCommit::issued(visibility, Ack::immediate());
    }
    let Some(st) = core(rt) else {
        return VisibilityCommit::issued(visibility, Ack::immediate());
    };
    let s = unsafe { surface_mut(ptr) };
    s.visibility = visibility;
    let Some(actor) = s.layer_actor.as_ref() else {
        return VisibilityCommit::issued(visibility, Ack::immediate());
    };
    let commit = actor.apply_visibility(visibility);
    // Release the lock before the caller can block on the ack: the actor thread
    // takes it to reach the runtime while it services the request.
    drop(st);
    rt.root().request_present();
    commit
}

// =====================================================================
// resize / window target
// =====================================================================

struct Covered {
    top: i32,
    lw: i32,
    lh: i32,
    pw: i32,
    ph: i32,
}

fn covered(
    s: &PlatformSurface,
    extent: crate::window_state::PublishedWindow,
) -> Result<Covered, Unrepresentable> {
    let (logical, physical) = (extent.logical(), extent.physical());
    let top = match s.placement {
        jfn_platform_abi::SurfacePlacement::Window => 0,
        jfn_platform_abi::SurfacePlacement::BelowTitlebar => {
            extent.web_top().unwrap_or(0).clamp(0, logical.h())
        }
    };
    let top_physical = extent.scale().to_physical(top)?.clamp(0, physical.h());
    Ok(Covered {
        top,
        lw: logical.w(),
        lh: (logical.h() - top).max(1),
        pw: physical.w(),
        ph: (physical.h() - top_physical).max(1),
    })
}

fn place(
    s: &PlatformSurface,
    extent: crate::window_state::PublishedWindow,
) -> Result<(), Unrepresentable> {
    let c = covered(s, extent)?;
    if let Some(sub) = s.subsurface.as_ref() {
        sub.set_position(0, c.top);
    }
    if let Some(surface) = s.surface.as_ref() {
        surface.set_destination(c.lw, c.lh);
    }
    if let Some(actor) = s.layer_actor.as_ref() {
        actor.resize(c.lw, c.lh, c.pw, c.ph);
    }
    Ok(())
}

fn place_all(
    surfaces: &[*mut PlatformSurface],
    extent: crate::window_state::PublishedWindow,
) -> Result<(), Unrepresentable> {
    let mut placed = Ok(());
    for &p in surfaces.iter().filter(|p| !p.is_null()) {
        let result = place(unsafe { surface_mut(p) }, extent);
        if placed.is_ok() {
            placed = result;
        }
    }
    placed
}

/// Marks the surface external, desynchronizes it, and hands back its swapchain
/// target.
///
/// Takes the `WlState` lock: every other reader of `PlatformSurface` holds the
/// same lock. Desync is what makes the surface's own commits reach the
/// compositor without a parent commit, which is what makes the frame callbacks
/// a FIFO swapchain throttles on arrive.
pub(crate) fn window_target(
    rt: &'static WlRuntime,
    ptr: *mut PlatformSurface,
) -> Option<jfn_gpu_paint::WindowTarget> {
    if ptr.is_null() {
        return None;
    }
    let st = core(rt)?;
    let s = unsafe { surface_mut(ptr) };
    s.external = true;
    if let Some(sub) = s.subsurface.as_ref() {
        sub.set_desync();
    }
    let target = s.surface.as_ref()?.window_target();
    st.flush();
    target
}

// =====================================================================
// Present (dmabuf / software)
// =====================================================================

/// Identity of the dmabuf behind a frame, for the buffer pool: CEF recycles a
/// small set of buffers, so the same `(dev, ino)` means the same `wl_buffer`
/// can be reattached instead of rebuilt. `None` disables pooling for the frame.
pub(crate) fn dmabuf_pool_key(frame: &SharedTexture) -> Option<(u64, u64)> {
    let plane = frame.planes().first()?;
    nix::sys::stat::fstat(&plane.fd)
        .ok()
        .map(|st| (st.st_dev, st.st_ino))
}

fn build_actor(
    rt: &'static WlRuntime,
    st: &WlState,
    surface: &WlSurface,
    viewport: &WpViewport,
    visibility: Visibility,
) -> LayerActor {
    let backend = match (st.use_gpu_paint, st.gpu) {
        (true, Some(ctx)) => LayerBackend::Gpu(ctx),
        _ => LayerBackend::Shm,
    };
    let layer = LayerSurface::new(st.conn.clone(), surface.clone(), viewport.clone());
    LayerActor::new(
        backend,
        crate::layer_actor::LayerDeps {
            rt,
            qh: st.qh.clone(),
            shm: st.shm.clone(),
            dmabuf: st.dmabuf.clone(),
        },
        layer,
        window_viewport(rt),
        visibility,
    )
}

/// The viewport the published window extent names, or
/// [`ViewportState::UNPUBLISHED`] before the first publish.
fn window_viewport(rt: &WlRuntime) -> ViewportState {
    rt.window()
        .window_extent()
        .map_or(ViewportState::UNPUBLISHED, |ext| ViewportState {
            lw: ext.logical().w(),
            lh: ext.logical().h(),
            pw: ext.physical().w(),
            ph: ext.physical().h(),
        })
}

/// The window extent `content` may be presented against, or `None` when this
/// surface has no commit stream for it right now.
///
/// Everything that can reject a frame is asked here, before the frame is
/// consumed, so no producer is handed a commit proof for a frame that goes
/// nowhere. A window that has published no extent rejects every frame: the
/// frame's own texel size names no scale, and nothing here may name one. A
/// dmabuf frame additionally needs the protocol and a size the window still
/// expects; mid-transition frames of the old size would flash the wrong
/// geometry.
fn accepts(
    rt: &'static WlRuntime,
    st: &WlState,
    s: &PlatformSurface,
    content: &Content<'_>,
) -> Option<crate::window_state::PublishedWindow> {
    if s.surface.is_none() || !s.visibility.is_shown() || s.external || s.layer_actor.is_none() {
        return None;
    }
    let extent = rt.window().window_extent()?;
    match content {
        Content::Accelerated(tex) => {
            st.dmabuf.is_some()
                && tex.coded().w > 0
                && tex.coded().h > 0
                && size_in_tolerance(rt, tex.visible_rect().w, tex.visible_rect().h)
        }
        Content::Software { size, pixels, .. } => {
            size.w > 0
                && size.h > 0
                && (size.h as usize)
                    .checked_mul((size.w as usize).saturating_mul(4))
                    .is_some_and(|need| pixels.len() >= need)
        }
    }
    .then_some(extent)
}

/// Hands `frame` to the surface's actor, or back to its producer when this
/// surface has no commit stream for it.
pub(crate) fn present<'a>(
    rt: &'static WlRuntime,
    ptr: *mut PlatformSurface,
    frame: PaintFrame<'a>,
) -> Result<Presented, PaintFrame<'a>> {
    if ptr.is_null() {
        return Err(frame);
    }
    let Some(st) = core(rt) else {
        return Err(frame);
    };
    let s = unsafe { surface_mut(ptr) };
    let Some(extent) = accepts(rt, &st, s, frame.content()) else {
        return Err(frame);
    };
    let c = match covered(s, extent) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(target: "Main", "surface top: {e}; frame not presented");
            return Err(frame);
        }
    };
    let Some(actor) = s.layer_actor.as_ref() else {
        return Err(frame);
    };
    actor.resize(c.lw, c.lh, c.pw, c.ph);
    // Taken before the frame is consumed: the actor holds a frame the swapchain
    // could not take, and only the producer named here is owed its successor.
    let source = frame.source();
    Ok(frame.present(|content| {
        match content {
            Content::Accelerated(tex) => actor.present_dmabuf(tex, source),
            Content::Software {
                size,
                pixels,
                dirty,
            } => actor.present_software(pixels, size.w, size.h, dirty, source),
        }
        Presented::issued()
    }))
}

pub(crate) fn on_configure(
    rt: &'static WlRuntime,
    fullscreen: bool,
) -> Result<(), Unrepresentable> {
    let Some(ext) = rt.window().window_extent() else {
        return Ok(());
    };

    let Some(mut st) = core(rt) else {
        return Ok(());
    };

    st.was_fullscreen = fullscreen;

    crate::wl_state::ensure_root_locked(rt, &mut st);

    let placed = place_all(&st.stack, ext);

    st.flush();
    placed
}
