use jfn_linux_util::menu::{MenuInputEvent, MenuPoint};
use jfn_platform_abi::{
    Generation, LogicalPoint, MenuClose, MenuMetrics, MenuPaint, MenuPlacement, PopupSurface,
};

use crate::popup_protocol::{PopupCommand, PopupParent};
use crate::runtime::WlRuntime;
use smithay_client_toolkit::shell::{WaylandSurface, xdg::XdgSurface as _};

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

    fn arm(&self, generation: Generation, anchor: LogicalPoint, serial: u32) {
        let Some(window) = self.rt.root().window() else {
            self.rt.menu().on_done(generation);
            return;
        };
        let serial = if serial != 0 {
            serial
        } else {
            self.rt.seat().last_input_serial()
        };
        send(
            self.rt,
            PopupCommand::Create {
                generation,
                anchor,
                serial,
                parent: PopupParent {
                    surface: window.wl_surface().clone(),
                    xdg_surface: window.xdg_surface().clone(),
                },
                input: std::sync::Arc::new(MenuInput {
                    rt: self.rt,
                    generation,
                }),
            },
        );
    }

    fn map_armed(&self, generation: Generation) {
        send(self.rt, PopupCommand::Map { generation });
    }

    fn reposition(&self, generation: Generation, place: MenuPlacement) {
        send(self.rt, PopupCommand::Reposition { generation, place });
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

/// Captures the trigger when content requests a popup, before layout work.
/// Ordinary button presses never create speculative protocol objects.
pub(crate) struct WlMenuHost {
    pub(crate) rt: &'static WlRuntime,
}
impl jfn_platform_abi::MenuHost for WlMenuHost {
    fn open(&self, request: jfn_platform_abi::MenuRequest) {
        self.rt
            .menu()
            .open_triggered(request, self.rt.seat().last_input_serial());
    }
    fn hide(&self) {
        self.rt.menu().hide();
    }
    fn shutdown(&self) {
        self.rt.menu().shutdown();
    }
}

/// Content adapter. Protocol routing invokes the registered destination;
/// only this adapter knows the destination contains a software menu.
pub(crate) struct MenuInput {
    rt: &'static WlRuntime,
    generation: Generation,
}
impl crate::protocol::InputTarget for MenuInput {
    fn configured(&self) {
        self.rt.menu().on_ready(self.generation);
    }
    fn dismissed(&self) {
        self.rt.menu().on_done(self.generation);
    }
    fn motion(&self, position: (f64, f64), _: u32, leave: bool) {
        if !leave {
            self.pointer(position, false);
        }
    }
    fn button(&self, _: u32, pressed: bool, position: (f64, f64), _: u32) {
        if pressed {
            self.pointer(position, true);
        }
    }
    fn scroll(&self, _: (f64, f64), _: i32, dy: i32, _: u32) {
        self.rt
            .menu()
            .input(self.generation, MenuInputEvent::Scroll(dy));
    }
    fn key(&self, event: &smithay_client_toolkit::seat::keyboard::KeyEvent, pressed: bool, _: u32) {
        if pressed {
            self.rt
                .menu()
                .input(self.generation, MenuInputEvent::Key(event.keysym.raw()));
        }
    }
}
impl MenuInput {
    fn pointer(&self, position: (f64, f64), press: bool) {
        self.rt.menu().input(
            self.generation,
            MenuInputEvent::Pointer {
                at: MenuPoint::Logical {
                    x: position.0 as i32,
                    y: position.1 as i32,
                },
                press,
            },
        );
    }
}
