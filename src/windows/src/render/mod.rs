//! DirectComposition presenter.
//!
//! Every frame CEF produces is presented 1:1 under a composition visual. The
//! composition target is the mpv HWND, so DWM clips a frame wider or taller
//! than the window and a frame smaller than it leaves the window's own
//! background beyond its edges. No frame is judged against a window size, and
//! the renderer holds no size state at all: a mis-sized frame is a
//! one-frame-old picture, corrected by the next paint after the window-changed
//! wakeup resizes CEF.
//!
//! Should DWM's window clip ever prove insufficient, the fallback is
//! `IDCompositionVisual::SetClip2` on the root visual, set to the client rect
//! from `crate::window::client_extent`.
//!
//! Threading. `STATE` is the process's DirectComposition serialization point:
//! every visual-tree call, every swapchain build, and every present happens
//! under it, each entry point taking it exactly once. That is only sound
//! while no window-message thread enters this module — `alloc`, `free`,
//! `restack`, `set_visible`, `present` and the popup entry points all run on
//! the CEF UI thread and are already serialized by it, leaving `init` and
//! `cleanup` on the app main thread at the process edges as the sole
//! cross-thread contention, where waiting out an in-flight `configure` costs
//! nothing. The WndProc hook in `crate::platform` must therefore stay
//! republish-only; routing any window message into the renderer would put a
//! GPU wait in front of the window's message queue.

mod device;
mod layer;

use std::ffi::c_int;

use jfn_gpu_paint::{Frame, FrameSize, Pixels};
use jfn_platform_abi::{PaintFrame, Scale, SurfaceHandle};
use parking_lot::Mutex;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::DirectComposition::IDCompositionVisual;

use crate::render::device::Devices;
use crate::render::layer::Layer;

/// Which of a surface's two visuals a call addresses.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Part {
    /// The browser view, filling the window.
    Content,
    /// The OSR dropdown, nested under the content visual.
    Popup,
}

/// A live surface's identity. Handed to CEF packed into a [`SurfaceHandle`]
/// and never dereferenced; ids start at 1 and are never reused, so a stale
/// handle resolves to nothing.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct SurfaceId(u64);

/// One CEF surface. The only constructor builds both visuals, nests the popup
/// under the content visual, and parents the content visual to the root, so
/// an `Entry` that exists is a surface that is in the tree with its popup
/// nested — neither is a rule the rest of the module has to keep.
struct Entry {
    id: SurfaceId,
    content: Layer,
    popup: Layer,
}

impl Entry {
    fn create(devices: &Devices, id: SurfaceId) -> windows_core::Result<Entry> {
        let content = devices.new_visual()?;
        let popup = devices.new_visual()?;
        unsafe {
            content.AddVisual(&popup, true, None::<&IDCompositionVisual>)?;
            devices
                .root()
                .AddVisual(&content, true, None::<&IDCompositionVisual>)?;
        }
        Ok(Entry {
            id,
            content: Layer::new(content, true),
            popup: Layer::new(popup, false),
        })
    }

    fn layer_mut(&mut self, part: Part) -> &mut Layer {
        match part {
            Part::Content => &mut self.content,
            Part::Popup => &mut self.popup,
        }
    }

    /// Unparents the content visual from `root`; the popup goes with it.
    fn unparent(&mut self, root: &IDCompositionVisual) {
        unsafe {
            let _ = root.RemoveVisual(self.content.visual());
        }
    }
}

struct Registry {
    devices: Option<Devices>,
    /// Live surfaces, bottom-to-top: exactly the root visual's children, in
    /// the root's child order.
    surfaces: Vec<Entry>,
    next_id: u64,
}

impl Registry {
    const fn new() -> Registry {
        Registry {
            devices: None,
            surfaces: Vec::new(),
            next_id: 1,
        }
    }

    fn find_mut(&mut self, h: SurfaceHandle) -> Option<&mut Entry> {
        let id = h.id();
        self.surfaces.iter_mut().find(|e| e.id.0 == id)
    }

    fn commit(&self) {
        if let Some(devices) = self.devices.as_ref() {
            devices.commit();
        }
    }
}

// The one unsafe impl in the renderer: COM interfaces are agile for the
// apartment they were created in, and every call on them is made under STATE.
unsafe impl Send for Registry {}

static STATE: Mutex<Registry> = Mutex::new(Registry::new());

/// Build the DComp device, its HWND target, and the root visual.
/// False when no GPU adapter is usable or device creation failed; any
/// partial state is dropped before returning.
pub(crate) fn init(hwnd: HWND) -> bool {
    if !jfn_gpu_paint::any_adapter() {
        tracing::error!(target: "platform", "renderer init failed: no usable GPU adapter");
        return false;
    }
    let mut st = STATE.lock();
    if st.devices.is_some() {
        return true;
    }
    match Devices::create(hwnd) {
        Ok(devices) => {
            st.devices = Some(devices);
            true
        }
        Err(e) => {
            tracing::error!(target: "platform", "renderer init failed: {e:?}");
            false
        }
    }
}

/// Drop every remaining surface, then the devices. Runs after the WndProc
/// hook is unhooked and the input thread is joined.
pub(crate) fn cleanup() {
    let mut st = STATE.lock();
    let root = st.devices.as_ref().map(|d| d.root().clone());
    for mut entry in std::mem::take(&mut st.surfaces) {
        if let Some(root) = root.as_ref() {
            entry.unparent(root);
        }
    }
    st.commit();
    st.devices = None;
}

/// Allocate a surface: a content visual parented on top of the root and a
/// popup visual nested under it.
pub(crate) fn alloc() -> SurfaceHandle {
    let mut st = STATE.lock();
    let id = SurfaceId(st.next_id);
    let Some(devices) = st.devices.as_ref() else {
        return SurfaceHandle::NONE;
    };
    let entry = match Entry::create(devices, id) {
        Ok(entry) => entry,
        Err(e) => {
            tracing::error!(target: "platform", "surface creation failed: {e:?}");
            return SurfaceHandle::NONE;
        }
    };
    devices.commit();
    st.next_id += 1;
    st.surfaces.push(entry);
    SurfaceHandle::from_id(id.0)
}

/// Unparent and drop a surface, its visuals and its painters.
pub(crate) fn free(h: SurfaceHandle) {
    let mut st = STATE.lock();
    let id = h.id();
    let Some(pos) = st.surfaces.iter().position(|e| e.id.0 == id) else {
        return;
    };
    let mut entry = st.surfaces.remove(pos);
    if let Some(devices) = st.devices.as_ref() {
        entry.unparent(devices.root());
        devices.commit();
    }
}

/// Reorder the root's children bottom-to-top. Live surfaces `ordered` does
/// not name keep their relative order above those it does, so every live
/// surface stays parented.
pub(crate) fn restack(ordered: &[SurfaceHandle]) {
    let mut st = STATE.lock();
    let Some(root) = st.devices.as_ref().map(|d| d.root().clone()) else {
        return;
    };
    let rank = |e: &Entry| ordered.iter().position(|h| h.id() == e.id.0);

    let (mut named, mut unnamed): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut st.surfaces)
        .into_iter()
        .partition(|e| rank(e).is_some());
    named.sort_by_key(|e| rank(e).unwrap_or(usize::MAX));
    named.append(&mut unnamed);

    unsafe {
        for entry in &named {
            let _ = root.RemoveVisual(entry.content.visual());
        }
        let mut prev: Option<&IDCompositionVisual> = None;
        for entry in &named {
            let visual = entry.content.visual();
            let placed = match prev {
                Some(prev) => root.AddVisual(visual, true, prev),
                None => root.AddVisual(visual, false, None::<&IDCompositionVisual>),
            };
            match placed {
                Ok(()) => prev = Some(visual),
                Err(e) => tracing::error!(target: "platform", "restack AddVisual failed: {e:?}"),
            }
        }
    }

    st.surfaces = named;
    st.commit();
}

/// Show or hide a surface's content visual. Hiding detaches its content, so
/// showing it again cannot flash the frame it was hidden with.
pub(crate) fn set_visible(h: SurfaceHandle, visible: bool) {
    let mut st = STATE.lock();
    let changed = st
        .find_mut(h)
        .is_some_and(|entry| entry.content.set_visible(visible));
    if changed {
        st.commit();
    }
}

/// Present one frame to `part`. False when the surface is gone, the part is
/// hidden, or the swapchain refused the frame.
pub(crate) fn present(h: SurfaceHandle, part: Part, frame: PaintFrame<'_>) -> bool {
    let mut st = STATE.lock();
    if st.devices.is_none() {
        return false;
    }
    let Some(entry) = st.find_mut(h) else {
        return false;
    };
    let layer = entry.layer_mut(part);
    let presented = match frame {
        PaintFrame::Accelerated(tex) => {
            if tex.handle().is_null() {
                return false;
            }
            layer.present(Frame::Shared(&tex), tex.coded())
        }
        // Only reachable with --disable-gpu-compositing: the painter draws
        // both frame kinds, so there is nothing to gain by refusing one.
        PaintFrame::Software {
            size,
            pixels,
            dirty,
        } => {
            if pixels.is_empty() || size.w <= 0 || size.h <= 0 {
                return false;
            }
            let size = FrameSize {
                w: size.w,
                h: size.h,
            };
            layer.present(
                // CEF's OnPaint buffer is tightly packed.
                Frame::Copied(Pixels {
                    size,
                    stride: size.w as u32 * 4,
                    bgra: pixels,
                    dirty,
                }),
                size,
            )
        }
    };
    // Publishes wgpu's own SetContent when the present configured.
    st.commit();
    presented
}

/// Place the popup visual at `x`, `y` — logical pixels inside the owning
/// surface — and show it.
pub(crate) fn popup_show(h: SurfaceHandle, x: c_int, y: c_int) {
    let scale = crate::window::client_scale()
        .unwrap_or(Scale(1.0))
        .or_one()
        .0;
    let mut st = STATE.lock();
    let Some(entry) = st.find_mut(h) else {
        return;
    };
    entry.popup.set_offset(x as f32 * scale, y as f32 * scale);
    entry.popup.set_visible(true);
    st.commit();
}

/// Hide the popup visual and detach its content.
pub(crate) fn popup_hide(h: SurfaceHandle) {
    let mut st = STATE.lock();
    let changed = st
        .find_mut(h)
        .is_some_and(|entry| entry.popup.set_visible(false));
    if changed {
        st.commit();
    }
}
