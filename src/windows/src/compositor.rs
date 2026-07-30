//! DirectComposition per-surface compositor.
//!
//! Owns all DComp / per-surface state; the pixels themselves go through
//! `jfn-gpu-paint`, which owns the device and every swapchain. The platform
//! module keeps only HWND, cached scale, fullscreen bookkeeping, the WndProc
//! hook, and the input thread; it calls into this module via the narrow
//! `jfn_win_*` accessors at the bottom of the file to initialize, tear down,
//! and drive the transition-locked routines.
//!
//! Threading: the CEF UI thread presents, allocates and frees; the app main
//! thread initializes; the WndProc thread resizes and transitions. `STATE` is
//! what joins them. No wgpu call which waits on the GPU — configure, and
//! dropping a painter — may run while `STATE` is held, because the WndProc
//! blocks on `STATE`.

#![allow(non_snake_case)]

use parking_lot::Mutex;
use std::ffi::{c_int, c_void};
use std::ptr::NonNull;
use std::sync::OnceLock;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows_core::Interface;

use jfn_compositor_core::stack::SurfaceStack;
use jfn_compositor_core::transition::{PresentDecision, TransitionGate};
use jfn_gpu_paint::{
    Frame, FrameSize, Pixels, Presented, SharedTexture, Surface as Painter, Surfaces, WindowTarget,
};
use jfn_platform_abi::{JfnRect, PhysicalSize};

// =====================================================================
// The process's GPU device. Built once and borrowed by every painter —
// which is why it is a `static` rather than a field of `State`: painters
// are `Painter<'static>`.
//
// Built at the first present rather than at init, because the adapter it
// must open on is the one CEF produced the frame on, and a shared handle
// is the only thing that names that adapter.
// =====================================================================

static GPU: OnceLock<Option<Surfaces>> = OnceLock::new();

fn gpu(sample: Option<&SharedTexture>) -> Option<&'static Surfaces> {
    GPU.get_or_init(|| Surfaces::init(sample, None)).as_ref()
}

// =====================================================================
// Per-surface state. Stored as `Box<Surface>` and exposed across the C
// ABI as the opaque `*mut c_void` PlatformSurface pointer.
// =====================================================================

pub(crate) struct Surface {
    visual: Option<IDCompositionVisual>,
    /// `None` means "not built yet, or checked out by the present in
    /// flight" — the two are told apart by whether the surface is live.
    painter: Option<Painter<'static>>,
    visible: bool,
    in_tree: bool,

    popup_visual: Option<IDCompositionVisual>,
    popup_painter: Option<Painter<'static>>,
    popup_visible: bool,
}

impl Surface {
    fn new() -> Self {
        Self {
            visual: None,
            painter: None,
            visible: true,
            in_tree: false,
            popup_visual: None,
            popup_painter: None,
            popup_visible: false,
        }
    }
}

// =====================================================================
// Shared compositor state. Mutex order: any caller that wants to touch
// `State.surfaces` / per-surface visuals must hold STATE.lock(). Equivalent
// of the C++ `g_win.surface_mtx`.
// =====================================================================

struct CompositorDevices {
    dcomp_device: IDCompositionDevice,
    // Held only to keep the composition target (and its bound root) alive for
    // the lifetime of the compositor; never read after construction.
    #[allow(dead_code)]
    dcomp_target: IDCompositionTarget,
    dcomp_root: IDCompositionVisual,
}

// COM interfaces are Send+Sync-by-COM-spec for the apartment we created them
// in. We serialize all access under STATE's Mutex so the apartment-confinement
// isn't violated.
unsafe impl Send for CompositorDevices {}
unsafe impl Send for Surface {}

struct State {
    devices: Option<CompositorDevices>,
    // Surface registry (live + stack order + main) shared with macOS via
    // jfn-compositor-core.
    surfaces: SurfaceStack<*mut Surface>,
    // Fullscreen/resize transition gate (was G_TRANSITIONING + expected_w/h +
    // transition_pw/ph), kept inside this single STATE lock.
    gate: TransitionGate,
    mpv_pw: i32,
    mpv_ph: i32,
    pending_lw: i32,
    pending_lh: i32,
}

unsafe impl Send for State {}

static STATE: Mutex<State> = Mutex::new(State {
    devices: None,
    surfaces: SurfaceStack::new(),
    gate: TransitionGate::new(),
    mpv_pw: 0,
    mpv_ph: 0,
    pending_lw: 0,
    pending_lh: 0,
});

/// Whether the main surface is currently gated. Takes the STATE lock, so
/// callers must not already hold it (none do).
pub(crate) fn gate_in_transition() -> bool {
    STATE.lock().gate.in_transition()
}

// =====================================================================
// Compositor init/cleanup — called from win_init/win_cleanup (C++).
// =====================================================================

/// Build the DComp device and the root visual. Returns false on failure with
/// the partial state torn down.
///
/// The GPU device is deliberately not built here: *whether* this machine has a
/// usable adapter is knowable now, but *which* adapter to open is not — that
/// waits for CEF's first frame.
pub fn jfn_win_init_compositor(hwnd: *mut c_void) -> bool {
    let hwnd = HWND(hwnd);
    if !jfn_gpu_paint::any_adapter() {
        tracing::error!(target: "platform", "compositor init failed: no usable GPU adapter");
        return false;
    }
    let mut st = STATE.lock();
    if st.devices.is_some() {
        return true;
    }
    match init_devices(hwnd) {
        Ok(d) => {
            st.devices = Some(d);
            true
        }
        Err(e) => {
            tracing::error!(target: "platform", "compositor init failed: {e:?}");
            false
        }
    }
}

fn init_devices(hwnd: HWND) -> windows_core::Result<CompositorDevices> {
    unsafe {
        // NULL rendering device: this compositor only builds a visual tree,
        // and the swapchains under it belong to wgpu.
        let dcomp_device: IDCompositionDevice =
            DCompositionCreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIDevice>)?;
        let dcomp_target = dcomp_device.CreateTargetForHwnd(hwnd, false)?;
        let dcomp_root = dcomp_device.CreateVisual()?;
        dcomp_target.SetRoot(&dcomp_root)?;
        dcomp_device.Commit()?;

        Ok(CompositorDevices {
            dcomp_device,
            dcomp_target,
            dcomp_root,
        })
    }
}

/// Release all surfaces + devices. Called from win_cleanup (C++) after the
/// WndProc hook is unhooked and the input thread is joined.
pub fn jfn_win_cleanup_compositor() {
    // Painters are dropped after STATE is released: dropping one unconfigures
    // its swapchain, which waits for the present queue to idle.
    let orphans = {
        let mut st = STATE.lock();
        let mut orphans = Vec::new();
        // Free any remaining surfaces. Browsers should normally free them
        // first, but be defensive.
        let live: Vec<*mut Surface> = st.surfaces.take_live();
        for ptr in live {
            if ptr.is_null() {
                continue;
            }
            // SAFETY: we own these pointers via Box::into_raw.
            unsafe {
                let mut s = Box::from_raw(ptr);
                orphans.push(s.painter.take());
                orphans.push(s.popup_painter.take());
                detach_surface(&mut s, st.devices.as_ref());
                drop(s);
            }
        }
        st.devices = None;
        orphans
    };
    drop(orphans);
}

/// Unbind a surface's visuals from the tree. COM only — no GPU wait — so it is
/// safe under `STATE`; the painters are taken out separately.
fn detach_surface(s: &mut Surface, devices: Option<&CompositorDevices>) {
    unsafe {
        if let Some(pv) = s.popup_visual.as_ref() {
            if let Some(v) = s.visual.as_ref() {
                let _ = v.RemoveVisual(pv);
            }
            let _ = pv.SetContent(None::<&windows_core::IUnknown>);
        }
        s.popup_visual = None;
        if let Some(v) = s.visual.as_ref() {
            if s.in_tree
                && let Some(d) = devices
            {
                let _ = d.dcomp_root.RemoveVisual(v);
            }
            let _ = v.SetContent(None::<&windows_core::IUnknown>);
        }
        s.visual = None;
    }
}

// =====================================================================
// Painter helpers.
// =====================================================================

/// Bind a painter to `visual`. wgpu retains the visual for as long as the
/// painter lives and calls `SetContent` itself from every configure; the
/// caller publishes that with a `Commit`.
fn build_painter(
    visual: &IDCompositionVisual,
    size: FrameSize,
    sample: Option<&SharedTexture>,
) -> Option<Painter<'static>> {
    let gpu = gpu(sample)?;
    let visual = NonNull::new(visual.as_raw())?;
    match gpu.new_surface(WindowTarget::CompositionVisual { visual }, size) {
        Ok(painter) => Some(painter),
        Err(e) => {
            tracing::error!(target: "platform", "gpu_paint surface creation failed: {e}");
            None
        }
    }
}

/// Everything a present needs, taken out from under `STATE` so the present
/// itself — which may configure, and so may wait on the GPU — runs with the
/// lock released.
struct CheckedOut {
    painter: Option<Painter<'static>>,
    visual: IDCompositionVisual,
}

/// Put a painter back, or hand it to the caller to drop when the surface it
/// belonged to was freed mid-present. Returns whether it was re-installed.
fn reinstall(
    st: &mut State,
    p: *mut Surface,
    painter: Painter<'static>,
    popup: bool,
) -> Option<Painter<'static>> {
    if !st.surfaces.live().contains(&p) {
        return Some(painter);
    }
    // SAFETY: the live set says this pointer is still ours.
    let surf = unsafe { &mut *p };
    if popup {
        surf.popup_painter = Some(painter);
    } else {
        surf.painter = Some(painter);
    }
    None
}

// =====================================================================
// Surface lifecycle + stacking.
// =====================================================================

pub fn win_alloc_surface() -> *mut c_void {
    let mut st = STATE.lock();
    let Some(devices) = st.devices.as_ref() else {
        return std::ptr::null_mut();
    };

    let mut s = Box::new(Surface::new());
    {
        unsafe {
            let visual = match devices.dcomp_device.CreateVisual() {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(target: "platform", "CreateVisual failed: {e:?}");
                    return std::ptr::null_mut();
                }
            };

            let popup = devices.dcomp_device.CreateVisual().ok();
            if let Some(pv) = popup.as_ref() {
                let _ = visual.AddVisual(pv, true, None::<&IDCompositionVisual>);
            } else {
                tracing::error!(target: "platform", "CreateVisual(popup) failed");
            }

            match devices
                .dcomp_root
                .AddVisual(&visual, true, None::<&IDCompositionVisual>)
            {
                Ok(()) => s.in_tree = true,
                Err(e) => tracing::error!(target: "platform", "AddVisual failed: {e:?}"),
            }

            s.visual = Some(visual);
            s.popup_visual = popup;

            let _ = devices.dcomp_device.Commit();
        }
    }

    let ptr = Box::into_raw(s);
    st.surfaces.add_live(ptr);
    ptr as *mut c_void
}

pub fn win_free_surface(s: *mut c_void) {
    if s.is_null() {
        return;
    }
    let p = s as *mut Surface;

    // Both painters leave the lock with us and die outside it: dropping one
    // unconfigures its swapchain, which waits for the present queue to idle,
    // and the WndProc thread blocks on STATE.
    let orphans = {
        let mut st = STATE.lock();
        st.surfaces.remove(p);

        let devices = st.devices.as_ref();
        unsafe {
            let mut s_box = Box::from_raw(p);
            let orphans = (s_box.painter.take(), s_box.popup_painter.take());
            detach_surface(&mut s_box, devices);
            if let Some(d) = devices {
                let _ = d.dcomp_device.Commit();
            }
            drop(s_box);
            orphans
        }
    };
    drop(orphans);
}

/// Rebuild the child-list under `dcomp_root` in `ordered` order
/// (bottom -> top). Popup visuals stay nested under their owning surface,
/// so they're not in this list.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn win_restack(ordered: *const *mut c_void, n: usize) {
    let mut st = STATE.lock();
    let Some(dcomp_root) = st.devices.as_ref().map(|d| d.dcomp_root.clone()) else {
        return;
    };

    // Snapshot live pointers so we can detach without holding a borrow of
    // `st` while we mutate per-surface state.
    let live_ptrs: Vec<*mut Surface> = st.surfaces.live().to_vec();
    {
        unsafe {
            for ptr in &live_ptrs {
                if ptr.is_null() {
                    continue;
                }
                let s = &mut **ptr;
                if let Some(v) = s.visual.as_ref()
                    && s.in_tree
                {
                    let _ = dcomp_root.RemoveVisual(v);
                    s.in_tree = false;
                }
            }
        }
    }

    st.surfaces.clear_stack();
    let mut prev_visual: Option<IDCompositionVisual> = None;
    {
        unsafe {
            for i in 0..n {
                let ptr = *ordered.add(i) as *mut Surface;
                if ptr.is_null() {
                    continue;
                }
                let s = &mut *ptr;
                let visual = match s.visual.as_ref() {
                    Some(v) => v.clone(),
                    None => continue,
                };
                let hr = if let Some(prev) = prev_visual.as_ref() {
                    dcomp_root.AddVisual(&visual, true, prev)
                } else {
                    dcomp_root.AddVisual(&visual, false, None::<&IDCompositionVisual>)
                };
                if let Err(e) = hr {
                    tracing::error!(target: "platform", "restack AddVisual failed: {e:?}");
                    continue;
                }
                s.in_tree = true;
                st.surfaces.push_stack(ptr);
                prev_visual = Some(visual);
            }
        }
    }
    st.surfaces.set_main_to_stack_first();
    if let Some(d) = st.devices.as_ref() {
        unsafe {
            let _ = d.dcomp_device.Commit();
        }
    }
}

// =====================================================================
// Per-frame presentation.
// =====================================================================

pub fn win_surface_present(s: *mut c_void, tex: &SharedTexture) -> bool {
    if s.is_null() || tex.handle().is_null() {
        return false;
    }
    present_frame(s as *mut Surface, tex.coded(), false, Frame::Shared(tex))
}

pub fn win_surface_present_software(
    s: *mut c_void,
    pixels: &[u8],
    size: PhysicalSize,
    dirty: &[JfnRect],
) -> bool {
    if s.is_null() || pixels.is_empty() || size.w <= 0 || size.h <= 0 {
        return false;
    }
    present_frame(
        s as *mut Surface,
        FrameSize {
            w: size.w,
            h: size.h,
        },
        false,
        // CEF's OnPaint buffer is tightly packed.
        Frame::Copied(Pixels {
            size: FrameSize {
                w: size.w,
                h: size.h,
            },
            stride: size.w as u32 * 4,
            bgra: pixels,
            dirty,
        }),
    )
}

/// The gates, then the present, then the commit — with the painter checked out
/// of `STATE` for the middle step.
///
/// A present may configure (the first one, and every one after a
/// `content_detached`), and a configure drains the shared device queue and
/// waits for the present queue. `STATE` is what the WndProc blocks on for
/// `WM_SIZE`, so that wait cannot happen underneath it. Nothing changes thread:
/// the same CEF UI thread takes `STATE`, releases it, presents, and takes it
/// again.
fn present_frame(p: *mut Surface, size: FrameSize, popup: bool, frame: Frame<'_>) -> bool {
    let Some(checked_out) = checkout(p, size, popup) else {
        return false;
    };
    let CheckedOut { painter, visual } = checked_out;
    // The frame that opens the device is also what names the adapter to open
    // it on, so it has to reach `build_painter` and not just the present.
    let sample = match &frame {
        Frame::Shared(tex) => Some(*tex),
        Frame::Copied(_) => None,
    };
    let mut painter = match painter.or_else(|| build_painter(&visual, size, sample)) {
        Some(painter) => painter,
        None => return false,
    };

    let presented = painter.present(frame, || {});

    let orphan = {
        let mut st = STATE.lock();
        let orphan = reinstall(&mut st, p, painter, popup);
        // Publishes wgpu's own SetContent when the present configured.
        if let Some(d) = st.devices.as_ref() {
            unsafe {
                let _ = d.dcomp_device.Commit();
            }
        }
        orphan
    };
    drop(orphan);

    match presented {
        Ok(p) => p == Presented::Yes,
        Err(e) => {
            tracing::error!(target: "platform", "gpu_paint present failed: {e}");
            false
        }
    }
}

/// Evaluate every gate under `STATE` and hand back the painter for the caller
/// to present with the lock released. `None` means the frame is rejected.
fn checkout(p: *mut Surface, size: FrameSize, popup: bool) -> Option<CheckedOut> {
    let mut st = STATE.lock();
    if !st.surfaces.live().contains(&p) {
        return None;
    }
    st.devices.as_ref()?;

    if !popup {
        let is_main = st.surfaces.is_main(p);
        // Transition logic applies only to the bottom-most ("main") surface.
        if is_main {
            match st.gate.main_present_decision((size.w, size.h)) {
                PresentDecision::Reject => return None,
                PresentDecision::EndTransitionThenPresent => {
                    // The gate cleared the transition flags; clear the
                    // (write-only) pending logical size too, matching the rest
                    // of end_transition_locked.
                    st.pending_lw = 0;
                    st.pending_lh = 0;
                }
                PresentDecision::Present => {}
            }
        }
        if is_main && st.mpv_pw > 0 && (size.w > st.mpv_pw + 2 || size.h > st.mpv_ph + 2) {
            return None;
        }
    }

    // SAFETY: the live set says this pointer is still ours.
    let surf = unsafe { &mut *p };
    if popup {
        if !surf.popup_visible {
            return None;
        }
        Some(CheckedOut {
            painter: surf.popup_painter.take(),
            visual: surf.popup_visual.as_ref()?.clone(),
        })
    } else {
        if !surf.visible {
            return None;
        }
        Some(CheckedOut {
            painter: surf.painter.take(),
            visual: surf.visual.as_ref()?.clone(),
        })
    }
}

/// CEF content dims are non-authoritative here: a composition visual shows the
/// swapchain 1:1, so the painter takes its extent from each frame and there is
/// nothing for a resize to do.
pub fn win_surface_resize(_s: *mut c_void, _lw: c_int, _lh: c_int, _pw: c_int, _ph: c_int) {}

pub fn win_surface_set_visible(s: *mut c_void, visible: bool) {
    if s.is_null() {
        return;
    }
    let st = STATE.lock();
    let devices = match st.devices.as_ref() {
        Some(d) => d,
        None => return,
    };
    let p = s as *mut Surface;
    if !st.surfaces.live().contains(&p) {
        return;
    }
    let surf = unsafe { &mut *p };
    if surf.visible == visible {
        return;
    }
    surf.visible = visible;
    let visual = match surf.visual.as_ref() {
        Some(v) => v.clone(),
        None => return,
    };
    if !visible {
        // Detach content so we don't display a stale frame when the surface is
        // shown again at a different size. The painter stays — destroying it
        // would wait for the present queue to idle, under STATE — and is told
        // to rebind on its next present.
        detach_content(&visual, surf.painter.as_mut());
    }
    // visible=true: content rebinds on the next present's configure.
    unsafe {
        let _ = devices.dcomp_device.Commit();
    }
}

/// Sever the visual's content and tell the painter to rebind next present.
///
/// wgpu binds the swapchain to the visual inside `configure` and nowhere else,
/// so an owner-side `SetContent(None)` leaves a painter whose extent never
/// moved and whose content is unbound; `content_detached` is what makes the
/// next present configure anyway.
fn detach_content(visual: &IDCompositionVisual, painter: Option<&mut Painter<'static>>) {
    unsafe {
        let _ = visual.SetContent(None::<&windows_core::IUnknown>);
    }
    if let Some(painter) = painter {
        painter.content_detached();
    }
}

// =====================================================================
// Transition state.
// =====================================================================

fn begin_transition_locked(st: &mut State) {
    if !st.gate.begin_capturing_if_idle((st.mpv_pw, st.mpv_ph)) {
        return;
    }
    st.pending_lw = 0;
    st.pending_lh = 0;

    // Detach main surface's content to avoid stale frames while resizing.
    let Some(p) = st.surfaces.main() else {
        return;
    };
    let devices = match st.devices.as_ref() {
        Some(d) => d,
        None => return,
    };
    unsafe {
        let s = &mut *p;
        if let Some(v) = s.visual.as_ref().cloned() {
            detach_content(&v, s.painter.as_mut());
        }
        let _ = devices.dcomp_device.Commit();
    }
}

fn end_transition_locked(st: &mut State) {
    st.gate.end();
    st.pending_lw = 0;
    st.pending_lh = 0;
}

/// Called by `win_begin_transition` (in lib.rs) — replaces the old
/// `win_begin_transition_impl` C++ helper. Takes STATE lock then runs
/// the locked routine.
pub fn jfn_win_begin_transition_locked() {
    let mut st = STATE.lock();
    begin_transition_locked(&mut st);
}

pub fn win_end_transition() {
    let mut st = STATE.lock();
    end_transition_locked(&mut st);
}

pub fn win_set_expected_size(w: c_int, h: c_int) {
    STATE.lock().gate.set_expected((w, h));
}

// =====================================================================
// Accessors used by C++ WndProc / fullscreen helpers.
// =====================================================================

/// Called from the WndProc on WM_SIZE: stores mpv's current physical size
/// (used by oversized-buffer rejection), records the logical size while a
/// transition is in progress, and ends that transition once the window has
/// settled at its new size. `force_end` ends it even if the physical size is
/// unchanged (a fullscreen-style edge that didn't alter the client size).
pub fn jfn_win_update_surface_size(lw: c_int, lh: c_int, pw: c_int, ph: c_int, force_end: bool) {
    let mut st = STATE.lock();
    if st.gate.in_transition() {
        st.pending_lw = lw;
        st.pending_lh = lh;
        if st.gate.note_window_size((pw, ph), force_end) {
            st.pending_lw = 0;
            st.pending_lh = 0;
        }
    }
    st.mpv_pw = pw;
    st.mpv_ph = ph;
}

/// Called from C++ WndProc on WM_SIZE to run begin_transition under the
/// state lock (matches the old win_begin_transition_locked behavior).
pub fn jfn_win_wndproc_begin_transition_locked() {
    let mut st = STATE.lock();
    begin_transition_locked(&mut st);
}

pub fn jfn_win_wndproc_end_transition_locked() {
    let mut st = STATE.lock();
    end_transition_locked(&mut st);
}

// =====================================================================
// Popup helpers.
// =====================================================================

pub fn win_popup_show(s: *mut c_void, x: c_int, y: c_int) {
    if s.is_null() {
        return;
    }
    let _st = STATE.lock();
    let surf = unsafe { &mut *(s as *mut Surface) };
    surf.popup_visible = true;
    if let Some(pv) = surf.popup_visual.as_ref() {
        let scale = crate::platform::win_get_scale();
        unsafe {
            let _ = pv.SetOffsetX2(x as f32 * scale);
            let _ = pv.SetOffsetY2(y as f32 * scale);
        }
    }
}

pub fn win_popup_hide(s: *mut c_void) {
    if s.is_null() {
        return;
    }
    let st = STATE.lock();
    let surf = unsafe { &mut *(s as *mut Surface) };
    surf.popup_visible = false;
    let pv = match surf.popup_visual.as_ref() {
        Some(v) => v.clone(),
        None => return,
    };
    detach_content(&pv, surf.popup_painter.as_mut());
    if let Some(d) = st.devices.as_ref() {
        unsafe {
            let _ = d.dcomp_device.Commit();
        }
    }
}

pub fn win_popup_present(s: *mut c_void, tex: &SharedTexture, _lw: c_int, _lh: c_int) {
    if s.is_null() || tex.handle().is_null() {
        return;
    }
    present_frame(s as *mut Surface, tex.coded(), true, Frame::Shared(tex));
}

pub fn win_popup_present_software(
    s: *mut c_void,
    pixels: &[u8],
    pw: c_int,
    ph: c_int,
    _lw: c_int,
    _lh: c_int,
) {
    if s.is_null() || pixels.is_empty() || pw <= 0 || ph <= 0 {
        return;
    }
    let size = FrameSize { w: pw, h: ph };
    present_frame(
        s as *mut Surface,
        size,
        true,
        // CEF's OnPaint buffer is tightly packed, and an empty dirty list is a
        // full write.
        Frame::Copied(Pixels {
            size,
            stride: pw as u32 * 4,
            bgra: pixels,
            dirty: &[],
        }),
    );
}
