//! App-level CEF context-menu items appended to every browser's menu.
//!
//! The build/dispatch closures returned here are installed via
//! `JfnCefLayer::set_context_menu_builder_rust` /
//! `set_context_menu_dispatcher_rust` by each business wrapper.

use cef::rc::ConvertReturnValue;
use cef::{ImplMenuModel, MenuModel, sys};
use std::os::raw::{c_int, c_void};

// Command IDs numbered from cef_menu_id_t::MENU_ID_USER_FIRST.
const MENU_ID_USER_FIRST: c_int = sys::cef_menu_id_t::MENU_ID_USER_FIRST as c_int;
pub const MENU_ID_TOGGLE_FULLSCREEN: c_int = MENU_ID_USER_FIRST;
pub const MENU_ID_CLIENT_SETTINGS: c_int = MENU_ID_USER_FIRST + 1;
pub const MENU_ID_EXIT: c_int = MENU_ID_USER_FIRST + 2;

const ITEMS: [(c_int, &str); 3] = [
    (MENU_ID_TOGGLE_FULLSCREEN, "Toggle Fullscreen"),
    (MENU_ID_CLIENT_SETTINGS, "Settings"),
    (MENU_ID_EXIT, "Exit"),
];

const RESTRICTED_ITEMS: [(c_int, &str); 2] = [
    (MENU_ID_TOGGLE_FULLSCREEN, "Toggle Fullscreen"),
    (MENU_ID_EXIT, "Exit"),
];

use jfn_playback::shutdown::jfn_shutdown_initiate;

/// Build closure for [`JfnCefLayer::set_context_menu_builder_rust`].
/// The slot invocation adds one ref to the menu model before calling this,
/// so we adopt it via `wrap_result` (no extra add_ref needed).
pub fn build_closure() -> Box<crate::client::ContextBuilderFn> {
    Box::new(|raw: *mut c_void| {
        if raw.is_null() {
            return;
        }
        let m: MenuModel = (raw as *mut sys::_cef_menu_model_t).wrap_result();
        for (id, label) in ITEMS {
            m.add_item(id, Some(&cef::CefString::from(label)));
        }
    })
}

/// The normal app menu, as a host-menu request in window coordinates.
pub fn open_at(x: c_int, y: c_int) {
    open_host_at(x, y, &ITEMS);
}

/// The app menu while the combined Settings/About overlay is open.
pub fn open_restricted_at(x: c_int, y: c_int) {
    open_host_at(x, y, &RESTRICTED_ITEMS);
}

fn open_host_at(x: c_int, y: c_int, definition: &[(c_int, &str)]) {
    let jfn_platform_abi::MenuDelivery::Host(host) =
        jfn_platform_abi::menu_delivery(jfn_platform_abi::MenuKind::ContextMenu)
    else {
        return;
    };
    let items = definition
        .iter()
        .map(|&(id, label)| jfn_platform_abi::MenuItem {
            id,
            label: label.to_owned(),
            enabled: true,
            separator: false,
        })
        .collect();
    host.open(jfn_platform_abi::MenuRequest {
        items,
        x,
        y,
        width: 0,
        initial: jfn_platform_abi::MENU_DISMISSED,
        on_selected: jfn_platform_abi::MenuSelection::new(|id| {
            dispatch(id);
        }),
    });
}

/// Dispatch closure for [`JfnCefLayer::set_context_menu_dispatcher_rust`].
pub fn dispatch_closure() -> Box<crate::client::ContextDispatcherFn> {
    Box::new(dispatch)
}

fn dispatch(cmd: c_int) -> bool {
    if cmd == MENU_ID_TOGGLE_FULLSCREEN {
        if let Some(p) = jfn_platform_abi::try_get() {
            p.toggle_fullscreen();
        }
        true
    } else if cmd == MENU_ID_CLIENT_SETTINGS {
        jfn_platform_abi::request_client_settings();
        true
    } else if cmd == MENU_ID_EXIT {
        jfn_shutdown_initiate();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        ITEMS, MENU_ID_CLIENT_SETTINGS, MENU_ID_EXIT, MENU_ID_TOGGLE_FULLSCREEN, RESTRICTED_ITEMS,
        dispatch,
    };

    static OPENED: AtomicUsize = AtomicUsize::new(0);

    fn opened() {
        OPENED.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn normal_and_restricted_definitions_are_exact() {
        assert_eq!(
            ITEMS,
            [
                (MENU_ID_TOGGLE_FULLSCREEN, "Toggle Fullscreen"),
                (MENU_ID_CLIENT_SETTINGS, "Settings"),
                (MENU_ID_EXIT, "Exit"),
            ]
        );
        assert_eq!(
            RESTRICTED_ITEMS,
            [
                (MENU_ID_TOGGLE_FULLSCREEN, "Toggle Fullscreen"),
                (MENU_ID_EXIT, "Exit"),
            ]
        );
    }

    #[test]
    fn settings_dispatch_is_handled_and_invokes_bridge() {
        jfn_platform_abi::set_client_settings_handler(opened);
        let before = OPENED.load(Ordering::Relaxed);
        assert!(dispatch(MENU_ID_CLIENT_SETTINGS));
        assert_eq!(OPENED.load(Ordering::Relaxed), before + 1);
    }
}
