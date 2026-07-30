//! `CAMetalLayer`-based per-surface compositor.
//!
//! All AppKit operations must run on the main thread; if Browsers calls
//! alloc/free/restack/resize/set_visible off-main we `dispatch_sync` (or
//! `dispatch_async` for fire-and-forget) onto the main queue. On this platform
//! CEF's UI thread *is* the app main thread (CEF runs under an external message
//! pump serviced from the main `CFRunLoop`), so alloc, free, restack and
//! present all run there and the `run_on_main_*` helpers take their inline
//! fast path; `surface_resize` / `surface_set_visible` are the two entry points
//! that genuinely arrive from other threads.
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
use std::ffi::{c_int, c_void};
use std::ptr;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::runtime::AnyObject;

use jfn_compositor_core::stack::SurfaceStack;
use jfn_compositor_core::transition::TransitionGate;
use jfn_gpu_paint::{
    Frame, FrameSize, Pixels, SharedTexture, Surface as Painter, Surfaces, WindowTarget,
};
use jfn_platform_abi::{JfnRect, PhysicalSize};

use crate::init::{jfn_macos_get_input_view, jfn_macos_get_window};

unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(
        queue: *mut c_void,
        ctx: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );

    // dispatch_sync_f — bounce onto the main queue and block until the
    // work item returns. Used by macos_alloc_surface / macos_free_surface
    // / macos_restack which need their AppKit mutations done before
    // returning to the caller.
    fn dispatch_sync_f(
        queue: *mut c_void,
        ctx: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
}

#[inline]
fn dispatch_get_main_queue() -> *mut c_void {
    std::ptr::addr_of!(_dispatch_main_q) as *mut c_void
}

fn is_main_thread() -> bool {
    unsafe {
        let cls = objc2::class!(NSThread);
        let b: bool = objc2::msg_send![cls, isMainThread];
        b
    }
}

/// Build an NSString from a Rust &str (UTF-8). The returned object is
/// retained (+1) by NSString init; the caller `release`s when done.
unsafe fn nsstring_from_str(s: &str) -> *mut AnyObject {
    unsafe {
        let bytes = s.as_bytes();
        let alloc: *mut AnyObject = objc2::msg_send![objc2::class!(NSString), alloc];
        let init: *mut AnyObject = objc2::msg_send![
            alloc,
            initWithBytes: bytes.as_ptr() as *const c_void,
            length: bytes.len(),
            encoding: 4u64 // NSUTF8StringEncoding
        ];
        init
    }
}

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
    /// `None` until the layer exists, and again once it is torn down.
    painter: Mutex<Option<Painter<'static>>>,
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
        }
    }
}

// =====================================================================
// Surface registry — current stack order bottom-to-top, as last applied
// via macos_restack, plus the live set every entry point resolves its
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

// Fullscreen/resize transition gate. set_expected_size arms the expected
// post-transition size; the present path clears the gate when an incoming
// frame matches it. macOS never captures a pre-resize size (it gates on
// the expected-size match, not a Windows-style begin/end size compare).
static G_GATE: Mutex<TransitionGate> = Mutex::new(TransitionGate::new());

/// Enter the transition (set by `macos_begin_transition` in lib.rs).
pub(crate) fn gate_begin() {
    G_GATE.lock().begin();
}

/// Whether the main surface is currently gated (read by `macos_in_transition`
/// and the present path).
pub(crate) fn gate_in_transition() -> bool {
    G_GATE.lock().in_transition()
}

// =====================================================================
// The process's GPU device. Lazy-init on first alloc_surface, on the calling
// thread, before the main-thread bounce.
// =====================================================================

static GPU: OnceLock<Option<Surfaces>> = OnceLock::new();

fn gpu() -> Option<&'static Surfaces> {
    GPU.get_or_init(|| Surfaces::init(None, None)).as_ref()
}

/// Run a closure on the AppKit main thread. Used for layer-tree mutations
/// (subview attach, frame writes, etc.). Sync — caller blocks until the
/// closure returns. Safe to call from the main thread (runs inline).
///
/// The closure runs strictly on the main thread; raw pointers it
/// captures don't actually cross threads (sync blocks until the work
/// item returns). We therefore drop the `Send` bound and shuttle the
/// closure pointer through `usize` to satisfy dispatch_sync_f's C ABI.
fn run_on_main_sync<F>(f: F)
where
    F: FnOnce(),
{
    if is_main_thread() {
        f();
        return;
    }
    let boxed: Box<dyn FnOnce()> = Box::new(f);
    let dbl_box: Box<Box<dyn FnOnce()>> = Box::new(boxed);
    let ptr_ctx = Box::into_raw(dbl_box) as *mut c_void;
    unsafe extern "C" fn trampoline(ctx: *mut c_void) {
        unsafe {
            let dbl_box: Box<Box<dyn FnOnce()>> = Box::from_raw(ctx as *mut _);
            let inner = *dbl_box;
            inner();
        }
    }
    unsafe { dispatch_sync_f(dispatch_get_main_queue(), ptr_ctx, trampoline) };
}

/// Async version — fire-and-forget; the closure runs later on the main
/// queue. Used by macos_surface_set_visible / macos_surface_resize where
/// the caller does not need ordering. Closure must be `'static`.
fn run_on_main_async<F>(f: F)
where
    F: FnOnce() + 'static,
{
    if is_main_thread() {
        f();
        return;
    }
    let boxed: Box<dyn FnOnce()> = Box::new(f);
    let dbl_box: Box<Box<dyn FnOnce()>> = Box::new(boxed);
    let ptr_ctx = Box::into_raw(dbl_box) as *mut c_void;
    unsafe extern "C" fn trampoline(ctx: *mut c_void) {
        unsafe {
            let dbl_box: Box<Box<dyn FnOnce()>> = Box::from_raw(ctx as *mut _);
            let inner = *dbl_box;
            inner();
        }
    }
    unsafe { dispatch_async_f(dispatch_get_main_queue(), ptr_ctx, trampoline) };
}

// =====================================================================
// CAMetalLayer + NSView creation. Called from main-thread context only
// (run_on_main_sync inside macos_alloc_surface).
// =====================================================================

unsafe fn create_content_layer(
    content_view: *mut AnyObject,
    frame: objc2_foundation::NSRect,
    scale: f64,
) -> (*mut AnyObject, *mut AnyObject) {
    unsafe {
        // NSView alloc/initWithFrame:
        let view_cls = objc2::class!(NSView);
        let view: *mut AnyObject = objc2::msg_send![view_cls, alloc];
        let view: *mut AnyObject = objc2::msg_send![view, initWithFrame: frame];
        let _: () = objc2::msg_send![view, setWantsLayer: true];
        // NSViewWidthSizable | NSViewHeightSizable = 2 | 16 (per AppKit).
        let _: () = objc2::msg_send![view, setAutoresizingMask: 2u64 | 16u64];

        // CAMetalLayer alloc + geometry. Device, pixel format, colorspace,
        // drawable size and framebuffer-only belong to the painter, which
        // writes them from its first configure and overwrites anything set
        // behind it.
        let layer_cls = objc2::class!(CAMetalLayer);
        let layer: *mut AnyObject = objc2::msg_send![layer_cls, layer];
        let _: () = objc2::msg_send![layer, setFrame: frame];
        let _: () = objc2::msg_send![layer, setContentsScale: scale];

        // Disable implicit animations on property changes — present
        // writes contents every frame and CA shouldn't cross-fade them.
        // Build an NSDictionary { "bounds": NSNull, ... }.
        let null_cls = objc2::class!(NSNull);
        let null_obj: *mut AnyObject = objc2::msg_send![null_cls, null];
        let dict_cls = objc2::class!(NSMutableDictionary);
        let dict: *mut AnyObject = objc2::msg_send![dict_cls, dictionaryWithCapacity: 5usize];
        for key in &[
            "bounds",
            "position",
            "contents",
            "anchorPoint",
            "contentsRect",
        ] {
            let k = nsstring_from_str(key);
            let _: () = objc2::msg_send![dict, setObject: null_obj, forKey: k];
            let _: () = objc2::msg_send![k, release];
        }
        let _: () = objc2::msg_send![layer, setActions: dict];

        let _: () = objc2::msg_send![view, setLayer: layer];

        // addSubview:positioned:relativeTo: — order applied by
        // macos_restack later; positionAbove=nil here.
        // NSWindowAbove == 1.
        let _: () = objc2::msg_send![
            content_view,
            addSubview: view,
            positioned: 1u64,
            relativeTo: ptr::null_mut::<AnyObject>(),
        ];

        (view, layer)
    }
}

// =====================================================================
// Vtable-exposed compositor functions
// =====================================================================

pub fn macos_set_expected_size(w: c_int, h: c_int) {
    G_GATE.lock().set_expected((w, h));
}

pub fn macos_alloc_surface() -> *mut c_void {
    // Allocate the Surface up front; the AppKit setup happens on the
    // main thread but writes into this stable heap address.
    let surf_ptr = Box::into_raw(Box::new(Surface::new()));
    // Register next to the `Box::into_raw` that mints the pointer and before
    // the bounce, so it is live for exactly as long as the handle is public.
    // `register`, not `add_live`: this platform derives its main surface from
    // `stack.first()`, and seeding `main` here would change is-main — and with
    // it transition gating — before the first restack.
    G_SURFACE_STACK.lock().register(SurfacePtr(surf_ptr));

    let Some(gpu) = gpu() else {
        // Allocation must still return a valid opaque handle so Browsers
        // can later free it; the surface will simply have no layer, and
        // every present is dropped.
        tracing::error!("[GPU] device init failed; surface has no painter");
        return surf_ptr as *mut c_void;
    };

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
        let scale: f64 = objc2::msg_send![win, backingScaleFactor];
        let (view, layer) = create_content_layer(content_view, frame, scale);
        let surf = &mut *(s_addr as *mut Surface);
        surf.view = view;
        surf.layer = layer;

        // The first configure lands here, on the thread that owns the layer.
        let Some(layer) = NonNull::new(layer.cast::<c_void>()) else {
            return;
        };
        let size = FrameSize {
            w: (frame.size.width * scale) as c_int,
            h: (frame.size.height * scale) as c_int,
        };
        match gpu.new_surface(WindowTarget::CoreAnimationLayer { layer }, size) {
            Ok(painter) => *surf.painter.lock() = Some(painter),
            Err(e) => tracing::error!("[GPU] surface creation failed: {e}"),
        }
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

pub fn macos_surface_present(s: *mut c_void, tex: &SharedTexture) -> bool {
    present_frame(s, tex.coded(), Frame::Shared(tex))
}

pub fn macos_surface_present_software(
    s: *mut c_void,
    pixels: &[u8],
    size: PhysicalSize,
    dirty: &[JfnRect],
) -> bool {
    if pixels.is_empty() || size.w <= 0 || size.h <= 0 {
        return false;
    }
    present_frame(
        s,
        FrameSize {
            w: size.w,
            h: size.h,
        },
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

/// Present inline, on whatever thread CEF called us on — which on this
/// platform is the main thread, because that is where CEF's UI thread is.
///
/// Configures nothing: under `ConfigureSite::Owner` the extent is whatever
/// `macos_surface_resize` last set from its main-thread closure. The painter
/// lock is held for the present alone and released before re-entering the
/// registry locks, which is the lock order every other entry point follows.
fn present_frame(s: *mut c_void, size: FrameSize, frame: Frame<'_>) -> bool {
    if s.is_null() {
        return false;
    }
    warn_once_if_off_main();
    let s_ptr = s as *mut Surface;

    // is-cef-main = bottom-of-stack check, plus the liveness resolve every
    // entry point does — the same lock acquisition doing one more thing.
    let is_main = {
        let stack = G_SURFACE_STACK.lock();
        if !stack.live().contains(&SurfacePtr(s_ptr)) {
            return false;
        }
        stack.is_main(SurfacePtr(s_ptr))
    };

    if is_main && G_GATE.lock().in_transition() {
        return false;
    }

    {
        // SAFETY: the live set says this pointer is still ours.
        let surf = unsafe { &*s_ptr };
        let mut painter = surf.painter.lock();
        match painter.as_mut() {
            Some(painter) => {
                if let Err(e) = painter.present(frame, || {}) {
                    tracing::error!("[GPU] present failed: {e}");
                }
            }
            None => tracing::warn!("[GPU] present skipped: no painter"),
        }
    }

    if is_main {
        // Clear the gate when the frame matches the expected post-transition
        // size. `coded` is the allocation size.
        G_GATE.lock().note_present_size((size.w, size.h));
    }
    true
}

fn warn_once_if_off_main() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !is_main_thread() && !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!("[GPU] present arrived off the main thread");
    }
}

pub fn macos_surface_resize(s: *mut c_void, lw: c_int, _lh: c_int, pw: c_int, ph: c_int) {
    if s.is_null() {
        return;
    }
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
        let scale: f64 = if pw > 0 && lw > 0 {
            pw as f64 / lw as f64
        } else if !win.is_null() {
            objc2::msg_send![win, backingScaleFactor]
        } else {
            1.0
        };
        let _: () = objc2::msg_send![surf.layer, setContentsScale: scale];
        if pw > 0 && ph > 0 {
            // The drawable resize, on the thread that owns the layer.
            if let Some(painter) = surf.painter.lock().as_mut() {
                painter.resize(FrameSize { w: pw, h: ph });
            }
        }
    });
}

pub fn macos_surface_set_visible(s: *mut c_void, visible: bool) {
    if s.is_null() {
        return;
    }
    let s_addr = s as usize;
    run_on_main_async(move || unsafe {
        let s_ptr = s_addr as *mut Surface;
        if !is_live(s_ptr) {
            return;
        }
        let surf = &*s_ptr;
        if !surf.view.is_null() {
            let _: () = objc2::msg_send![surf.view, setHidden: !visible];
        }
    });
}

pub fn macos_restack(ordered: *const *mut c_void, n: usize) {
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
    // restack must complete before Browsers proceeds — use sync.
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
