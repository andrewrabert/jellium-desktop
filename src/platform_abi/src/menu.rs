use std::ffi::c_int;
use std::num::NonZeroU64;

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
pub enum MenuScript {
    SelectMenu,
}

pub fn menu_scripts(kind: MenuKind) -> &'static [MenuScript] {
    match (menu_delivery(kind), kind) {
        (MenuDelivery::Page, MenuKind::Dropdown) => &[MenuScript::SelectMenu],
        _ => &[],
    }
}

#[derive(Copy, Clone)]
pub enum MenuDelivery {
    Host(&'static dyn MenuHost),
    Composited,
    Page,
}

pub fn menu_delivery(kind: MenuKind) -> MenuDelivery {
    crate::get().menu_delivery(kind)
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
