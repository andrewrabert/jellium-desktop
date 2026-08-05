//! CEF's Alloy OSR popup renders `<select>` hover/selection highlights as
//! opaque black on macOS, so its popup runs invisibly and a native NSMenu is
//! presented in its place.

use jfn_platform_abi::{MENU_DISMISSED, MenuHost, MenuRequest, menu_has_selectable};

use crate::ns_menu::{MenuEntry, MenuSpec, present_on_main};

pub(crate) struct NsMenuHost;

impl MenuHost for NsMenuHost {
    fn open(&self, req: MenuRequest) {
        if !menu_has_selectable(&req.items) {
            req.on_selected.resolve(MENU_DISMISSED);
            return;
        }
        let entries = req
            .items
            .into_iter()
            .map(|it| MenuEntry {
                checked: it.id == req.initial && !it.separator,
                title: it.label,
                tag: it.id,
                enabled: it.enabled,
                separator: it.separator,
            })
            .collect();
        let spec = MenuSpec {
            entries,
            x: req.x,
            y: req.y,
            positioning_tag: (req.initial >= 0).then_some(req.initial),
            min_width: (req.width > 0).then_some(req.width),
        };
        present_on_main(spec, Some(req.on_selected));
    }
}
