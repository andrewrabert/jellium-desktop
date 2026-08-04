use std::ffi::c_int;
use std::num::NonZeroU64;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::Serialize;

use crate::DisplayBackend;

/// Identifies one popup across its whole life; a surface drops anything
/// naming a generation it no longer owns.
pub type Generation = NonZeroU64;

/// Selection value meaning "nothing was chosen".
pub const MENU_DISMISSED: c_int = -1;

/// Runs once, on whatever thread resolves the pick, with no lock held.
pub type MenuSelectionFn = Box<dyn FnOnce(c_int) + Send>;

#[derive(Clone)]
pub struct MenuItem {
    pub id: c_int,
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
}

pub struct MenuRequest {
    pub items: Vec<MenuItem>,
    /// Anchor in logical (view) coordinates.
    pub x: c_int,
    pub y: c_int,
    /// Desired logical width; `<= 0` is content-sized.
    pub width: c_int,
    /// Row highlighted at open; `-1` for none.
    pub initial: c_int,
    pub on_selected: MenuSelectionFn,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MenuKind {
    ContextMenu,
    Dropdown,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MenuStyle {
    Platform,
    Js,
    Page,
    Composited,
}

pub fn menu_style(kind: MenuKind, backend: DisplayBackend) -> MenuStyle {
    match (kind, backend) {
        (MenuKind::ContextMenu, DisplayBackend::Windows) => MenuStyle::Js,
        (MenuKind::ContextMenu, _) => MenuStyle::Platform,
        (MenuKind::Dropdown, DisplayBackend::Wayland | DisplayBackend::MacOS) => {
            MenuStyle::Platform
        }
        (MenuKind::Dropdown, DisplayBackend::X11) => MenuStyle::Page,
        (MenuKind::Dropdown, DisplayBackend::Windows) => MenuStyle::Composited,
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MenuScript {
    ContextMenu,
    SelectMenu,
}

pub fn menu_scripts(kind: MenuKind) -> &'static [MenuScript] {
    let Some(p) = crate::try_get() else {
        return &[];
    };
    match menu_style(kind, p.display()) {
        MenuStyle::Platform | MenuStyle::Composited => &[],
        MenuStyle::Js | MenuStyle::Page => match kind {
            MenuKind::ContextMenu => &[MenuScript::ContextMenu],
            MenuKind::Dropdown => &[MenuScript::SelectMenu],
        },
    }
}

#[derive(Copy, Clone)]
pub enum MenuDelivery {
    Host(&'static dyn MenuHost),
    Composited,
    Page,
}

pub fn menu_delivery(kind: MenuKind) -> MenuDelivery {
    let Some(p) = crate::try_get() else {
        return MenuDelivery::Page;
    };
    match menu_style(kind, p.display()) {
        MenuStyle::Platform => match p.menu() {
            Some(host) => MenuDelivery::Host(host),
            None => MenuDelivery::Page,
        },
        MenuStyle::Js => MenuDelivery::Host(js_menu_host()),
        MenuStyle::Composited => MenuDelivery::Composited,
        MenuStyle::Page => MenuDelivery::Page,
    }
}

pub trait MenuHost: Send + Sync {
    fn warm(&self) {}

    /// Replaces any menu already open, and returns before the menu is drawn.
    fn open(&self, req: MenuRequest);

    /// Tears the menu down without firing its selection callback.
    fn hide(&self) {}

    /// Fires any pending selection callback with [`MENU_DISMISSED`].
    fn shutdown(&self) {}
}

pub trait MenuJsBridge: Send + Sync {
    /// Park `on_selected` until the page reports a pick, then evaluate
    /// `script` in the page.
    fn open(&self, script: String, on_selected: MenuSelectionFn);
}

static MENU_JS_BRIDGE: RwLock<Option<Arc<dyn MenuJsBridge>>> = RwLock::new(None);

/// Replaces any previously installed bridge.
pub fn install_menu_js_bridge(bridge: Arc<dyn MenuJsBridge>) {
    *MENU_JS_BRIDGE.write() = Some(bridge);
}

pub struct JsMenuHost;

impl MenuHost for JsMenuHost {
    fn open(&self, req: MenuRequest) {
        let bridge = MENU_JS_BRIDGE.read().clone();
        let (Some(bridge), Some(items)) = (bridge, items_js_json(&req.items)) else {
            (req.on_selected)(MENU_DISMISSED);
            return;
        };
        bridge.open(
            format!("window._showContextMenu({items},{},{})", req.x, req.y),
            req.on_selected,
        );
    }
}

pub fn js_menu_host() -> &'static JsMenuHost {
    &JsMenuHost
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MenuPlacement {
    /// Anchor in logical (view) coordinates.
    pub x: c_int,
    pub y: c_int,
    /// Logical (compositor) size of the visible menu.
    pub lw: c_int,
    pub lh: c_int,
    /// Physical (buffer) size of the visible menu.
    pub pw: c_int,
    pub ph: c_int,
}

pub struct MenuPaint {
    pub generation: Generation,
    /// Premultiplied BGRA, `pw` x `ph`.
    pub pixels: Vec<u8>,
    pub pw: c_int,
    pub ph: c_int,
    /// Scroll offset into the buffer, physical px.
    pub scroll: c_int,
    /// Visible height of the crop, physical px.
    pub view_ph: c_int,
    /// Logical size the crop is scaled to.
    pub lw: c_int,
    pub lh: c_int,
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub struct MenuMetrics {
    /// Physical pixels per logical pixel.
    pub scale: f32,
    /// Window height, physical px, that a width-constrained menu is clamped to;
    /// `None` leaves every menu full height.
    pub clamp_ph: Option<c_int>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MenuClose {
    Finished,
    Speculative,
    External,
}

/// The platform surface a software-rendered menu drives. Every method is called
/// from the menu's own thread and must not block it.
pub trait PopupSurface: Send + Sync {
    fn metrics(&self) -> MenuMetrics;

    /// `serial` is the input serial a grab must cite; backends that do not grab
    /// on a serial ignore it.
    fn create(&self, generation: Generation, place: MenuPlacement, serial: u32);

    fn reposition(&self, generation: Generation, place: MenuPlacement);

    fn present(&self, paint: MenuPaint);

    fn destroy(&self, generation: Generation, reason: MenuClose);
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
    fn from_item(item: &'a MenuItem) -> JsMenuEntry<'a> {
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

fn items_js_json(items: &[MenuItem]) -> Option<String> {
    let entries: Vec<JsMenuEntry> = items.iter().map(JsMenuEntry::from_item).collect();
    jfn_js_json::to_js_json(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: c_int, label: &str) -> MenuItem {
        MenuItem {
            id,
            label: label.to_string(),
            enabled: true,
            separator: false,
        }
    }

    fn separator() -> MenuItem {
        MenuItem {
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

    #[test]
    fn context_menu_is_platform_drawn_off_windows() {
        assert_eq!(
            menu_style(MenuKind::ContextMenu, DisplayBackend::Wayland),
            MenuStyle::Platform
        );
        assert_eq!(
            menu_style(MenuKind::ContextMenu, DisplayBackend::Windows),
            MenuStyle::Js
        );
    }

    #[test]
    fn dropdown_style_is_per_backend() {
        assert_eq!(
            menu_style(MenuKind::Dropdown, DisplayBackend::Wayland),
            MenuStyle::Platform
        );
        assert_eq!(
            menu_style(MenuKind::Dropdown, DisplayBackend::X11),
            MenuStyle::Page
        );
        assert_eq!(
            menu_style(MenuKind::Dropdown, DisplayBackend::Windows),
            MenuStyle::Composited
        );
        assert_eq!(
            menu_style(MenuKind::Dropdown, DisplayBackend::MacOS),
            MenuStyle::Platform
        );
    }
}
