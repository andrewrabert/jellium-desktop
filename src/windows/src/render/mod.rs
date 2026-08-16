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
//! `apply_stack`, `set_visibility`, `present` and the popup entry points all
//! run on the CEF UI thread and are already serialized by it, leaving `init` and
//! `cleanup` on the app main thread at the process edges as the sole
//! cross-thread contention, where waiting out an in-flight `configure` costs
//! nothing. The WndProc hook in `crate::platform` must therefore stay
//! republish-only; routing any window message into the renderer would put a
//! GPU wait in front of the window's message queue.

mod device;
mod layer;

use std::ffi::c_int;

use jfn_gpu_paint::{FrameSize, Pixels, WindowTarget};
use jfn_platform_abi::{
    Ack, Content, PaintFrame, Presented, SurfaceHandle, SurfaceSize, Visibility, VisibilityCommit,
};
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
            content: Layer::new(content),
            popup: Layer::new(popup),
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

    fn commit_and_wait(&self) {
        if let Some(devices) = self.devices.as_ref() {
            devices.commit_and_wait();
        }
    }
}

// SAFETY: `Registry` holds DirectComposition interface pointers, which COM
// marks apartment-bound. DComp objects are in fact free-threaded (documented
// "DirectComposition and multithreading": any thread may call them, the
// device serializing internally), so moving them across threads is sound; the
// mutual exclusion this module additionally relies on comes from `STATE`'s
// Mutex plus the threading contract in the module doc — every entry point
// takes the lock exactly once, and no window-message thread enters.
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
    let Registry {
        devices, surfaces, ..
    } = &mut *st;
    if let Some(devices) = devices.as_ref() {
        for entry in surfaces.iter_mut() {
            entry.unparent(devices.root());
        }
    }
    surfaces.clear();
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

/// Apply the whole z-order to the root's children, bottom-to-top. Live
/// surfaces `ordered` does not name keep their relative order above those it
/// does, so every live surface stays parented. Video needs no placing: the
/// composition target is mpv's own HWND, so every visual is above it.
pub(crate) fn apply_stack(ordered: &[SurfaceHandle]) {
    let mut st = STATE.lock();
    let Registry {
        devices, surfaces, ..
    } = &mut *st;
    let Some(root) = devices.as_ref().map(Devices::root) else {
        return;
    };
    let rank = |e: &Entry| ordered.iter().position(|h| h.id() == e.id.0);

    let (mut named, mut unnamed): (Vec<Entry>, Vec<Entry>) = std::mem::take(surfaces)
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
                Err(e) => {
                    tracing::error!(target: "platform", "apply_stack AddVisual failed: {e:?}");
                }
            }
        }
    }

    *surfaces = named;
    st.commit();
}

/// Set a surface's content visual to `visibility`, publish it, and wait for
/// the composition engine to process the commit. Nothing here moves the visual
/// or touches its content binding.
pub(crate) fn set_visibility(h: SurfaceHandle, visibility: Visibility) -> VisibilityCommit {
    let mut st = STATE.lock();
    if let Some(entry) = st.find_mut(h) {
        entry.content.set_visible(visibility.is_shown());
    }
    st.commit_and_wait();
    VisibilityCommit::issued(visibility, Ack::immediate())
}

/// The extent a frame's content asks the swapchain for, or `None` when the
/// content carries nothing presentable.
fn frame_extent(content: &Content<'_>) -> Option<FrameSize> {
    match content {
        Content::Accelerated(tex) => (!tex.handle().is_null()).then(|| tex.coded()),
        Content::Software { size, pixels, .. } => (!pixels.is_empty() && size.w > 0 && size.h > 0)
            .then_some(FrameSize {
                w: size.w,
                h: size.h,
            }),
    }
}

/// Present one frame to `part`. The frame comes back undischarged when the
/// surface is gone, the content carries no picture, or the layer has no
/// swapchain to take it.
pub(crate) fn present<'a>(
    h: SurfaceHandle,
    part: Part,
    frame: PaintFrame<'a>,
) -> Result<Presented, PaintFrame<'a>> {
    let mut st = STATE.lock();
    if st.devices.is_none() {
        return Err(frame);
    }
    let Some(size) = frame_extent(frame.content()) else {
        return Err(frame);
    };
    let (presented, needs_commit) = {
        let Some(entry) = st.find_mut(h) else {
            return Err(frame);
        };
        let layer = entry.layer_mut(part);
        if !layer.ready(size) {
            return Err(frame);
        }
        let presented = match frame.content() {
            Content::Accelerated(tex) => layer.present_shared(tex),
            Content::Software { pixels, dirty, .. } => layer.present_pixels(Pixels {
                size,
                stride: size.w as u32 * 4,
                bgra: pixels,
                dirty,
            }),
        };
        (presented, layer.take_needs_commit())
    };
    if needs_commit {
        st.commit();
    }
    if !presented {
        // The surface is fine and the frame never entered a commit stream, so
        // its producer owes the successor.
        return Err(frame);
    }
    Ok(frame.present(|_content| Presented::issued()))
}

/// Offsets the content visual by the reserved top inset and sizes it to match.
pub(crate) fn resize(h: SurfaceHandle, size: SurfaceSize) {
    let mut st = STATE.lock();
    let Some(entry) = st.find_mut(h) else {
        return;
    };
    entry.content.set_offset(0.0, size.physical_top as f32);
    st.commit();
}

/// The content `IDCompositionVisual` for `h`.
///
/// The first call marks the surface external: its [`Layer`] builds no painter
/// and drops every present.
pub(crate) fn window_target(h: SurfaceHandle) -> Option<WindowTarget> {
    let mut st = STATE.lock();
    st.find_mut(h)?.content.window_target()
}

/// Place the popup visual at `x`, `y` — logical pixels inside the owning
/// surface. The popup shows itself with its first frame.
pub(crate) fn popup_show(h: SurfaceHandle, x: c_int, y: c_int) {
    let scale = crate::platform::win_get_scale();
    let (Some(px), Some(py)) = (scale.to_physical(x), scale.to_physical(y)) else {
        tracing::error!(target: "platform", "popup offset {x},{y} is unrepresentable at scale {scale}");
        return;
    };
    let mut st = STATE.lock();
    let Some(entry) = st.find_mut(h) else {
        return;
    };
    entry.popup.set_offset(px as f32, py as f32);
    st.commit();
}

/// Detach the popup visual's content, so reopening it cannot flash the frame
/// it was closed with.
pub(crate) fn popup_hide(h: SurfaceHandle) {
    let mut st = STATE.lock();
    let Some(entry) = st.find_mut(h) else {
        return;
    };
    entry.popup.detach();
    st.commit();
}
