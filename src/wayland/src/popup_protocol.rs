//! Popup protocol objects share the seat's event queue and dispatch owner.
use crate::input::State;
use crate::protocol::InputTarget;
use jfn_platform_abi::{Generation, LogicalPoint, LogicalSize, MenuPaint, MenuPlacement};
use smithay_client_toolkit::compositor::Surface;
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

/// Menu-popup requests. Create, paint, reposition and destroy must reach the
/// compositor in the order they were issued, so they share one queue.
pub(crate) enum PopupCommand {
    Create {
        generation: Generation,
        anchor: LogicalPoint,
        /// The press or key serial the grab cites. Captured on the input
        /// thread at request time; by the time this is applied the seat's last
        /// serial has moved on.
        serial: u32,
        input: Arc<dyn InputTarget>,
        parent: PopupParent,
    },
    Map {
        generation: Generation,
    },
    Reposition {
        generation: Generation,
        place: MenuPlacement,
    },
    Paint(MenuPaint),
    Destroy {
        generation: Generation,
    },
}

/// Placement bookkeeping for one menu popup, free of protocol objects: it owns
/// what the compositor has been given and what it may still be sent, so "never
/// reposition an unmapped popup" is decided here and nowhere else.
pub(crate) mod popup_place {
    use super::MenuPlacement;

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub(crate) struct Placed {
        sent: Option<MenuPlacement>,
        held: Option<MenuPlacement>,
    }

    impl Placed {
        /// A popup armed with no menu on it: the compositor holds no placement.
        pub(crate) fn armed() -> Placed {
            Placed {
                sent: None,
                held: None,
            }
        }

        #[cfg(test)]
        pub(crate) fn created(sent: MenuPlacement) -> Placed {
            Placed {
                sent: Some(sent),
                held: None,
            }
        }

        /// What an unmapped popup wants next. A want equal to `sent` clears the
        /// hold, so a placement already on the wire is never re-sent.
        pub(crate) fn hold(&mut self, want: MenuPlacement) {
            self.held = (Some(want) != self.sent).then_some(want);
        }

        /// The placement the mapping commit must be followed with, consumed as
        /// it is read. `None` when the create-time placement still stands.
        pub(crate) fn on_map(&mut self) -> Option<MenuPlacement> {
            let place = self.held.take()?;
            self.sent = Some(place);
            Some(place)
        }

        /// The placement to put on the wire now; `None` when the compositor
        /// already holds it.
        pub(crate) fn send(&mut self, want: MenuPlacement) -> Option<MenuPlacement> {
            (Some(want) != self.sent).then(|| {
                self.sent = Some(want);
                want
            })
        }
    }
}
use popup_place::Placed;

/// One live menu popup and everything that names its `wl_surface`.
struct LivePopup {
    generation: Generation,
    popup: Popup,
    viewport: WpViewport,
    place: Placed,
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

    /// Attach [`ARMED_PIXEL`] and commit: the surface maps holding a grab and
    /// showing no menu. The viewport crops the one pixel to [`ARMED_SIZE`],
    /// the size the popup was configured at.
    fn attach_armed(&self, buffer: &crate::wl_state::AttachedBuffer) {
        let surface = self.popup.wl_surface();
        self.viewport.set_source(0.0, 0.0, 1.0, 1.0);
        self.viewport.set_destination(ARMED_SIZE.w, ARMED_SIZE.h);
        buffer.attach_to(surface);
        surface.damage_buffer(0, 0, 1, 1);
        surface.commit();
    }

    /// `xdg_popup.reposition`; every caller stands in a mapped state.
    fn reposition(&self, xdg_shell: &XdgShell, place: MenuPlacement) {
        let Some(positioner) = positioner(xdg_shell, place.anchor, place.view.logical()) else {
            return;
        };
        self.popup.reposition(&positioner, 0);
    }
}

// The viewport names the `wl_surface` that dropping the popup destroys, so it
// goes first.
impl Drop for LivePopup {
    fn drop(&mut self) {
        self.viewport.destroy();
    }
}

/// The menu popup's mapping state: `Unmapped` has no path to
/// [`LivePopup::reposition`], so a placement requested there is held until the
/// commit that maps the popup applies it.
#[derive(Default)]
enum PopupMapping {
    #[default]
    None,
    Unmapped {
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
            Self::Unmapped { live } | Self::Mapped { live, .. } => Some(live),
        }
    }

    fn reposition(&mut self, xdg_shell: &XdgShell, want: MenuPlacement) {
        match self {
            Self::None => {}
            Self::Unmapped { live } => live.place.hold(want),
            Self::Mapped { live, .. } => {
                if let Some(place) = live.place.send(want) {
                    live.reposition(xdg_shell, place);
                }
            }
        }
    }

    /// Commits the armed buffer, mapping the popup so its grab takes effect,
    /// then sends whatever placement was held since the arm. A popup already
    /// mapped keeps the buffer it has.
    fn map_armed(&mut self, xdg_shell: &XdgShell, buffer: crate::wl_state::AttachedBuffer) {
        let mut live = match std::mem::take(self) {
            Self::None => return,
            Self::Unmapped { live } => live,
            Self::Mapped { live, buffer } => {
                *self = Self::Mapped { live, buffer };
                return;
            }
        };
        live.attach_armed(&buffer);
        if let Some(place) = live.place.on_map() {
            live.reposition(xdg_shell, place);
        }
        *self = Self::Mapped { live, buffer };
    }

    /// Commits `buffer`, mapping an unmapped popup and then sending whatever
    /// placement was held since the arm.
    fn paint(
        &mut self,
        xdg_shell: &XdgShell,
        buffer: crate::wl_state::AttachedBuffer,
        paint: &MenuPaint,
    ) {
        let (mut live, retired) = match std::mem::take(self) {
            Self::None => return,
            Self::Unmapped { live } => (live, None),
            Self::Mapped { live, buffer } => (live, Some(buffer)),
        };
        live.attach(&buffer, paint);
        // Retired only once the replacement is committed, so the surface is
        // never left naming a destroyed buffer.
        drop(retired);
        if let Some(place) = live.place.on_map() {
            live.reposition(xdg_shell, place);
        }
        *self = Self::Mapped { live, buffer };
    }
}

/// The smallest size `xdg_positioner.set_size` admits: it answers a width or
/// height that is not positive with `invalid_input`.
const ARMED_SIZE: LogicalSize = LogicalSize { w: 1, h: 1 };

/// One transparent premultiplied BGRA pixel: the buffer whose commit maps the
/// armed popup.
const ARMED_PIXEL: [u8; 4] = [0, 0, 0, 0];

/// The positioner for a surface of `size` logical px whose top-left sits at
/// `anchor`. The anchor rect is one pixel because the anchor is a point.
fn positioner(
    xdg_shell: &XdgShell,
    anchor: LogicalPoint,
    size: LogicalSize,
) -> Option<XdgPositioner> {
    let p = XdgPositioner::new(xdg_shell)
        .inspect_err(|e| tracing::error!(target: "Main", "menu positioner: {e}"))
        .ok()?;
    p.set_size(size.w, size.h);
    p.set_anchor_rect(anchor.x, anchor.y, 1, 1);
    p.set_anchor(Anchor::TopLeft);
    p.set_gravity(Gravity::BottomRight);
    p.set_constraint_adjustment(
        ConstraintAdjustment::FlipX
            | ConstraintAdjustment::FlipY
            | ConstraintAdjustment::SlideX
            | ConstraintAdjustment::SlideY,
    );
    Some(p)
}

impl State {
    /// Creates the popup that holds the grab, with no menu on it, sized
    /// [`ARMED_SIZE`]. The grab cites the input thread's last press serial
    /// (button or key) — valid here only because every app connection shares
    /// one wl_client.
    pub(crate) fn create_popup(
        &mut self,
        generation: Generation,
        anchor: LogicalPoint,
        serial: u32,
        input: Arc<dyn InputTarget>,
        parent: PopupParent,
    ) {
        // Each generation drives exactly one create, so `<=` (not `<`) also
        // blocks resurrecting a just-destroyed popup: teardown leaves latest_generation
        // at its peak.
        if generation.get() <= self.popups.latest_generation || serial == 0 {
            input.dismissed();
            return;
        }
        self.popups.latest_generation = generation.get();
        if let Some(live) = self.popups.current.live() {
            self.protocol.retire(&live.popup.wl_surface().id());
        }
        self.popups.current = PopupMapping::None;
        let Some(positioner) = positioner(&self.popups.xdg_shell, anchor, ARMED_SIZE) else {
            input.dismissed();
            return;
        };
        self.open_popup(
            generation,
            &positioner,
            serial,
            Placed::armed(),
            input,
            parent,
        );
    }

    /// Creates the popup against `positioner` and takes the grab; `place` is
    /// what the compositor is thereafter considered to hold.
    fn open_popup(
        &mut self,
        generation: Generation,
        positioner: &XdgPositioner,
        serial: u32,
        place: Placed,
        input: Arc<dyn InputTarget>,
        parent: PopupParent,
    ) {
        let surface = match Surface::new(&self.compositor, &self.qh) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(target: "Main", "popup surface: {e}");
                input.dismissed();
                return;
            }
        };
        let viewport = self
            .popups
            .viewporter
            .get_viewport(surface.wl_surface(), &self.qh, ());
        // xdg_popup.grab is only honored before the popup's first commit, so
        // the grab and the commit below must stay in that order.
        let popup = match Popup::from_surface(
            Some(&parent.xdg_surface),
            positioner,
            &self.qh,
            surface,
            &self.popups.xdg_shell,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(target: "Main", "menu popup: {e}");
                viewport.destroy();
                input.dismissed();
                return;
            }
        };
        popup.xdg_popup().grab(&self.seat, serial);
        popup.wl_surface().commit();
        self.protocol.register_popup(
            popup.wl_surface().clone(),
            parent.surface.id(),
            generation,
            input.clone(),
        );
        self.popups.current = PopupMapping::Unmapped {
            live: LivePopup {
                generation,
                popup,
                viewport,
                place,
                input,
            },
        };
    }

    /// Drops the request when `generation` no longer owns the popup.
    pub(crate) fn map_popup(&mut self, generation: Generation) {
        if self.popups.current.generation() != Some(generation) {
            return;
        }
        let Some(buffer) = self
            .popups
            .pool
            .as_mut()
            .and_then(|pool| crate::wl_state::draw_from_pixels(pool, &ARMED_PIXEL, 1, 1))
            .map(crate::wl_state::AttachedBuffer::Shm)
        else {
            tracing::error!(target: "Main", "popup: no mapping buffer");
            self.dismiss_popup(generation);
            return;
        };
        self.popups
            .current
            .map_armed(&self.popups.xdg_shell, buffer);
    }

    pub(crate) fn reposition_popup(&mut self, generation: Generation, place: MenuPlacement) {
        if self.popups.current.generation() != Some(generation) {
            return;
        }
        self.popups
            .current
            .reposition(&self.popups.xdg_shell, place);
    }

    pub(crate) fn paint_popup(&mut self, paint: MenuPaint) {
        if self.popups.current.generation() != Some(paint.generation) {
            return;
        }
        let Some(buffer) = self
            .popups
            .pool
            .as_mut()
            .and_then(|pool| {
                crate::wl_state::draw_from_pixels(
                    pool,
                    &paint.pixels,
                    paint.buffer.w,
                    paint.buffer.h,
                )
            })
            .map(crate::wl_state::AttachedBuffer::Shm)
        else {
            return;
        };
        self.popups
            .current
            .paint(&self.popups.xdg_shell, buffer, &paint);
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
    /// SCTK has already acked the serial.
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        _: PopupConfigure,
    ) {
        if let Some(live) = self
            .popups
            .current
            .live()
            .filter(|live| &live.popup == popup)
        {
            live.input.configured();
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
