//! The crate's owned state root.

use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::Mutex;

use crate::app_conn::AppConn;
use crate::decoration_probe::DecorationGlobals;
use crate::selection::Selections;
use jfn_linux_util::menu::SoftwareMenu;

use crate::input::{InputThread, SeatShared};
use crate::mpv_proxy::ProxyShared;
use crate::paint_override::WlPaintOverride;
use crate::root_window::RootShared;
use crate::window_state::WindowState;
use crate::wl_state::{Callbacks, DmabufRegistry, WlState};

const DECORATION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct WlRuntime {
    paint_request: Option<WlPaintOverride>,
    decorations: OnceLock<DecorationGlobals>,
    window: WindowState,
    core: OnceLock<Mutex<WlState>>,
    buffers: DmabufRegistry,
    callbacks: Callbacks,
    root: RootShared,
    proxy: ProxyShared,
    seat: SeatShared,
    input: OnceLock<InputThread>,
    menu: OnceLock<SoftwareMenu>,
    menu_host: OnceLock<crate::popup::WlMenuHost>,
    selections: Selections,
    app_conn: AppConn,
    #[cfg(feature = "kde-palette")]
    palette: crate::kde_palette::Palette,
}

impl WlRuntime {
    pub(crate) fn install(paint_request: Option<WlPaintOverride>) -> &'static Self {
        Box::leak(Box::new(Self {
            paint_request,
            decorations: OnceLock::new(),
            window: WindowState::new(),
            core: OnceLock::new(),
            buffers: DmabufRegistry::new(),
            callbacks: Callbacks::new(),
            root: RootShared::new(),
            proxy: ProxyShared::new(),
            seat: SeatShared::new(),
            input: OnceLock::new(),
            menu: OnceLock::new(),
            menu_host: OnceLock::new(),
            selections: Selections::new(),
            app_conn: AppConn::new(),
            #[cfg(feature = "kde-palette")]
            palette: crate::kde_palette::Palette::new(),
        }))
    }

    pub(crate) fn window(&self) -> &WindowState {
        &self.window
    }

    pub(crate) fn try_core(&self) -> Option<&Mutex<WlState>> {
        self.core.get()
    }

    pub(crate) fn set_core(&self, state: WlState) -> Result<(), ()> {
        self.core.set(Mutex::new(state)).map_err(|_| ())
    }

    pub(crate) fn buffers(&self) -> &DmabufRegistry {
        &self.buffers
    }

    pub(crate) fn callbacks(&'static self) -> &'static Callbacks {
        &self.callbacks
    }

    pub(crate) fn root(&self) -> &RootShared {
        &self.root
    }

    pub(crate) fn proxy(&self) -> &ProxyShared {
        &self.proxy
    }

    pub(crate) fn seat(&self) -> &SeatShared {
        &self.seat
    }

    pub(crate) fn input(&self) -> Option<&InputThread> {
        self.input.get()
    }

    pub(crate) fn set_input(&self, thread: InputThread) -> Result<(), ()> {
        self.input.set(thread).map_err(|_| ())
    }

    pub(crate) fn menu(&'static self) -> &'static SoftwareMenu {
        self.menu.get_or_init(|| {
            SoftwareMenu::spawn(std::sync::Arc::new(crate::popup::WlPopupSurface {
                rt: self,
            }))
        })
    }

    pub(crate) fn menu_host(&'static self) -> &'static crate::popup::WlMenuHost {
        self.menu_host
            .get_or_init(|| crate::popup::WlMenuHost { rt: self })
    }

    pub(crate) fn try_menu(&self) -> Option<&SoftwareMenu> {
        self.menu.get()
    }

    pub(crate) fn selections(&self) -> &Selections {
        &self.selections
    }

    pub(crate) fn app_conn(&self) -> &AppConn {
        &self.app_conn
    }

    #[cfg(feature = "kde-palette")]
    pub(crate) fn palette(&self) -> &crate::kde_palette::Palette {
        &self.palette
    }

    pub(crate) fn paint_request(&self) -> Option<WlPaintOverride> {
        self.paint_request
    }

    /// Probe for decoration globals. Must run before the mpv proxy rewrites
    /// `WAYLAND_DISPLAY`, or the probe connects to the proxy socket instead of
    /// the real compositor.
    pub(crate) fn probe_decorations(&self) {
        let _ = self.decorations.set(crate::decoration_probe::probe_bounded(
            DECORATION_PROBE_TIMEOUT,
        ));
    }

    /// Probe failure (or a missed [`Self::probe_decorations`]) reads as "no
    /// globals", which resolves to CSD — the only mode that never depends on
    /// the compositor.
    pub(crate) fn decorations(&self) -> DecorationGlobals {
        self.decorations.get().copied().unwrap_or_default()
    }
}
