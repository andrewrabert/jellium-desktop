//! Surface lifecycle + paint ops.
//!
//! All entry points run under the runtime's `WlState` mutex. Each
//! protocol-touching op calls `WlState::flush()` (or `conn.flush()`)
//! before returning so commits land in compositor order matching the
//! C++ original.

use jfn_gpu_paint::SharedTexture;
use jfn_platform_abi::JfnRect;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

use crate::layer::{LayerSurface, Present, PresentError, SurfaceRef, ViewportState};
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
fn new_boxed() -> *mut PlatformSurface {
    Box::into_raw(Box::new(PlatformSurface::new()))
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

pub(crate) fn alloc_surface(rt: &'static WlRuntime) -> *mut PlatformSurface {
    // Take the lock before allocating: bailing out afterwards would leak the box.
    let Some(mut st) = core(rt) else {
        return std::ptr::null_mut();
    };
    let ptr = new_boxed();
    // SAFETY: ptr is freshly heap-allocated; no aliases yet.
    let s = unsafe { surface_mut(ptr) };

    let surface = st.compositor.create_surface(&st.qh, ());

    // No input region on subsurface — keystrokes/clicks go to parent only.
    if let Some(empty) = st.empty_region() {
        surface.set_input_region(Some(empty.wl_region()));
    }

    let viewport = st
        .viewporter
        .as_ref()
        .map(|vp| vp.get_viewport(&surface, &st.qh, ()));

    surface.commit();
    st.flush();

    s.layer_actor = Some(build_actor(rt, &st, &surface, &viewport, s.visible));
    s.surface = Some(SurfaceRef::new(surface, viewport));
    crate::wl_state::parent_layer(&mut st, ptr);

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

pub(crate) fn restack(rt: &'static WlRuntime, ordered: &[*mut PlatformSurface]) {
    let Some(mut st) = core(rt) else { return };
    st.stack.clear();
    st.stack.extend_from_slice(ordered);
    let order: Vec<crate::scene::LayerId> = ordered
        .iter()
        .filter(|p| !p.is_null())
        .map(|p| crate::scene::LayerId(*p as usize))
        .collect();
    crate::scene::dispatch(rt, &mut st, crate::scene::SceneEvent::Restack(order));
}

// =====================================================================
// set_visible
// =====================================================================

pub(crate) fn surface_set_visible(
    rt: &'static WlRuntime,
    ptr: *mut PlatformSurface,
    visible: bool,
    bg_r: u8,
    bg_g: u8,
    bg_b: u8,
) {
    if ptr.is_null() {
        return;
    }
    let Some(st) = core(rt) else { return };
    let s = unsafe { surface_mut(ptr) };
    if s.visible == visible {
        return;
    }
    s.visible = visible;
    if s.surface.is_none() {
        return;
    }

    // Skip the placeholder in GPU mode: Vulkan-WSI owns this surface's buffers,
    // so an shm placeholder would fight the swapchain.
    let use_gpu_paint = st.use_gpu_paint;
    if let Some(actor) = s.layer_actor.as_ref() {
        actor.set_visible(visible);
        if visible && !use_gpu_paint {
            actor.request_placeholder(bg_r, bg_g, bg_b);
        }
    }
    s.null_attached = !visible;
    rt.root().request_present();
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
    viewport: &Option<WpViewport>,
    visible: bool,
) -> LayerActor {
    let backend = match (st.use_gpu_paint, st.gpu) {
        (true, Some(ctx)) => LayerBackend::Gpu(ctx),
        _ => LayerBackend::Shm,
    };
    let (lw, lh, pw, ph) = extent_or(rt, 0, 0);
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
        ViewportState { lw, lh, pw, ph },
        visible,
    )
}

fn extent_or(rt: &WlRuntime, w: i32, h: i32) -> (i32, i32, i32, i32) {
    rt.window().window_extent().map_or((w, h, w, h), |ext| {
        (
            ext.logical().w(),
            ext.logical().h(),
            ext.physical().w(),
            ext.physical().h(),
        )
    })
}

pub(crate) fn surface_present(
    rt: &'static WlRuntime,
    ptr: *mut PlatformSurface,
    frame: SharedTexture,
) -> Result<Present, PresentError> {
    if ptr.is_null() {
        return Ok(Present::Skipped);
    }
    let (w, h) = (frame.coded().w, frame.coded().h);
    let (vw, vh) = (frame.visible_rect().w, frame.visible_rect().h);

    let Some(st) = core(rt) else {
        return Ok(Present::Skipped);
    };
    let s = unsafe { surface_mut(ptr) };
    if s.surface.is_none() || !s.visible || st.dmabuf.is_none() {
        return Ok(Present::Skipped);
    }
    if !size_in_tolerance(rt, vw, vh) && !s.null_attached {
        return Ok(Present::Skipped);
    }

    s.null_attached = false;
    let (lw, lh, pw, ph) = extent_or(rt, w, h);

    let Some(actor) = s.layer_actor.as_ref() else {
        return Ok(Present::Skipped);
    };
    actor.set_visible(s.visible);
    actor.resize(lw, lh, pw, ph);
    actor.present_dmabuf(frame)
}

pub(crate) fn surface_present_software(
    rt: &'static WlRuntime,
    ptr: *mut PlatformSurface,
    dirty: &[JfnRect],
    pixels: &[u8],
    w: i32,
    h: i32,
) -> Result<Present, PresentError> {
    if ptr.is_null() || w <= 0 || h <= 0 {
        return Err(PresentError::BadDimensions(w, h));
    }

    let Some(_st) = core(rt) else {
        return Ok(Present::Skipped);
    };
    let s = unsafe { surface_mut(ptr) };
    if s.surface.is_none() || !s.visible {
        return Ok(Present::Skipped);
    }

    s.null_attached = false;
    let (lw, lh, pw, ph) = extent_or(rt, w, h);

    let Some(actor) = s.layer_actor.as_ref() else {
        return Ok(Present::Skipped);
    };
    actor.set_visible(s.visible);
    actor.resize(lw, lh, pw, ph);
    actor.present_software(pixels, w, h, dirty)
}

pub(crate) fn on_configure(rt: &'static WlRuntime, fullscreen: bool) {
    let Some(ext) = rt.window().window_extent() else {
        return;
    };
    let (lw, lh) = (ext.logical().w(), ext.logical().h());
    let (pw, ph) = (ext.physical().w(), ext.physical().h());

    let Some(mut st) = core(rt) else { return };

    st.was_fullscreen = fullscreen;

    crate::wl_state::ensure_root_locked(rt, &mut st);

    for &p in &st.stack {
        if p.is_null() {
            continue;
        }
        let s = unsafe { surface_mut(p) };
        if let Some(actor) = s.layer_actor.as_ref() {
            actor.resize(lw, lh, pw, ph);
        }
    }

    st.flush();
}
