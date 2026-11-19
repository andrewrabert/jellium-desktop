use jfn_linux_util::menu::MenuInputEvent;
use jfn_platform_abi::{
    Generation, LogicalSize, MENU_DISMISSED, MenuClose, MenuHandle, MenuHost, MenuMetrics,
    MenuOpenError, MenuPaint, MenuPlacement, MenuShutdownError, PopupSurface, WindowBasis,
};

use crate::popup_protocol::{PopupCommand, PopupParent};
use crate::protocol::SurfacePoint;
use crate::runtime::WlRuntime;
use smithay_client_toolkit::shell::{WaylandSurface, xdg::XdgSurface as _};
use wayland_client::protocol::wl_surface::WlSurface;

pub(crate) struct WlPopupSurface {
    pub(crate) rt: &'static WlRuntime,
}

impl PopupSurface for WlPopupSurface {
    fn metrics(&self) -> MenuMetrics {
        let extent = self.rt.window().window_extent();
        MenuMetrics {
            scale: extent.map_or_else(|| self.rt.window().scale(), |e| e.scale()),
            clamp_ph: extent.map(|e| e.physical().h()),
        }
    }

    fn window_basis(&self) -> WindowBasis {
        self.rt.window().basis()
    }

    fn create(&self, generation: Generation, place: MenuPlacement) {
        let Some(window) = self.rt.root().window() else {
            if let Some(menu) = self.rt.menu() {
                menu.on_done(generation);
            }
            return;
        };
        send(
            self.rt,
            PopupCommand::Create {
                generation,
                place,
                parent: PopupParent {
                    surface: window.wl_surface().clone(),
                    xdg_surface: window.xdg_surface().clone(),
                },
                input: std::sync::Arc::new(MenuInput {
                    rt: self.rt,
                    generation,
                    popup: std::sync::OnceLock::new(),
                }),
            },
        );
    }

    fn present(&self, paint: MenuPaint) {
        send(self.rt, PopupCommand::Paint(paint));
    }

    fn destroy(&self, generation: Generation, _reason: MenuClose) {
        send(self.rt, PopupCommand::Destroy { generation });
    }
}

fn send(rt: &WlRuntime, command: PopupCommand) {
    if let Some(input) = rt.input() {
        input.popup(command);
    }
}

pub(crate) struct WlMenuHost {
    pub(crate) rt: &'static WlRuntime,
}
impl MenuHost for WlMenuHost {
    fn open(&self, request: jfn_platform_abi::MenuRequest) -> Result<MenuHandle, MenuOpenError> {
        let Some(menu) = self.rt.menu() else {
            request.on_selected.resolve(MENU_DISMISSED);
            return Err(MenuOpenError::EngineUnavailable);
        };
        menu.open_triggered(request, self.rt.seat().last_input_serial())
    }
    fn hide(&self, handle: MenuHandle) -> Result<(), jfn_platform_abi::MenuHideError> {
        self.rt.menu().map_or(Ok(()), |menu| menu.hide(handle))
    }
    fn shutdown(&self) -> Result<(), MenuShutdownError> {
        self.rt.menu().map_or(Ok(()), MenuHost::shutdown)
    }
}

/// Content adapter. Protocol routing invokes the registered destination;
/// only this adapter knows the destination contains a software menu.
pub(crate) struct MenuInput {
    rt: &'static WlRuntime,
    generation: Generation,
    popup: std::sync::OnceLock<WlSurface>,
}
impl crate::protocol::InputTarget for MenuInput {
    fn registered(&self, surface: &WlSurface) {
        let _ = self.popup.set(surface.clone());
    }
    fn configured(&self, size: LogicalSize) {
        if let Some(menu) = self.rt.menu() {
            menu.on_ready(self.generation, size);
        }
    }
    fn dismissed(&self) {
        if let Some(menu) = self.rt.menu() {
            menu.on_done(self.generation);
        }
    }
    fn motion(&self, at: &SurfacePoint, _: u32) {
        self.pointer(at, false);
    }
    fn button(&self, _: u32, pressed: bool, at: &SurfacePoint, _: u32) {
        if pressed {
            self.pointer(at, true);
        }
    }
    fn scroll(&self, _: &SurfacePoint, _: i32, dy: i32, _: u32) {
        self.deliver(MenuInputEvent::Scroll(dy));
    }
    fn key(&self, event: &smithay_client_toolkit::seat::keyboard::KeyEvent, pressed: bool, _: u32) {
        if pressed {
            self.deliver(MenuInputEvent::Key(event.keysym.raw()));
        }
    }
}
impl MenuInput {
    fn pointer(&self, at: &SurfacePoint, press: bool) {
        let Some(at) = self.popup.get().and_then(|popup| at.menu_point(popup)) else {
            return;
        };
        self.deliver(MenuInputEvent::Pointer { at, press });
    }

    fn deliver(&self, event: MenuInputEvent) {
        if let Some(menu) = self.rt.menu() {
            menu.input(self.generation, event);
        }
    }
}
