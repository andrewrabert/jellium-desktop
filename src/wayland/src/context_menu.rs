use jfn_platform_abi::{
    ContextMenuBackend, ContextMenuStyle, Delivery, DisplayBackend, JfnContextMenuRequest,
    JsMenuContextMenu, context_menu_style,
};

use crate::runtime::WlRuntime;

pub(crate) fn backend(rt: &'static WlRuntime) -> Box<dyn ContextMenuBackend> {
    match context_menu_style(DisplayBackend::Wayland) {
        ContextMenuStyle::PlatformMenu => Box::new(XdgPopupContextMenu { rt }),
        ContextMenuStyle::JsMenu => Box::new(JsMenuContextMenu),
    }
}

struct XdgPopupContextMenu {
    rt: &'static WlRuntime,
}

impl ContextMenuBackend for XdgPopupContextMenu {
    fn show(&self, req: JfnContextMenuRequest) {
        let Delivery::Native(cb) = req.delivery else {
            debug_assert!(false, "XdgPopupContextMenu requires Delivery::Native");
            return;
        };
        let items = req
            .items
            .into_iter()
            .map(|i| jfn_menu::MenuItem {
                id: i.id,
                label: i.label,
                enabled: i.enabled,
                separator: i.separator,
            })
            .collect();
        crate::popup::show(self.rt, items, req.x, req.y, cb);
    }
}
