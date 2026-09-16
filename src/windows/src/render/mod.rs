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

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Part {
    Content,
    Popup,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct SurfaceId(u64);

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

    fn unparent(&mut self, root: &IDCompositionVisual) {
        unsafe {
            let _ = root.RemoveVisual(self.content.visual());
        }
    }
}

struct Registry {
    devices: Option<Devices>,
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

unsafe impl Send for Registry {}

static STATE: Mutex<Registry> = Mutex::new(Registry::new());

pub(crate) fn init(hwnd: HWND) -> Result<(), jfn_platform_abi::PlatformInitError> {
    use jfn_platform_abi::PlatformInitError as Error;
    if !jfn_gpu_paint::any_adapter() {
        return Err(Error::backend(
            "DirectComposition adapter selection",
            "no usable GPU adapter",
        ));
    }
    let mut st = STATE.lock();
    if st.devices.is_some() {
        return Err(Error::backend(
            "DirectComposition initialization",
            "already initialized",
        ));
    }
    st.devices = Some(
        Devices::create(hwnd)
            .map_err(|e| Error::backend("DirectComposition device creation", e))?,
    );
    Ok(())
}

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

pub(crate) fn set_visibility(h: SurfaceHandle, visibility: Visibility) -> VisibilityCommit {
    let mut st = STATE.lock();
    if let Some(entry) = st.find_mut(h) {
        entry.content.set_visible(visibility.is_shown());
    }
    st.commit_and_wait();
    VisibilityCommit::issued(visibility, Ack::immediate())
}

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
        return Err(frame);
    }
    Ok(frame.present(|_content| Presented::issued()))
}

pub(crate) fn resize(h: SurfaceHandle, size: SurfaceSize) {
    let mut st = STATE.lock();
    let Some(entry) = st.find_mut(h) else {
        return;
    };
    entry.content.set_offset(0.0, size.physical_top as f32);
    st.commit();
}

pub(crate) fn window_target(h: SurfaceHandle) -> Option<WindowTarget> {
    let mut st = STATE.lock();
    st.find_mut(h)?.content.window_target()
}

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

pub(crate) fn popup_hide(h: SurfaceHandle) {
    let mut st = STATE.lock();
    let Some(entry) = st.find_mut(h) else {
        return;
    };
    entry.popup.detach();
    st.commit();
}
