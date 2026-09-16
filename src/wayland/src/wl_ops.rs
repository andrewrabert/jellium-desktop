use jfn_gpu_paint::SharedTexture;
use jfn_platform_abi::{Ack, Content, PaintFrame, Presented, Visibility, VisibilityCommit};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

use crate::layer::{LayerSurface, SurfaceRef, ViewportState};
use crate::layer_actor::{LayerActor, LayerBackend};
use crate::runtime::WlRuntime;
use crate::wl_state::{PlatformSurface, WlState, size_in_tolerance};

fn core(rt: &WlRuntime) -> Option<parking_lot::MutexGuard<'_, WlState>> {
    rt.try_core().map(parking_lot::Mutex::lock)
}

fn new_boxed(visibility: Visibility) -> *mut PlatformSurface {
    Box::into_raw(Box::new(PlatformSurface::new(visibility)))
}

unsafe fn drop_boxed(p: *mut PlatformSurface) {
    if !p.is_null() {
        drop(unsafe { Box::from_raw(p) });
    }
}

unsafe fn surface_mut<'a>(p: *mut PlatformSurface) -> &'a mut PlatformSurface {
    unsafe { &mut *p }
}

pub(crate) fn alloc_surface(rt: &'static WlRuntime, initial: Visibility) -> *mut PlatformSurface {
    let Some(mut st) = core(rt) else {
        return std::ptr::null_mut();
    };
    let ptr = new_boxed(initial);
    let s = unsafe { surface_mut(ptr) };

    let surface = st.compositor.create_surface(&st.qh, ());

    if let Some(empty) = st.empty_region() {
        surface.set_input_region(Some(empty.wl_region()));
    }

    let viewport = st.viewporter.get_viewport(&surface, &st.qh, ());

    surface.commit();
    st.flush();

    s.layer_actor = Some(build_actor(rt, &st, &surface, &viewport, s.visibility));
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

    {
        let s = unsafe { surface_mut(ptr) };
        if let Some(actor) = s.layer_actor.take() {
            actor.shutdown();
        }
    }

    {
        let Some(mut st) = core(rt) else { return };
        st.stack.retain(|p| *p != ptr);

        crate::scene::dispatch(
            rt,
            &mut st,
            crate::scene::SceneEvent::LayerRemoved(crate::scene::LayerId(ptr as usize)),
        );

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
    crate::scene::dispatch(rt, &mut st, crate::scene::SceneEvent::Order(order));
}

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
    drop(st);
    rt.root().request_present();
    commit
}

pub(crate) fn surface_resize(
    rt: &'static WlRuntime,
    ptr: *mut PlatformSurface,
    size: jfn_platform_abi::SurfaceSize,
) {
    if ptr.is_null() {
        return;
    }
    let Some(st) = core(rt) else { return };
    let s = unsafe { surface_mut(ptr) };
    let logical = size.extent.logical();
    let physical = size.extent.physical();
    s.top_logical = size.logical_top.max(0);
    s.top_physical = size.physical_top.max(0);
    if let Some(sub) = s.subsurface.as_ref() {
        sub.set_position(0, s.top_logical);
    }
    if let Some(surface) = s.surface.as_ref() {
        surface.set_destination(logical.w, logical.h);
    }
    if let Some(actor) = s.layer_actor.as_ref() {
        actor.resize(logical.w, logical.h, physical.w, physical.h);
    }
    st.flush();
}

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

fn inset_extent(
    s: &PlatformSurface,
    extent: crate::window_state::WindowExtentSnapshot,
) -> (i32, i32, i32, i32) {
    (
        extent.logical().w(),
        (extent.logical().h() - s.top_logical).max(1),
        extent.physical().w(),
        (extent.physical().h() - s.top_physical).max(1),
    )
}

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

fn accepts(
    rt: &'static WlRuntime,
    st: &WlState,
    s: &PlatformSurface,
    content: &Content<'_>,
) -> Option<crate::window_state::WindowExtentSnapshot> {
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
    let (lw, lh, pw, ph) = inset_extent(s, extent);
    let Some(actor) = s.layer_actor.as_ref() else {
        return Err(frame);
    };
    actor.resize(lw, lh, pw, ph);
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
        let (slw, slh, spw, sph) = (
            lw,
            (lh - s.top_logical).max(1),
            pw,
            (ph - s.top_physical).max(1),
        );
        if let Some(actor) = s.layer_actor.as_ref() {
            actor.resize(slw, slh, spw, sph);
        }
    }

    st.flush();
}
