use parking_lot::RwLock;

use crate::runtime::WlRuntime;
use jfn_platform_abi::Scale;

use crate::scale::Scale120;
use crate::wl_ops;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowSize {
    w: i32,
    h: i32,
}

impl WindowSize {
    pub(crate) fn new(w: i32, h: i32) -> Option<Self> {
        (w > 0 && h > 0).then_some(Self { w, h })
    }

    pub(crate) fn w(self) -> i32 {
        self.w
    }

    pub(crate) fn h(self) -> i32 {
        self.h
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowMode {
    Floating,
    Tiled,
    Maximized,
    Fullscreen,
}

impl WindowMode {
    pub(crate) fn uses_floating_restore(self) -> bool {
        matches!(self, WindowMode::Floating)
    }
}

#[derive(Clone, Copy)]
struct WindowExtent {
    logical: WindowSize,
    physical: WindowSize,
    scale: Scale120,
    generation: u64,
    mode: WindowMode,
}

impl WindowExtent {
    fn build(
        logical: WindowSize,
        scale: Scale120,
        mode: WindowMode,
        generation: u64,
    ) -> Option<Self> {
        let reported = scale.scale();
        let physical = WindowSize::new(
            reported.to_physical(logical.w())?,
            reported.to_physical(logical.h())?,
        )?;
        Some(Self {
            logical,
            physical,
            scale,
            generation,
            mode,
        })
    }
}

struct Inner {
    scale: Option<Scale120>,
    unstated: crate::scale::UnstatedLog,
    extent: Option<WindowExtent>,
    generation: u64,
}

pub(crate) struct WindowState {
    inner: RwLock<Inner>,
}

#[derive(Clone, Copy)]
pub(crate) struct WindowExtentSnapshot {
    logical: WindowSize,
    physical: WindowSize,
    scale: Scale,
    mode: WindowMode,
}

impl WindowExtentSnapshot {
    fn from_extent(e: &WindowExtent) -> Self {
        Self {
            logical: e.logical,
            physical: e.physical,
            scale: e.scale.scale(),
            mode: e.mode,
        }
    }

    pub(crate) fn logical(&self) -> WindowSize {
        self.logical
    }

    pub(crate) fn physical(&self) -> WindowSize {
        self.physical
    }

    pub(crate) fn scale(&self) -> Scale {
        self.scale
    }

    pub(crate) fn mode(&self) -> WindowMode {
        self.mode
    }
}

impl WindowState {
    pub(crate) fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                scale: None,
                unstated: crate::scale::UnstatedLog::new(),
                extent: None,
                generation: 0,
            }),
        }
    }

    fn extent(&self) -> Option<WindowExtent> {
        self.inner.read().extent
    }

    pub(crate) fn window_extent(&self) -> Option<WindowExtentSnapshot> {
        self.extent().map(|e| WindowExtentSnapshot::from_extent(&e))
    }

    pub(crate) fn stated_scale(&self) -> Option<Scale120> {
        self.inner.read().scale
    }

    pub(crate) fn scale(&self) -> Scale {
        let stated = {
            let st = self.inner.read();
            st.extent.map(|e| e.scale).or(st.scale)
        };
        match stated {
            Some(scale) => scale.scale(),
            None => {
                let mut st = self.inner.write();
                crate::scale::unstated(&mut st.unstated).scale()
            }
        }
    }

    pub(crate) fn resolve_unstated_scale(&self) {
        let mut st = self.inner.write();
        if st.scale.is_some() {
            return;
        }
        st.scale = Some(crate::scale::unstated(&mut st.unstated));
    }

    pub(crate) fn publish(&self, rt: &'static WlRuntime, logical: WindowSize, mode: WindowMode) {
        let built = {
            let mut st = self.inner.write();
            let Some(scale) = st.scale else {
                return;
            };
            let built = WindowExtent::build(logical, scale, mode, st.generation + 1);
            if let Some(extent) = built {
                st.generation = extent.generation;
                st.extent = Some(extent);
            }
            built.ok_or(scale)
        };
        let extent = match built {
            Ok(extent) => extent,
            Err(scale) => {
                tracing::error!(
                    target: "Main",
                    "window extent {}x{} at scale {scale} is unrepresentable; the published extent stands",
                    logical.w(),
                    logical.h()
                );
                return;
            }
        };
        tracing::debug!(
            target: "Main",
            "window extent gen={} logical={}x{} physical={}x{} scale={}",
            extent.generation, extent.logical.w, extent.logical.h,
            extent.physical.w, extent.physical.h, extent.scale
        );

        let fullscreen = mode == WindowMode::Fullscreen;
        if rt.try_core().is_some() {
            wl_ops::on_configure(rt, fullscreen);
        }
        jfn_platform_abi::notify_window_changed();
    }

    pub(crate) fn report_scale(&self, scale: Scale120) {
        let first = {
            let mut st = self.inner.write();
            let first = st.scale.is_none();
            st.scale = Some(scale);
            first
        };
        if first {
            tracing::info!(target: "Main", "scale stated: {scale}");
        }
    }

    pub(crate) fn seed_scale(&self, scale: Scale120) {
        let seeded = {
            let mut st = self.inner.write();
            if st.scale.is_some() {
                false
            } else {
                st.scale = Some(scale);
                true
            }
        };
        if seeded {
            tracing::info!(target: "Main", "scale seeded from the output probe: {scale}");
        }
    }
}

pub(crate) fn feed_suspended(suspended: bool) {
    jfn_playback::lifecycle::jfn_lifecycle_set_visible(!suspended);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(raw: u32) -> Option<Scale120> {
        Scale120::from_wire(raw)
    }

    #[test]
    fn a_probe_seed_never_displaces_a_compositor_scale() {
        let st = WindowState::new();
        let (Some(stated), Some(probed)) = (wire(180), wire(120)) else {
            return;
        };
        st.report_scale(stated);
        st.seed_scale(probed);
        assert_eq!(st.stated_scale(), Some(stated));
    }

    #[test]
    fn a_compositor_scale_displaces_a_probe_seed() {
        let st = WindowState::new();
        let (Some(probed), Some(stated)) = (wire(120), wire(180)) else {
            return;
        };
        st.seed_scale(probed);
        assert_eq!(st.stated_scale(), Some(probed));
        st.report_scale(stated);
        assert_eq!(st.stated_scale(), Some(stated));
    }

    #[test]
    fn a_session_that_will_state_no_scale_reports_and_publishes_one() {
        let st = WindowState::new();
        assert_eq!(st.stated_scale(), None);
        st.resolve_unstated_scale();
        let resolved = st.stated_scale();
        assert_eq!(resolved.map(Scale120::scale), Some(st.scale()));
        assert!(resolved.is_some());
        let Some(logical) = WindowSize::new(1280, 720) else {
            return;
        };
        let Some(scale) = resolved else {
            return;
        };
        assert!(WindowExtent::build(logical, scale, WindowMode::Floating, 1).is_some());
    }

    #[test]
    fn a_resolved_absence_never_displaces_a_stated_scale() {
        let st = WindowState::new();
        let Some(stated) = wire(180) else {
            return;
        };
        st.report_scale(stated);
        st.resolve_unstated_scale();
        assert_eq!(st.stated_scale(), Some(stated));
    }
}
