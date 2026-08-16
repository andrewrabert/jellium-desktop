//! `CAMetalLayer`-based per-surface compositor.
//!
//! All AppKit operations must run on the main thread; if Browsers calls
//! alloc/free/apply-stack/resize/set-visibility off-main we `dispatch_sync` (or
//! `dispatch_async` for fire-and-forget) onto the main queue. On this platform
//! CEF's UI thread *is* the app main thread (CEF runs under an external message
//! pump serviced from the main `CFRunLoop`), so alloc, free, stack and
//! present all run there and the `run_on_main_*` helpers take their inline
//! fast path; `surface_resize` / `set_surface_visibility` are the two entry
//! points that genuinely arrive from other threads.
//!
//! Pixels go through `jfn-gpu-paint`, which owns the device and the layer's
//! swapchain. The one rule that adds: configuring a surface *is* a
//! `CAMetalLayer` mutation, so it only ever happens inside a main-thread
//! closure — surface construction and `Surface::resize`, never a present.
//!
//! Per-surface state is owned by `Box<Surface>`. The opaque pointer
//! returned from `macos_alloc_surface` is `Box::into_raw`; `macos_free_surface`
//! reconstitutes via `Box::from_raw` after detaching the AppKit subview. Every
//! entry point resolves its pointer through the registry's live set before
//! dereferencing it, because deferred work reached through `run_on_main_async`
//! may not run until after a free.

use parking_lot::Mutex;
use std::ffi::c_void;
use std::ptr;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSView, NSWindowOrderingMode};
use objc2_foundation::{NSDictionary, NSMutableDictionary, NSNull, NSRect, NSString};
use objc2_quartz_core::{CAAction, CAMetalLayer};

use jfn_compositor_core::stack::SurfaceStack;
use jfn_compositor_core::transition::TransitionGate;
use jfn_gpu_paint::{
    FrameSize, Pixels, PresentFailed, SharedTexture, Surface as Painter, Surfaces, WindowTarget,
};
use jfn_platform_abi::{Ack, JfnRect, PhysicalSize, Presented, Visibility, VisibilityCommit};

use crate::dispatch::{is_main_thread, run_on_main_async, run_on_main_sync};
use crate::init::{jfn_macos_get_input_view, jfn_macos_get_window};

// =====================================================================
// Per-surface state. One per CefLayer (allocated by macos_alloc_surface,
// destroyed by macos_free_surface).
// =====================================================================

struct Surface {
    /// NSView hosting `layer`. Owned (+1 retain) when non-null.
    view: *mut AnyObject,
    /// `CAMetalLayer` the painter presents into. Owned by `view`'s layer
    /// property; non-retained here.
    layer: *mut AnyObject,
    /// `None` until the first present builds it, and again once it is torn
    /// down. Never built for an external surface.
    painter: Mutex<Option<Painter<'static>>>,
    /// Set by [`macos_surface_window_target`]: the caller presents to this
    /// surface's layer itself, so no painter is ever built for it and every
    /// frame handed here is dropped.
    external: AtomicBool,
}

// `view` and `layer` are written once inside the alloc closure and afterwards
// only read or cleared on the main thread; `painter` is behind the mutex.
unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    fn new() -> Self {
        Self {
            view: ptr::null_mut(),
            layer: ptr::null_mut(),
            painter: Mutex::new(None),
            external: AtomicBool::new(false),
        }
    }
}

// =====================================================================
// Surface registry — current stack order bottom-to-top, as last applied
// via macos_apply_stack, plus the live set every entry point resolves its
// pointer through. stack[0] is the cef-main surface for transition gating
// in macos_surface_present.
//
// Stored as raw *mut Surface (not Box) because the same pointer is
// handed to / from C/Rust callers across the vtable. We Box::from_raw
// only when macos_free_surface is called.
// =====================================================================

#[derive(Clone, Copy, PartialEq)]
struct SurfacePtr(*mut Surface);
unsafe impl Send for SurfacePtr {}

static G_SURFACE_STACK: Mutex<SurfaceStack<SurfacePtr>> = Mutex::new(SurfaceStack::new());

/// Whether this pointer is still one of ours. Resolved before every
/// dereference: a `run_on_main_async` closure may not run until after a free.
fn is_live(p: *mut Surface) -> bool {
    G_SURFACE_STACK.lock().live().contains(&SurfacePtr(p))
}

// Fullscreen/resize transition gate. `MacosResizeGate::set_expected` arms the
// expected post-transition size; the present path clears the gate when an incoming
// frame matches it. macOS never captures a pre-resize size (it gates on
// the expected-size match, not a Windows-style begin/end size compare).
static G_GATE: Mutex<TransitionGate> = Mutex::new(TransitionGate::new());

/// macOS's resize-transition gate: it arms an expected post-transition size
/// and never captures a pre-resize one, because the present path clears the
/// gate on the expected-size match.
pub(crate) struct MacosResizeGate;

pub(crate) static MACOS_RESIZE_GATE: MacosResizeGate = MacosResizeGate;

impl jfn_platform_abi::ResizeGate for MacosResizeGate {
    /// Enters the transition without capturing a size.
    fn begin(&self) {
        G_GATE.lock().begin();
    }

    /// The present path clears the gate on a frame matching the expected
    /// size, so there is nothing for an explicit end to do.
    fn end(&self) {}

    fn in_transition(&self) -> bool {
        G_GATE.lock().in_transition()
    }

    fn set_expected(&self, size: jfn_platform_abi::PhysicalSize) {
        G_GATE.lock().set_expected((size.w, size.h));
    }
}

// =====================================================================
// The process's GPU device. Lazy-init on first alloc_surface, on the calling
// thread, before the main-thread bounce.
// =====================================================================

fn gpu() -> Option<&'static Surfaces> {
    Surfaces::init(None)
}

// =====================================================================
// CAMetalLayer + NSView creation. Called from main-thread context only
// (run_on_main_sync inside macos_alloc_surface).
// =====================================================================

unsafe fn create_content_layer(
    content_view: *mut AnyObject,
    frame: NSRect,
    scale: jfn_platform_abi::Scale,
    visibility: Visibility,
) -> (*mut AnyObject, *mut AnyObject) {
    unsafe {
        // SAFETY: callers run inside a main-queue closure.
        let mtm = MainThreadMarker::new_unchecked();

        let view = NSView::initWithFrame(NSView::alloc(mtm), frame);
        // Before the view joins the tree, so a hidden surface never shows a
        // frame it was not asked for.
        view.setHidden(!visibility.is_shown());
        view.setWantsLayer(true);
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        // Device, pixel format, colorspace, drawable size and framebuffer-only
        // belong to the painter, which writes them from its first configure and
        // overwrites anything set behind it.
        let layer = CAMetalLayer::layer();
        layer.setFrame(frame);
        layer.setContentsScale(scale.as_f64());

        // Disable implicit animations on property changes — present writes
        // contents every frame and CA shouldn't cross-fade them.
        let actions = NSMutableDictionary::<NSString, AnyObject>::dictionaryWithCapacity(5);
        let null = NSNull::null();
        for key in [
            "bounds",
            "position",
            "contents",
            "anchorPoint",
            "contentsRect",
        ] {
            let key = NSString::from_str(key);
            actions.setObject_forKey(&null, ProtocolObject::from_ref(&*key));
        }
        // SAFETY: NSNull is CoreAnimation's documented "no action" value; the
        // element type is only a Rust-side claim.
        let actions: Retained<NSDictionary<NSString, ProtocolObject<dyn CAAction>>> =
            Retained::cast_unchecked(actions);
        layer.setActions(Some(&actions));

        view.setLayer(Some(&layer));

        // addSubview:positioned:relativeTo: — order applied by
        // macos_apply_stack later; positionAbove=nil here.
        // SAFETY: `content_view` is the window's contentView.
        let content_view: &NSView = &*content_view.cast::<NSView>();
        content_view.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Above, None);

        // The view keeps its own retain on the layer; ours is dropped here.
        let layer_ptr = Retained::as_ptr(&layer).cast_mut().cast::<AnyObject>();
        (Retained::into_raw(view).cast::<AnyObject>(), layer_ptr)
    }
}

// =====================================================================
// Vtable-exposed compositor functions
// =====================================================================

pub fn macos_alloc_surface(initial: Visibility) -> *mut c_void {
    // Allocate the Surface up front; the AppKit setup happens on the
    // main thread but writes into this stable heap address.
    let surf_ptr = Box::into_raw(Box::new(Surface::new()));
    // Register next to the `Box::into_raw` that mints the pointer and before
    // the bounce, so it is live for exactly as long as the handle is public.
    // `register`, not `add_live`: this platform derives its main surface from
    // `stack.first()`, and seeding `main` here would change is-main — and with
    // it transition gating — before the first stack apply.
    G_SURFACE_STACK.lock().register(SurfacePtr(surf_ptr));

    let s_addr = surf_ptr as usize;
    run_on_main_sync(move || unsafe {
        let win = jfn_macos_get_window();
        if win.is_null() {
            return;
        }
        let content_view: *mut AnyObject = objc2::msg_send![win, contentView];
        if content_view.is_null() {
            return;
        }
        let frame: objc2_foundation::NSRect = objc2::msg_send![content_view, bounds];
        let (view, layer) =
            create_content_layer(content_view, frame, crate::backend::macos_scale(), initial);
        let surf = &mut *(s_addr as *mut Surface);
        surf.view = view;
        surf.layer = layer;
    });
    surf_ptr as *mut c_void
}

pub fn macos_free_surface(s: *mut c_void) {
    if s.is_null() {
        return;
    }
    let s_ptr = s as *mut Surface;

    // Leave the registry first, on the calling thread: the pointer stops
    // being resolvable before the bounce, so no entry point will reach it
    // again and there is nothing left to race the `Box::from_raw`.
    // `deregister`, not `remove`: `remove`'s live fallback would name a main
    // surface that is not `stack.first()`.
    G_SURFACE_STACK.lock().deregister(SurfacePtr(s_ptr));

    let s_addr = s_ptr as usize;
    run_on_main_sync(move || unsafe {
        let surf = &mut *(s_addr as *mut Surface);
        // First, so wgpu releases its CAMetalLayer retain on the main thread.
        drop(surf.painter.lock().take());
        if !surf.view.is_null() {
            let _: () = objc2::msg_send![surf.view, removeFromSuperview];
            let _: () = objc2::msg_send![surf.view, release];
            surf.view = ptr::null_mut();
        }
        // layer is owned by the view; do not release.
        surf.layer = ptr::null_mut();
    });

    // Reclaim the heap allocation.
    unsafe { drop(Box::from_raw(s_ptr)) };
}

pub fn macos_surface_present(s: *mut c_void, tex: &SharedTexture) -> Option<Presented> {
    present_frame(s, tex.coded(), |painter| painter.present_shared(tex, || {}))
}

pub fn macos_surface_present_software(
    s: *mut c_void,
    pixels: &[u8],
    size: PhysicalSize,
    dirty: &[JfnRect],
) -> Option<Presented> {
    if pixels.is_empty() || size.w <= 0 || size.h <= 0 {
        return None;
    }
    present_frame(
        s,
        FrameSize {
            w: size.w,
            h: size.h,
        },
        |painter| {
            // CEF's OnPaint buffer is tightly packed.
            painter.present_pixels(
                Pixels {
                    size: FrameSize {
                        w: size.w,
                        h: size.h,
                    },
                    stride: size.w as u32 * 4,
                    bgra: pixels,
                    dirty,
                },
                || {},
            )
        },
    )
}

/// Present inline, on whatever thread CEF called us on — which on this
/// platform is the main thread, because that is where CEF's UI thread is.
///
/// Configures nothing: under `ConfigureSite::Owner` the extent is whatever
/// `macos_surface_resize` last set from its main-thread closure. The painter
/// lock is held for the present alone and released before re-entering the
/// registry locks, which is the lock order every other entry point follows.
fn present_frame(
    s: *mut c_void,
    size: FrameSize,
    present: impl FnOnce(&mut Painter<'static>) -> Result<Presented, PresentFailed>,
) -> Option<Presented> {
    if s.is_null() {
        return None;
    }
    warn_once_if_off_main();
    let s_ptr = s as *mut Surface;

    // is-cef-main = bottom-of-stack check, plus the liveness resolve every
    // entry point does — the same lock acquisition doing one more thing.
    let is_main = {
        let stack = G_SURFACE_STACK.lock();
        if !stack.live().contains(&SurfacePtr(s_ptr)) {
            return None;
        }
        stack.is_main(SurfacePtr(s_ptr))
    };

    if is_main && G_GATE.lock().in_transition() {
        return None;
    }

    {
        // SAFETY: the live set says this pointer is still ours.
        let surf = unsafe { &*s_ptr };
        if surf.external.load(Ordering::Acquire) {
            return None;
        }
        let mut painter = surf.painter.lock();
        if painter.is_none() {
            *painter = build_painter(surf, size);
        }
        match painter.as_mut() {
            Some(painter) => match present(painter) {
                Ok(_presented) => {}
                // The frame never entered the commit stream; CEF owes the
                // successor, which is what a `None` return asks it for.
                Err(PresentFailed::Deferred(_) | PresentFailed::Import | PresentFailed::Kind) => {
                    return None;
                }
                Err(e) => {
                    tracing::error!("[GPU] present failed: {e}");
                    return None;
                }
            },
            None => {
                tracing::warn!("[GPU] present skipped: no painter");
                return None;
            }
        }
    }

    if is_main {
        // Clear the gate when the frame matches the expected post-transition
        // size. `coded` is the allocation size.
        G_GATE.lock().note_present_size((size.w, size.h));
    }
    Some(Presented::issued())
}

fn warn_once_if_off_main() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !is_main_thread() && !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!("[GPU] present arrived off the main thread");
    }
}

/// Builds the surface's swapchain on its `CAMetalLayer`. Runs on the layer's
/// owner thread — the same main thread every present arrives on.
fn build_painter(surf: &Surface, size: FrameSize) -> Option<Painter<'static>> {
    let gpu = gpu()?;
    let layer = NonNull::new(surf.layer.cast::<c_void>())?;
    match gpu.new_surface(WindowTarget::CoreAnimationLayer { layer }, size) {
        Ok(painter) => Some(painter),
        Err(e) => {
            tracing::error!("[GPU] surface creation failed: {e}");
            None
        }
    }
}

/// The `CAMetalLayer` for `s`, or `None` before it exists.
///
/// The first call marks the surface external: [`present_frame`] builds no
/// painter for it and drops every frame.
pub fn macos_surface_window_target(s: *mut c_void) -> Option<WindowTarget> {
    if s.is_null() {
        return None;
    }
    let s_ptr = s as *mut Surface;
    if !is_live(s_ptr) {
        return None;
    }
    // SAFETY: the live set says this pointer is still ours.
    let surf = unsafe { &*s_ptr };
    surf.external.store(true, Ordering::Release);
    drop(surf.painter.lock().take());
    let layer = NonNull::new(surf.layer.cast::<c_void>())?;
    Some(WindowTarget::CoreAnimationLayer { layer })
}

pub fn macos_surface_resize(s: *mut c_void, size: jfn_platform_abi::SurfaceSize) {
    if s.is_null() {
        return;
    }
    let physical = size.extent.physical();
    let (pw, ph) = (physical.w, physical.h);
    let s_addr = s as usize;
    run_on_main_async(move || unsafe {
        let s_ptr = s_addr as *mut Surface;
        if !is_live(s_ptr) {
            return;
        }
        let surf = &*s_ptr;
        if surf.view.is_null() || surf.layer.is_null() {
            return;
        }
        let win = jfn_macos_get_window();
        if !win.is_null() {
            let content_view: *mut AnyObject = objc2::msg_send![win, contentView];
            if !content_view.is_null() {
                let bounds: objc2_foundation::NSRect = objc2::msg_send![content_view, bounds];
                let _: () = objc2::msg_send![surf.view, setFrame: bounds];
            }
        }
        let _: () = objc2::msg_send![surf.layer, setContentsScale: size.extent.scale().as_f64()];
        if pw > 0 && ph > 0 {
            // The drawable resize, on the thread that owns the layer.
            if let Some(painter) = surf.painter.lock().as_mut() {
                painter.resize(FrameSize { w: pw, h: ph });
            }
        }
    });
}

/// Hides or shows `s`'s view, and nothing else: the view keeps its frame and
/// its place in the stack, so a surface that comes back is exactly where it
/// was.
///
/// The change is wrapped in an explicit `CATransaction` and flushed before the
/// main-thread closure returns, so the commit carrying it has been issued by
/// the time the returned [`VisibilityCommit`] exists — hence
/// [`Ack::immediate`].
pub fn macos_set_surface_visibility(s: *mut c_void, visibility: Visibility) -> VisibilityCommit {
    if !s.is_null() {
        let s_addr = s as usize;
        run_on_main_sync(move || unsafe {
            let s_ptr = s_addr as *mut Surface;
            if !is_live(s_ptr) {
                return;
            }
            let surf = &*s_ptr;
            if surf.view.is_null() {
                return;
            }
            let transaction = objc2::class!(CATransaction);
            let _: () = objc2::msg_send![transaction, begin];
            let _: () = objc2::msg_send![surf.view, setHidden: !visibility.is_shown()];
            let _: () = objc2::msg_send![transaction, commit];
            let _: () = objc2::msg_send![transaction, flush];
        });
    }
    VisibilityCommit::issued(visibility, Ack::immediate())
}

pub fn macos_apply_stack(ordered: *const *mut c_void, n: usize) {
    // Copy the order into a Vec<usize> we can move into the closure.
    let order: Vec<usize> = if ordered.is_null() || n == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(ordered, n) }
            .iter()
            .map(|p| *p as usize)
            .collect()
    };

    let apply = move || unsafe {
        // Drop anything the caller named that is not (or is no longer) ours,
        // so the views below are only dereferenced for live surfaces.
        let order: Vec<usize> = {
            let mut stack = G_SURFACE_STACK.lock();
            let order: Vec<usize> = order
                .iter()
                .copied()
                .filter(|p| stack.live().contains(&SurfacePtr(*p as *mut Surface)))
                .collect();
            let ordered: Vec<SurfacePtr> = order
                .iter()
                .map(|p| SurfacePtr(*p as *mut Surface))
                .collect();
            stack.replace_stack(&ordered);
            order
        };
        let win = jfn_macos_get_window();
        if win.is_null() {
            return;
        }
        let content_view: *mut AnyObject = objc2::msg_send![win, contentView];
        if content_view.is_null() {
            return;
        }
        let mut prev: *mut AnyObject = ptr::null_mut();
        for raw in &order {
            let s_ptr = *raw as *mut Surface;
            let view = (*s_ptr).view;
            if view.is_null() {
                continue;
            }
            // NSWindowAbove == 1.
            let _: () = objc2::msg_send![
                content_view,
                addSubview: view,
                positioned: 1u64,
                relativeTo: prev,
            ];
            prev = view;
        }
        // Keep the input view on top of every CefLayer.
        let input_view = jfn_macos_get_input_view();
        if !input_view.is_null() {
            let _: () = objc2::msg_send![
                content_view,
                addSubview: input_view,
                positioned: 1u64,
                relativeTo: prev,
            ];
        }
    };
    // The order must be applied before the caller proceeds — use sync.
    run_on_main_sync(apply);
}

// =====================================================================
// Compositor teardown — called from C++ macos_cleanup via the narrow
// jfn_macos_compositor_cleanup accessor. Drops any stragglers and clears
// the stack.
// =====================================================================

pub fn jfn_macos_compositor_cleanup() {
    // Detach lingering subviews + release retained AppKit objects.
    let stragglers: Vec<usize> = G_SURFACE_STACK
        .lock()
        .take_stack()
        .iter()
        .map(|e| e.0 as usize)
        .collect();
    for raw in stragglers {
        if raw == 0 {
            continue;
        }
        unsafe {
            let surf = &mut *(raw as *mut Surface);
            // First, for the same retain-release reason as the free path.
            drop(surf.painter.lock().take());
            if !surf.view.is_null() {
                let _: () = objc2::msg_send![surf.view, removeFromSuperview];
                let _: () = objc2::msg_send![surf.view, release];
                surf.view = ptr::null_mut();
            }
            surf.layer = ptr::null_mut();
            // We don't own the Box — Browsers will call free_surface on
            // each surface during its own teardown, which will reclaim
            // the heap allocation. We just zero our cached AppKit refs.
        }
    }

    G_GATE.lock().end();
}
