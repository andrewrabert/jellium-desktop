//! Registry probe for decoration-related globals.

use std::time::Duration;

use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use wayland_client::Connection;
use wayland_client::globals::registry_queue_init;

#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct DecorationGlobals {
    /// `org_kde_kwin_server_decoration_palette_manager` — SSD can be tinted.
    pub(crate) kde_palette: bool,
}

const KDE_PALETTE_MANAGER: &str = "org_kde_kwin_server_decoration_palette_manager";

struct ProbeState {
    registry_state: RegistryState,
}

impl ProvidesRegistryState for ProbeState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![];
}

delegate_registry!(ProbeState);

fn probe() -> DecorationGlobals {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("WAYLAND_SOCKET").is_none()
    {
        return DecorationGlobals::default();
    }
    let Ok(conn) = Connection::connect_to_env() else {
        return DecorationGlobals::default();
    };
    let Ok((globals, _queue)) = registry_queue_init::<ProbeState>(&conn) else {
        return DecorationGlobals::default();
    };
    DecorationGlobals {
        kde_palette: RegistryState::new(&globals)
            .globals_by_interface(KDE_PALETTE_MANAGER)
            .next()
            .is_some(),
    }
}

/// [`probe`] on a throwaway thread, abandoned on timeout: the round trip
/// blocks indefinitely if the compositor stalls, and this runs inline during
/// startup.
pub(crate) fn probe_bounded(timeout: Duration) -> DecorationGlobals {
    let (tx, rx) = crossbeam_channel::bounded::<DecorationGlobals>(1);
    let spawned = std::thread::Builder::new()
        .name("wl-deco-probe".into())
        .spawn(move || {
            let _ = tx.send(probe());
        })
        .is_ok();
    if !spawned {
        return DecorationGlobals::default();
    }
    rx.recv_timeout(timeout).unwrap_or_default()
}
