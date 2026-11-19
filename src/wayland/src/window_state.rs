use parking_lot::RwLock;

use crate::runtime::WlRuntime;
use jfn_platform_abi::{Scale, Unrepresentable, WindowBasis};
use std::ffi::c_int;

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
    /// Compositor-tiled (snapped). Like Maximized/Fullscreen the size is
    /// compositor-dictated, so it must not feed the floating restore size.
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

#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
pub(crate) enum BuildError {
    #[error(transparent)]
    Unrepresentable(#[from] Unrepresentable),
    #[error("physical {w}x{h} has an axis below one pixel")]
    BelowOnePixel { w: i32, h: i32 },
}

impl WindowExtent {
    fn build(
        logical: WindowSize,
        scale: Scale120,
        mode: WindowMode,
        generation: u64,
    ) -> Result<Self, BuildError> {
        let reported = scale.scale();
        let (w, h) = (
            reported.to_physical(logical.w())?,
            reported.to_physical(logical.h())?,
        );
        let physical = WindowSize::new(w, h).ok_or(BuildError::BelowOnePixel { w, h })?;
        Ok(Self {
            logical,
            physical,
            scale,
            generation,
            mode,
        })
    }
}

impl WindowExtent {
    fn same_size(&self, other: &WindowExtent) -> bool {
        self.logical == other.logical
            && self.physical == other.physical
            && self.scale == other.scale
    }
}

struct Inner {
    scale: Option<Scale120>,
    unstated: crate::scale::UnstatedLog,
    extent: Option<WindowExtent>,
    generation: u64,
    titlebar_present: Option<bool>,
    web_top: Option<c_int>,
    basis: WindowBasis,
}

impl Inner {
    fn set_web_top(&mut self, web_top: Option<c_int>) -> bool {
        let changed = self.web_top != web_top;
        if changed {
            self.web_top = web_top;
            self.basis = self.basis.next();
        }
        changed
    }
}

pub(crate) struct WindowState {
    inner: RwLock<Inner>,
}

/// A coherent view of the window geometry from one lock acquisition.
#[derive(Clone, Copy)]
pub(crate) struct PublishedWindow {
    logical: WindowSize,
    physical: WindowSize,
    scale: Scale,
    mode: WindowMode,
    web_top: Option<c_int>,
}

impl PublishedWindow {
    fn from_inner(st: &Inner) -> Option<Self> {
        let e = st.extent.as_ref()?;
        Some(Self {
            logical: e.logical,
            physical: e.physical,
            scale: e.scale.scale(),
            mode: e.mode,
            web_top: st.web_top,
        })
    }

    pub(crate) fn web_top(&self) -> Option<c_int> {
        self.web_top
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
                titlebar_present: None,
                web_top: None,
                basis: WindowBasis::default(),
            }),
        }
    }

    pub(crate) fn window_extent(&self) -> Option<PublishedWindow> {
        PublishedWindow::from_inner(&self.inner.read())
    }

    pub(crate) fn basis(&self) -> WindowBasis {
        self.inner.read().basis
    }

    pub(crate) fn state(&self) -> (Option<PublishedWindow>, WindowBasis) {
        let st = self.inner.read();
        (PublishedWindow::from_inner(&st), st.basis)
    }

    pub(crate) fn with_basis<R>(&self, basis: WindowBasis, f: impl FnOnce() -> R) -> Option<R> {
        let st = self.inner.read();
        (st.basis == basis).then(f)
    }

    pub(crate) fn representable(&self, logical: WindowSize) -> Result<(), BuildError> {
        match self.inner.read().scale {
            Some(scale) => WindowExtent::build(logical, scale, WindowMode::Floating, 0).map(|_| ()),
            None => Ok(()),
        }
    }

    pub(crate) fn stated_scale(&self) -> Option<Scale120> {
        self.inner.read().scale
    }

    /// The scale this backend reports: the one the compositor has stated, else
    /// [`crate::scale::unstated`], whose log flag rides in the same lock as
    /// the absent scale.
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

    /// Records [`crate::scale::unstated`] as the scale this backend states, for
    /// the one session where no source will ever state one: no
    /// `wp_fractional_scale_manager_v1` to send `preferred_scale`, and an
    /// output probe that answered nothing.
    ///
    /// [`WindowState::stated_scale`] and [`WindowState::publish`] read the
    /// scale, not the absence, so this is where the absence stops for them.
    /// Records nothing when a scale is already held.
    pub(crate) fn resolve_unstated_scale(&self) {
        let mut st = self.inner.write();
        if st.scale.is_some() {
            return;
        }
        st.scale = Some(crate::scale::unstated(&mut st.unstated));
    }

    pub(crate) fn publish(&self, rt: &'static WlRuntime, logical: WindowSize, mode: WindowMode) {
        let decorations = rt.root().effective_decorations();
        let fullscreen = mode == WindowMode::Fullscreen;
        let built = {
            let mut st = self.inner.write();
            let Some(scale) = st.scale else {
                return;
            };
            match WindowExtent::build(logical, scale, mode, st.generation + 1) {
                Ok(extent) => {
                    let resized = st.extent.is_none_or(|old| !old.same_size(&extent));
                    if resized {
                        st.basis = st.basis.next();
                    }
                    st.generation = extent.generation;
                    st.extent = Some(extent);
                    let web_top = st
                        .titlebar_present
                        .map(|present| jfn_platform_abi::web_top(decorations, fullscreen, present));
                    let moved = st.set_web_top(web_top);
                    Ok((extent, st.web_top, resized || moved))
                }
                Err(e) => Err(e),
            }
        };
        let (extent, web_top, changed) = match built {
            Ok(built) => built,
            Err(e) => {
                tracing::error!(
                    target: "Main",
                    "window extent {}x{}: {e}; the published extent stands",
                    logical.w(),
                    logical.h()
                );
                return;
            }
        };
        tracing::debug!(
            target: "Main",
            "window extent gen={} logical={}x{} physical={}x{} scale={} web_top={web_top:?}",
            extent.generation, extent.logical.w, extent.logical.h,
            extent.physical.w, extent.physical.h, extent.scale
        );
        Self::published(rt, fullscreen, changed);
    }

    pub(crate) fn set_titlebar_present(&self, rt: &'static WlRuntime, present: bool) {
        let decorations = rt.root().effective_decorations();
        let (fullscreen, changed) = {
            let mut st = self.inner.write();
            if st.titlebar_present == Some(present) {
                return;
            }
            st.titlebar_present = Some(present);
            let Some(extent) = st.extent else {
                return;
            };
            let fullscreen = extent.mode == WindowMode::Fullscreen;
            let web_top = Some(jfn_platform_abi::web_top(decorations, fullscreen, present));
            (fullscreen, st.set_web_top(web_top))
        };
        Self::published(rt, fullscreen, changed);
    }

    fn published(rt: &'static WlRuntime, fullscreen: bool, changed: bool) {
        if rt.try_core().is_some() {
            wl_ops::on_configure(rt, fullscreen);
        }
        jfn_platform_abi::notify_window_changed();
        if changed && let Some(menu) = rt.menu() {
            menu.window_changed();
        }
    }

    /// The compositor's `wp_fractional_scale_v1.preferred_scale`: the
    /// authoritative scale, recorded whatever is already held.
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

    /// The output probe's scale, recorded only while none is held: a probe
    /// result that lands after the compositor has spoken is stale.
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
        assert!(WindowExtent::build(logical, scale, WindowMode::Floating, 1).is_ok());
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
