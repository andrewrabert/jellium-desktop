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

struct Surface {
    view: *mut AnyObject,
    layer: *mut AnyObject,
    painter: Mutex<Option<Painter<'static>>>,
    external: AtomicBool,
}

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

#[derive(Clone, Copy, PartialEq)]
struct SurfacePtr(*mut Surface);
unsafe impl Send for SurfacePtr {}

static G_SURFACE_STACK: Mutex<SurfaceStack<SurfacePtr>> = Mutex::new(SurfaceStack::new());

fn is_live(p: *mut Surface) -> bool {
    G_SURFACE_STACK.lock().live().contains(&SurfacePtr(p))
}

static G_GATE: Mutex<TransitionGate> = Mutex::new(TransitionGate::new());

pub(crate) struct MacosResizeGate;

pub(crate) static MACOS_RESIZE_GATE: MacosResizeGate = MacosResizeGate;

impl jfn_platform_abi::ResizeGate for MacosResizeGate {
    fn begin(&self) {
        G_GATE.lock().begin();
    }

    fn end(&self) {}

    fn in_transition(&self) -> bool {
        G_GATE.lock().in_transition()
    }

    fn set_expected(&self, size: jfn_platform_abi::PhysicalSize) {
        G_GATE.lock().set_expected((size.w, size.h));
    }
}

fn gpu() -> Option<&'static Surfaces> {
    Surfaces::init(None)
}

unsafe fn create_content_layer(
    content_view: *mut AnyObject,
    frame: NSRect,
    scale: jfn_platform_abi::Scale,
    visibility: Visibility,
) -> (*mut AnyObject, *mut AnyObject) {
    unsafe {
        let mtm = MainThreadMarker::new_unchecked();

        let view = NSView::initWithFrame(NSView::alloc(mtm), frame);
        view.setHidden(!visibility.is_shown());
        view.setWantsLayer(true);
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let layer = CAMetalLayer::layer();
        layer.setFrame(frame);
        layer.setContentsScale(scale.as_f64());

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
        let actions: Retained<NSDictionary<NSString, ProtocolObject<dyn CAAction>>> =
            Retained::cast_unchecked(actions);
        layer.setActions(Some(&actions));

        view.setLayer(Some(&layer));

        let content_view: &NSView = &*content_view.cast::<NSView>();
        content_view.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Above, None);

        let layer_ptr = Retained::as_ptr(&layer).cast_mut().cast::<AnyObject>();
        (Retained::into_raw(view).cast::<AnyObject>(), layer_ptr)
    }
}

pub fn macos_alloc_surface(initial: Visibility) -> *mut c_void {
    let surf_ptr = Box::into_raw(Box::new(Surface::new()));
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

    G_SURFACE_STACK.lock().deregister(SurfacePtr(s_ptr));

    let s_addr = s_ptr as usize;
    run_on_main_sync(move || unsafe {
        let surf = &mut *(s_addr as *mut Surface);
        drop(surf.painter.lock().take());
        if !surf.view.is_null() {
            let _: () = objc2::msg_send![surf.view, removeFromSuperview];
            let _: () = objc2::msg_send![surf.view, release];
            surf.view = ptr::null_mut();
        }
        surf.layer = ptr::null_mut();
    });

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

pub fn macos_surface_window_target(s: *mut c_void) -> Option<WindowTarget> {
    if s.is_null() {
        return None;
    }
    let s_ptr = s as *mut Surface;
    if !is_live(s_ptr) {
        return None;
    }
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
            if let Some(painter) = surf.painter.lock().as_mut() {
                painter.resize(FrameSize { w: pw, h: ph });
            }
        }
    });
}

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
    let order: Vec<usize> = if ordered.is_null() || n == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(ordered, n) }
            .iter()
            .map(|p| *p as usize)
            .collect()
    };

    let apply = move || unsafe {
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
            let _: () = objc2::msg_send![
                content_view,
                addSubview: view,
                positioned: 1u64,
                relativeTo: prev,
            ];
            prev = view;
        }
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
    run_on_main_sync(apply);
}

pub fn jfn_macos_compositor_cleanup() {
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
            drop(surf.painter.lock().take());
            if !surf.view.is_null() {
                let _: () = objc2::msg_send![surf.view, removeFromSuperview];
                let _: () = objc2::msg_send![surf.view, release];
                surf.view = ptr::null_mut();
            }
            surf.layer = ptr::null_mut();
        }
    }

    G_GATE.lock().end();
}
