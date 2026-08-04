use std::ffi::c_int;

use serde::Serialize;

use crate::DisplayBackend;

pub struct JfnMenuItem {
    pub id: c_int,
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
}

/// Selection value meaning "nothing was chosen".
pub const MENU_DISMISSED: c_int = -1;

/// Selection callback: receives the chosen item id, or [`MENU_DISMISSED`].
pub type MenuSelectionFn = Box<dyn FnOnce(c_int) + Send>;

pub struct JsMenuChannel {
    pub exec: Box<dyn FnOnce(String)>,
    /// Stores `on_selected` until the menuItemSelected / menuDismissed IPC
    /// fires it.
    pub park_selection: Box<dyn FnOnce(MenuSelectionFn)>,
    pub on_selected: MenuSelectionFn,
}

pub enum Delivery {
    Native(MenuSelectionFn),
    Js(JsMenuChannel),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DeliveryKind {
    Native,
    Js,
}

pub struct JfnContextMenuRequest {
    /// Logical (CEF view) coordinates of the click, not physical pixels.
    pub x: c_int,
    pub y: c_int,
    pub items: Vec<JfnMenuItem>,
    pub delivery: Delivery,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ContextMenuStyle {
    PlatformMenu,
    JsMenu,
}

pub fn context_menu_style(b: DisplayBackend) -> ContextMenuStyle {
    match b {
        DisplayBackend::Wayland => ContextMenuStyle::PlatformMenu,
        DisplayBackend::X11 => ContextMenuStyle::PlatformMenu,
        DisplayBackend::Windows => ContextMenuStyle::JsMenu,
        DisplayBackend::MacOS => ContextMenuStyle::PlatformMenu,
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ContextMenuScript {
    ContextMenu,
}

pub trait ContextMenuBackend: Send + Sync {
    fn scripts(&self) -> &'static [ContextMenuScript] {
        &[]
    }
    fn delivery_kind(&self) -> DeliveryKind {
        DeliveryKind::Native
    }
    fn show(&self, req: JfnContextMenuRequest);
}

pub struct JsMenuContextMenu;

impl ContextMenuBackend for JsMenuContextMenu {
    fn scripts(&self) -> &'static [ContextMenuScript] {
        &[ContextMenuScript::ContextMenu]
    }

    fn delivery_kind(&self) -> DeliveryKind {
        DeliveryKind::Js
    }

    fn show(&self, req: JfnContextMenuRequest) {
        let Delivery::Js(js) = req.delivery else {
            debug_assert!(false, "JsMenuContextMenu requires Delivery::Js");
            return;
        };
        let Some(items) = items_js_json(&req.items) else {
            (js.on_selected)(MENU_DISMISSED);
            return;
        };
        (js.park_selection)(js.on_selected);
        (js.exec)(format!(
            "window._showContextMenu({items},{},{})",
            req.x, req.y
        ));
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum JsMenuEntry<'a> {
    Separator {
        sep: bool,
    },
    Item {
        id: c_int,
        label: &'a str,
        enabled: bool,
    },
}

impl<'a> JsMenuEntry<'a> {
    fn from_item(item: &'a JfnMenuItem) -> JsMenuEntry<'a> {
        if item.separator {
            JsMenuEntry::Separator { sep: true }
        } else {
            JsMenuEntry::Item {
                id: item.id,
                label: &item.label,
                enabled: item.enabled,
            }
        }
    }
}

/// `[{"sep":true},{"id":N,"label":"…","enabled":true}]`, escaped for embedding
/// in JS source.
fn items_js_json(items: &[JfnMenuItem]) -> Option<String> {
    let entries: Vec<JsMenuEntry> = items.iter().map(JsMenuEntry::from_item).collect();
    jfn_js_json::to_js_json(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: c_int, label: &str) -> JfnMenuItem {
        JfnMenuItem {
            id,
            label: label.to_string(),
            enabled: true,
            separator: false,
        }
    }

    fn separator() -> JfnMenuItem {
        JfnMenuItem {
            id: 0,
            label: String::new(),
            enabled: false,
            separator: true,
        }
    }

    #[test]
    fn separator_and_item_entries_serialize_in_order() {
        let items = vec![item(1, "Copy"), separator(), item(2, "Paste")];
        assert_eq!(
            items_js_json(&items).as_deref(),
            Some(
                r#"[{"id":1,"label":"Copy","enabled":true},{"sep":true},{"id":2,"label":"Paste","enabled":true}]"#
            )
        );
    }

    #[test]
    fn label_line_separators_are_escaped() {
        let items = vec![item(1, "a\u{2028}b\u{2029}c")];
        let json = items_js_json(&items).unwrap_or_default();
        assert!(json.contains("\\u2028"), "{json}");
        assert!(json.contains("\\u2029"), "{json}");
        assert!(!json.contains('\u{2028}'), "{json}");
        assert!(!json.contains('\u{2029}'), "{json}");
    }

    #[test]
    fn label_quotes_and_control_chars_are_escaped() {
        let items = vec![item(1, "a\"b\\c\nd\te\u{1}")];
        assert_eq!(
            items_js_json(&items).as_deref(),
            Some(r#"[{"id":1,"label":"a\"b\\c\nd\te\u0001","enabled":true}]"#)
        );
    }
}
