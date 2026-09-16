use std::ffi::c_int;
use std::num::NonZeroU64;

pub type Generation = NonZeroU64;

pub const MENU_DISMISSED: c_int = -1;

pub struct MenuSelection {
    resolve: Option<Box<dyn FnOnce(c_int) + Send>>,
}

impl MenuSelection {
    pub fn new(f: impl FnOnce(c_int) + Send + 'static) -> MenuSelection {
        MenuSelection {
            resolve: Some(Box::new(f)),
        }
    }

    pub fn resolve(mut self, id: c_int) {
        if let Some(f) = self.resolve.take() {
            f(id);
        }
    }
}

impl Drop for MenuSelection {
    fn drop(&mut self) {
        if let Some(f) = self.resolve.take() {
            f(MENU_DISMISSED);
        }
    }
}

#[derive(Clone)]
pub struct MenuItem {
    pub id: c_int,
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
}

pub fn menu_has_selectable(items: &[MenuItem]) -> bool {
    items.iter().any(|i| i.enabled && !i.separator)
}

pub fn menu_initial_row(items: &[MenuItem], initial: c_int) -> c_int {
    usize::try_from(initial)
        .ok()
        .and_then(|i| items.get(i))
        .filter(|i| i.enabled && !i.separator)
        .map_or(MENU_DISMISSED, |_| initial)
}

pub struct MenuRequest {
    pub items: Vec<MenuItem>,
    pub x: c_int,
    pub y: c_int,
    pub width: c_int,
    pub initial: c_int,
    pub on_selected: MenuSelection,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MenuKind {
    ContextMenu,
    Dropdown,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MenuScript {
    SelectMenu,
}

pub fn menu_scripts(kind: MenuKind) -> &'static [MenuScript] {
    let Some(lease) = crate::try_lease() else {
        return &[];
    };
    match (lease.menu_delivery(kind), kind) {
        (MenuDelivery::Page, MenuKind::Dropdown) => &[MenuScript::SelectMenu],
        _ => &[],
    }
}

#[derive(Copy, Clone)]
pub enum MenuDelivery<'a> {
    Host(&'a dyn MenuHost),
    Composited,
    Page,
}

pub trait MenuHost: Send + Sync {
    fn warm(&self) {}

    fn open(&self, req: MenuRequest);

    fn hide(&self) {}

    fn shutdown(&self) {}
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub struct MenuPlacement {
    pub anchor: crate::geometry::LogicalPoint,
    pub view: crate::geometry::WindowExtent,
}

pub struct MenuPaint {
    pub generation: Generation,
    pub pixels: Vec<u8>,
    pub buffer: crate::geometry::PhysicalSize,
    pub scroll: c_int,
    pub view: crate::geometry::WindowExtent,
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub struct MenuMetrics {
    pub scale: crate::geometry::Scale,
    pub clamp_ph: Option<c_int>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MenuClose {
    Finished,
    Speculative,
    External,
}

pub trait PopupSurface: Send + Sync {
    fn metrics(&self) -> MenuMetrics;

    fn arm(&self, generation: Generation, anchor: crate::geometry::LogicalPoint, serial: u32);

    fn map_armed(&self, generation: Generation);

    fn reposition(&self, generation: Generation, place: MenuPlacement);

    fn present(&self, paint: MenuPaint);

    fn destroy(&self, generation: Generation, reason: MenuClose);
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    fn item(id: c_int, enabled: bool, separator: bool) -> MenuItem {
        MenuItem {
            id,
            label: String::new(),
            enabled,
            separator,
        }
    }

    fn recorder() -> (Arc<Mutex<Vec<c_int>>>, MenuSelection) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let selection = MenuSelection::new(move |id| {
            if let Ok(mut v) = sink.lock() {
                v.push(id);
            }
        });
        (seen, selection)
    }

    #[test]
    fn a_dropped_selection_resolves_as_dismissed() {
        let (seen, sel) = recorder();
        drop(sel);
        assert_eq!(
            seen.lock().ok().map(|v| v.clone()),
            Some(vec![MENU_DISMISSED])
        );
    }

    #[test]
    fn a_resolved_selection_fires_once_with_its_id() {
        let (seen, sel) = recorder();
        sel.resolve(7);
        assert_eq!(seen.lock().ok().map(|v| v.clone()), Some(vec![7]));
    }

    #[test]
    fn separators_and_disabled_items_are_not_selectable() {
        assert!(!menu_has_selectable(&[]));
        assert!(!menu_has_selectable(&[
            item(0, false, true),
            item(1, false, false)
        ]));
        assert!(!menu_has_selectable(&[item(0, true, true)]));
        assert!(menu_has_selectable(&[
            item(0, false, true),
            item(1, true, false)
        ]));
    }

    #[test]
    fn an_initial_row_survives_only_when_it_is_selectable() {
        let items = [
            item(10, true, false),
            item(0, false, true),
            item(20, false, false),
        ];
        assert_eq!(menu_initial_row(&items, 0), 0);
        assert_eq!(menu_initial_row(&items, 1), MENU_DISMISSED);
        assert_eq!(menu_initial_row(&items, 2), MENU_DISMISSED);
        assert_eq!(menu_initial_row(&items, 3), MENU_DISMISSED);
        assert_eq!(menu_initial_row(&items, MENU_DISMISSED), MENU_DISMISSED);
    }
}
