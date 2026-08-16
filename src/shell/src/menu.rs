//! The edit menu, raised through the platform's own menu host.

use std::os::raw::c_int;

use jfn_platform_abi::{
    LogicalPoint, MenuDelivery, MenuItem, MenuKind, MenuRequest, MenuSelection,
};

use crate::actor::{Target, Work};
use crate::fields::Snapshot;
use crate::lang::Strings;

/// Command IDs numbered past the app menu's, so a host that dispatches by ID
/// never confuses the two menus.
const MENU_ID_EDIT_FIRST: c_int = jfn_cef::app_menu::MENU_ID_EXIT + 1;

/// The edit menu's items, in order.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Item {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
}

impl Item {
    pub const ALL: [Item; 6] = [
        Item::Undo,
        Item::Redo,
        Item::Cut,
        Item::Copy,
        Item::Paste,
        Item::SelectAll,
    ];

    pub fn command(self) -> jfn_input::EditCommand {
        use jfn_input::EditCommand as E;
        match self {
            Item::Undo => E::Undo,
            Item::Redo => E::Redo,
            Item::Cut => E::Cut,
            Item::Copy => E::Copy,
            Item::Paste => E::Paste,
            Item::SelectAll => E::SelectAll,
        }
    }

    pub fn label(self, strings: &Strings) -> &'static str {
        match self {
            Item::Undo => strings.undo,
            Item::Redo => strings.redo,
            Item::Cut => strings.cut,
            Item::Copy => strings.copy,
            Item::Paste => strings.paste,
            Item::SelectAll => strings.select_all,
        }
    }

    /// Enabled exactly when it would change something; Paste always.
    pub fn enabled(self, field: &Snapshot) -> bool {
        let edit = field.edit_state();
        match self {
            Item::Undo => edit.undo,
            Item::Redo => edit.redo,
            Item::Cut => edit.cut,
            Item::Copy => edit.copy,
            Item::Paste => true,
            Item::SelectAll => edit.select_all,
        }
    }

    fn id(self) -> c_int {
        MENU_ID_EDIT_FIRST
            + match self {
                Item::Undo => 0,
                Item::Redo => 1,
                Item::Cut => 2,
                Item::Copy => 3,
                Item::Paste => 4,
                Item::SelectAll => 5,
            }
    }

    fn from_id(id: c_int) -> Option<Item> {
        Item::ALL.into_iter().find(|item| item.id() == id)
    }
}

/// Raises the edit menu for `field` at `anchor`, in window coordinates. A
/// selection posts [`crate::actor::Work::EditAt`] naming `field`; a dismissal
/// posts nothing. No accelerator text is drawn.
pub fn open_edit(field: &Snapshot, anchor: LogicalPoint, strings: &Strings) {
    let MenuDelivery::Host(host) = jfn_platform_abi::menu_delivery(MenuKind::ContextMenu) else {
        return;
    };
    let items = Item::ALL
        .into_iter()
        .map(|item| MenuItem {
            id: item.id(),
            label: item.label(strings).to_owned(),
            enabled: item.enabled(field),
            separator: false,
        })
        .collect();
    let id = field.id.clone();
    host.open(MenuRequest {
        items,
        x: anchor.x,
        y: anchor.y,
        width: 0,
        initial: jfn_platform_abi::MENU_DISMISSED,
        on_selected: MenuSelection::new(move |selected| {
            let Some(item) = Item::from_id(selected) else {
                return;
            };
            crate::post(Work::EditAt {
                field: Target::Named(id),
                command: item.command(),
            });
        }),
    });
}
