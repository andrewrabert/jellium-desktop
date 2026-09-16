use jfn_platform_abi::{
    LogicalPoint, MenuDelivery, MenuItem, MenuKind, MenuRequest, MenuSelection,
};

const TOGGLE_FULLSCREEN: i32 = 26_500;
const CLIENT_SETTINGS: i32 = TOGGLE_FULLSCREEN + 1;
const EXIT: i32 = TOGGLE_FULLSCREEN + 2;

fn items(restricted: bool) -> Vec<MenuItem> {
    [
        (TOGGLE_FULLSCREEN, "Toggle Fullscreen"),
        (CLIENT_SETTINGS, "Settings"),
        (EXIT, "Exit"),
    ]
    .into_iter()
    .filter(|(id, _)| !restricted || *id != CLIENT_SETTINGS)
    .map(|(id, label)| MenuItem {
        id,
        label: label.to_owned(),
        enabled: true,
        separator: false,
    })
    .collect()
}

fn dispatch(id: i32) -> bool {
    match id {
        TOGGLE_FULLSCREEN => {
            if let Some(platform) = jfn_platform_abi::try_lease() {
                platform.toggle_fullscreen();
            }
        }
        CLIENT_SETTINGS => crate::shell::shell_open_client_settings(),
        EXIT => jfn_playback::shutdown::jfn_shutdown_initiate(),
        _ => return false,
    }
    true
}

fn open_host(point: LogicalPoint, restricted: bool) {
    let Some(platform) = jfn_platform_abi::try_lease() else {
        return;
    };
    let MenuDelivery::Host(host) = platform.menu_delivery(MenuKind::ContextMenu) else {
        return;
    };
    host.open(MenuRequest {
        items: items(restricted),
        x: point.x,
        y: point.y,
        width: 0,
        initial: jfn_platform_abi::MENU_DISMISSED,
        on_selected: MenuSelection::new(|id| {
            dispatch(id);
        }),
    });
}

pub(crate) fn shell_actions() -> crate::shell::ApplicationActions {
    crate::shell::ApplicationActions {
        open_menu: open_host,
    }
}
pub(crate) fn cef_menu() -> jfn_cef::ApplicationMenu {
    jfn_cef::ApplicationMenu {
        items: items(false),
        on_selected: std::sync::Arc::new(dispatch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restricted_menu_keeps_fullscreen_and_exit_but_omits_settings() {
        assert_eq!(
            items(false).iter().map(|item| item.id).collect::<Vec<_>>(),
            [TOGGLE_FULLSCREEN, CLIENT_SETTINGS, EXIT]
        );
        assert_eq!(
            items(true).iter().map(|item| item.id).collect::<Vec<_>>(),
            [TOGGLE_FULLSCREEN, EXIT]
        );
        assert!(!dispatch(jfn_platform_abi::MENU_DISMISSED));
    }
}
