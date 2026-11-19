use std::ffi::c_int;
use std::num::NonZeroU64;

/// Identifies one popup across its whole life; a surface drops anything
/// naming a generation it no longer owns.
pub type Generation = NonZeroU64;

/// Selection value meaning "nothing was chosen".
pub const MENU_DISMISSED: c_int = -1;

/// Resolves exactly once: through [`MenuSelection::resolve`], or with
/// [`MENU_DISMISSED`] when dropped first.
pub struct MenuSelection {
    resolve: Option<Box<dyn FnOnce(c_int) + Send>>,
}

impl MenuSelection {
    pub fn new(f: impl FnOnce(c_int) + Send + 'static) -> MenuSelection {
        MenuSelection {
            resolve: Some(Box::new(f)),
        }
    }

    /// Runs on the calling thread; never call it while holding a lock the
    /// callback can reach.
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

/// True when at least one item is enabled and not a separator.
pub fn menu_has_selectable(items: &[MenuItem]) -> bool {
    items.iter().any(|i| i.enabled && !i.separator)
}

/// `initial` when it names an enabled, non-separator item, else
/// [`MENU_DISMISSED`].
pub fn menu_initial_row(items: &[MenuItem], initial: c_int) -> c_int {
    usize::try_from(initial)
        .ok()
        .and_then(|i| items.get(i))
        .filter(|i| i.enabled && !i.separator)
        .map_or(MENU_DISMISSED, |_| initial)
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MenuHandle(NonZeroU64);

impl MenuHandle {
    pub fn new(id: NonZeroU64) -> MenuHandle {
        MenuHandle(id)
    }
}

pub struct MenuRequest {
    pub items: Vec<MenuItem>,
    pub anchor: crate::geometry::WindowPoint,
    pub basis: crate::geometry::WindowBasis,
    /// Desired logical width; `<= 0` is content-sized.
    pub width: c_int,
    /// Row highlighted at open; `-1` for none.
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

/// Native menu access is borrowed from the platform lease.
///
/// ```compile_fail
/// fn escape(lease: &jfn_platform_abi::PlatformLease) -> jfn_platform_abi::MenuDelivery<'static> {
///     lease.menu_delivery(jfn_platform_abi::MenuKind::ContextMenu)
/// }
/// ```
#[derive(Copy, Clone)]
pub enum MenuDelivery<'a> {
    Host(&'a dyn MenuHost),
    Composited,
    Page,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MenuOpenError {
    #[error("the menu has no selectable item")]
    NothingSelectable,
    #[error("the menu engine is not running")]
    EngineUnavailable,
    #[error("no window to raise the menu from")]
    NoWindow,
    #[error("the window state the menu was requested under has been replaced")]
    StaleBasis,
    #[error("the menu request could not be delivered to the thread that shows it")]
    Undeliverable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MenuHideError {
    #[error("the end request could not be delivered to the menu thread")]
    Undeliverable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MenuShutdownError {
    #[error("menu thread panicked")]
    ThreadPanicked,
    #[error("the end request could not be delivered to the menu thread")]
    Undeliverable,
}

pub trait MenuHost: Send + Sync {
    fn warm(&self) {}

    fn open(&self, req: MenuRequest) -> Result<MenuHandle, MenuOpenError>;

    fn hide(&self, _handle: MenuHandle) -> Result<(), MenuHideError> {
        Ok(())
    }

    /// Resolves any pending selection with [`MENU_DISMISSED`].
    fn shutdown(&self) -> Result<(), MenuShutdownError> {
        Ok(())
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub struct MenuPlacement {
    pub anchor: crate::geometry::WindowPoint,
    /// The visible menu's own size in both spaces.
    pub view: crate::geometry::WindowExtent,
    pub serial: u32,
    pub basis: crate::geometry::WindowBasis,
}

pub struct MenuPaint {
    pub generation: Generation,
    /// Premultiplied BGRA, `buffer.w` x `buffer.h`.
    pub pixels: Vec<u8>,
    /// Physical (buffer) size of the whole menu.
    pub buffer: crate::geometry::PhysicalSize,
    /// Scroll offset into the buffer, physical px.
    pub scroll: c_int,
    /// The visible crop's own size in both spaces.
    pub view: crate::geometry::WindowExtent,
    pub basis: crate::geometry::WindowBasis,
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub struct MenuMetrics {
    /// Physical pixels per logical pixel.
    pub scale: crate::geometry::Scale,
    /// Window height, physical px, that a width-constrained menu is clamped to;
    /// `None` leaves every menu full height.
    pub clamp_ph: Option<c_int>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MenuClose {
    Finished,
    External,
}

/// The platform surface a software-rendered menu drives. Every method is called
/// from the menu's own thread and must not block it.
pub trait PopupSurface: Send + Sync {
    fn metrics(&self) -> MenuMetrics;

    fn window_basis(&self) -> crate::geometry::WindowBasis;

    fn create(&self, generation: Generation, place: MenuPlacement);

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
