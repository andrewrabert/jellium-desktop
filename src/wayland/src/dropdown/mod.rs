mod subsurface;
mod xdg_popup;

use jfn_platform_abi::{
    DisplayBackend, DropdownBackend, DropdownStyle, JsMenuDropdown, dropdown_style,
};

use subsurface::SubsurfaceDropdown;
use xdg_popup::XdgPopupDropdown;

use crate::runtime::WlRuntime;

pub(crate) fn backend(rt: &'static WlRuntime) -> Box<dyn DropdownBackend> {
    match dropdown_style(DisplayBackend::Wayland) {
        DropdownStyle::PlatformMenu => Box::new(XdgPopupDropdown { rt }),
        DropdownStyle::Composited => Box::new(SubsurfaceDropdown { rt }),
        DropdownStyle::JsMenu => Box::new(JsMenuDropdown),
    }
}
