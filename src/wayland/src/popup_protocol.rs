//! Popup protocol objects share the seat's event queue and dispatch owner.
use crate::input::State;
use crate::protocol::InputTarget;
use crate::window_state::Superseded;
use jfn_platform_abi::{Generation, LogicalSize, MenuPaint, MenuPlacement, WindowPoint};
use smithay_client_toolkit::compositor::Surface;
use smithay_client_toolkit::error::GlobalError;
use smithay_client_toolkit::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use smithay_client_toolkit::shell::xdg::{XdgPositioner, XdgShell};
use smithay_client_toolkit::shm::slot::SlotPool;
use std::sync::Arc;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::xdg_positioner::{
    Anchor, ConstraintAdjustment, Gravity,
};

pub(crate) struct PopupParent {
    pub(crate) surface: wayland_client::protocol::wl_surface::WlSurface,
    pub(crate) xdg_surface: wayland_protocols::xdg::shell::client::xdg_surface::XdgSurface,
}

pub(crate) struct Popups {
    xdg_shell: XdgShell,
    viewporter: WpViewporter,
    pool: Option<SlotPool>,
    current: PopupMapping,
    latest_generation: u64,
}
impl Popups {
    pub(crate) fn new(
        xdg_shell: XdgShell,
        viewporter: WpViewporter,
        pool: Option<SlotPool>,
    ) -> Self {
        Self {
            xdg_shell,
            viewporter,
            pool,
            current: PopupMapping::None,
            latest_generation: 0,
        }
    }
}

pub(crate) enum PopupCommand {
    Create {
        generation: Generation,
        place: MenuPlacement,
        input: Arc<dyn InputTarget>,
        parent: PopupParent,
    },
    Paint(MenuPaint),
    Destroy {
        generation: Generation,
    },
}

/// One live menu popup and everything that names its `wl_surface`.
struct LivePopup {
    generation: Generation,
    popup: Popup,
    viewport: WpViewport,
    input: Arc<dyn InputTarget>,
}

impl LivePopup {
    /// Crop, attach, damage and commit the menu buffer; the first one maps the
    /// popup.
    fn attach(&self, buffer: &crate::wl_state::AttachedBuffer, paint: &MenuPaint) {
        let surface = self.popup.wl_surface();
        self.viewport.set_source(
            0.0,
            f64::from(paint.scroll),
            f64::from(paint.buffer.w),
            f64::from(paint.view.physical().h),
        );
        self.viewport
            .set_destination(paint.view.logical().w, paint.view.logical().h);
        buffer.attach_to(surface);
        surface.damage_buffer(0, 0, paint.buffer.w, paint.buffer.h);
        surface.commit();
    }
}

// The viewport names the `wl_surface` that dropping the popup destroys, so it
// goes first.
impl Drop for LivePopup {
    fn drop(&mut self) {
        self.viewport.destroy();
    }
}

#[derive(Default)]
enum PopupMapping {
    #[default]
    None,
    Created {
        live: LivePopup,
    },
    Mapped {
        live: LivePopup,
        buffer: crate::wl_state::AttachedBuffer,
    },
}

impl PopupMapping {
    fn generation(&self) -> Option<Generation> {
        Some(self.live()?.generation)
    }

    fn live(&self) -> Option<&LivePopup> {
        match self {
            Self::None => None,
            Self::Created { live } | Self::Mapped { live, .. } => Some(live),
        }
    }

    fn paint(&mut self, buffer: crate::wl_state::AttachedBuffer, paint: &MenuPaint) {
        let (live, retired) = match std::mem::take(self) {
            Self::None => return,
            Self::Created { live } => (live, None),
            Self::Mapped { live, buffer } => (live, Some(buffer)),
        };
        live.attach(&buffer, paint);
        // Retired only once the replacement is committed, so the surface is
        // never left naming a destroyed buffer.
        drop(retired);
        *self = Self::Mapped { live, buffer };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
enum PopupPaintError {
    #[error("no shm pool")]
    NoPool,
    #[error("no buffer drawn from the menu's pixels")]
    Draw,
}

/// The positioner for a surface of `size` logical px whose top-left sits at
/// `anchor`. The anchor rect is one pixel because the anchor is a point.
fn positioner(
    xdg_shell: &XdgShell,
    anchor: WindowPoint,
    size: LogicalSize,
) -> Result<XdgPositioner, GlobalError> {
    let p = XdgPositioner::new(xdg_shell)?;
    let (x, y) = anchor.floor();
    p.set_size(size.w, size.h);
    p.set_anchor_rect(x, y, 1, 1);
    p.set_anchor(Anchor::TopLeft);
    p.set_gravity(Gravity::BottomRight);
    p.set_constraint_adjustment(
        ConstraintAdjustment::FlipX
            | ConstraintAdjustment::FlipY
            | ConstraintAdjustment::SlideX
            | ConstraintAdjustment::SlideY,
    );
    Ok(p)
}

impl State {
    pub(crate) fn create_popup(
        &mut self,
        generation: Generation,
        place: MenuPlacement,
        input: Arc<dyn InputTarget>,
        parent: PopupParent,
    ) {
        // Each generation drives exactly one create, so `<=` (not `<`) also
        // blocks resurrecting a just-destroyed popup: teardown leaves latest_generation
        // at its peak.
        if generation.get() <= self.popups.latest_generation || place.serial == 0 {
            input.dismissed();
            return;
        }
        self.popups.latest_generation = generation.get();
        if let Some(live) = self.popups.current.live() {
            self.protocol.retire(&live.popup.wl_surface().id());
        }
        self.popups.current = PopupMapping::None;
        let rt = self.rt;
        let created = rt.window().with_basis(place.basis, || {
            self.open_popup(generation, place, input.clone(), parent)
        });
        match created {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(target: "Main", "menu popup: {e}");
                input.dismissed();
            }
            Err(Superseded) => input.dismissed(),
        }
    }

    fn open_popup(
        &mut self,
        generation: Generation,
        place: MenuPlacement,
        input: Arc<dyn InputTarget>,
        parent: PopupParent,
    ) -> Result<(), GlobalError> {
        let positioner = positioner(&self.popups.xdg_shell, place.anchor, place.view.logical())?;
        let surface = Surface::new(&self.compositor, &self.qh)?;
        let viewport = self
            .popups
            .viewporter
            .get_viewport(surface.wl_surface(), &self.qh, ());
        // xdg_popup.grab is only honored before the popup's first commit, so
        // the grab and the commit below must stay in that order.
        let popup = match Popup::from_surface(
            Some(&parent.xdg_surface),
            &positioner,
            &self.qh,
            surface,
            &self.popups.xdg_shell,
        ) {
            Ok(p) => p,
            Err(e) => {
                viewport.destroy();
                return Err(e);
            }
        };
        popup.xdg_popup().grab(&self.seat, place.serial);
        popup.wl_surface().commit();
        self.protocol.register_popup(
            popup.wl_surface().clone(),
            parent.surface.id(),
            generation,
            input.clone(),
        );
        self.popups.current = PopupMapping::Created {
            live: LivePopup {
                generation,
                popup,
                viewport,
                input,
            },
        };
        Ok(())
    }

    pub(crate) fn paint_popup(&mut self, paint: MenuPaint) {
        let generation = paint.generation;
        if self.popups.current.generation() != Some(generation) {
            return;
        }
        let first = matches!(self.popups.current, PopupMapping::Created { .. });
        let rt = self.rt;
        let attached = rt
            .window()
            .with_basis(paint.basis, || self.attach_popup(&paint));
        match attached {
            Ok(Ok(())) => {}
            Ok(Err(e)) if !first => {
                tracing::debug!(target: "Main", "popup frame: {e}; the frame on screen stands");
            }
            Ok(Err(e)) => {
                tracing::error!(target: "Main", "popup first frame: {e}; dismissed");
                self.dismiss_popup(generation);
            }
            Err(Superseded) => self.dismiss_popup(generation),
        }
    }

    fn attach_popup(&mut self, paint: &MenuPaint) -> Result<(), PopupPaintError> {
        let pool = self.popups.pool.as_mut().ok_or(PopupPaintError::NoPool)?;
        let (w, h) = (paint.buffer.w, paint.buffer.h);
        let buffer = crate::wl_state::draw_from_pixels(pool, &paint.pixels, w, h)
            .ok_or(PopupPaintError::Draw)?;
        self.popups
            .current
            .paint(crate::wl_state::AttachedBuffer::Shm(buffer), paint);
        Ok(())
    }

    /// Tear the popup down, but only if `generation` still owns it — a newer
    /// menu may have taken the role in the gap between a stale teardown being
    /// decided and this call, and must not be torn down by it.
    pub(crate) fn destroy_popup(&mut self, generation: Generation) {
        if self.popups.current.generation() != Some(generation) {
            return;
        }
        if let Some(live) = self.popups.current.live() {
            self.protocol.retire(&live.popup.wl_surface().id());
        }
        self.popups.current = PopupMapping::None;
    }

    pub(crate) fn dismiss_popup(&mut self, generation: Generation) {
        let input = self
            .popups
            .current
            .live()
            .filter(|live| live.generation == generation)
            .map(|live| live.input.clone());
        self.destroy_popup(generation);
        if let Some(input) = input {
            input.dismissed();
        }
    }

    fn popup_generation(&self, popup: &Popup) -> Option<Generation> {
        let live = self.popups.current.live()?;
        (&live.popup == popup).then_some(live.generation)
    }
}

impl PopupHandler for State {
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        configure: PopupConfigure,
    ) {
        if let Some(live) = self
            .popups
            .current
            .live()
            .filter(|live| &live.popup == popup)
        {
            live.input.configured(LogicalSize {
                w: configure.width,
                h: configure.height,
            });
        }
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        let Some(generation) = self.popup_generation(popup) else {
            return;
        };
        // The role and its input destination retire together on this queue.
        // SCTK releases its callback handle when this method returns.
        self.dismiss_popup(generation);
    }
}

impl Dispatch<WpViewporter, ()> for State {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WpViewport, ()> for State {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: <WpViewport as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// SCTK's XdgShell binding includes toplevel-decoration dispatch bounds. This
// queue creates popup roles only; root-window roles retain their own handler.
impl smithay_client_toolkit::shell::xdg::window::WindowHandler for State {
    fn request_close(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &smithay_client_toolkit::shell::xdg::window::Window,
    ) {
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &smithay_client_toolkit::shell::xdg::window::Window,
        _: smithay_client_toolkit::shell::xdg::window::WindowConfigure,
        _: u32,
    ) {
    }
}
