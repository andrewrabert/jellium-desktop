//! The shell overlay: one app-drawn surface hosting the connect screen, the
//! about panel and the titlebar.
//!
//! It is allocated once at boot, before CEF exists, and freed once at
//! shutdown; between the two only its visibility changes. jellyfin-web is the
//! only CEF layer left in the process.

#![deny(clippy::let_underscore_must_use)]

pub mod about;
pub mod actor;
pub mod chrome;
pub mod connect;
mod controls;
pub mod field;
pub mod fields;
pub mod key;
pub mod lang;
pub mod logo;
pub mod menu;
pub mod modal;
pub mod paint;
pub mod router_sink;
pub mod settings;
pub mod settings_overlay;
pub mod spinner;
pub mod state;
pub mod theme;

use std::sync::OnceLock;

use parking_lot::{Condvar, Mutex};

use actor::{Actor, Channel, Work};
use jfn_platform_abi::{Plane, SurfaceHandle, SurfaceSize, Visibility};

/// Taken at shutdown so the thread can be joined before the surface is freed.
static ACTOR: Mutex<Option<Actor>> = Mutex::new(None);
/// The actor's sender, created before any thread exists, so work posted before
/// the render thread starts is delivered when it does.
static CHANNEL: OnceLock<Channel> = OnceLock::new();
static SURFACE: Mutex<Option<SurfaceHandle>> = Mutex::new(None);
/// Set by the first [`shell_start`]; a later call returns the surface that one
/// produced.
static STARTED: OnceLock<()> = OnceLock::new();
/// Set once the bundled font is in the global font database; [`wait_fonts_ready`]
/// blocks on it.
static FONTS_READY: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// Publishes the routing state of a window with no shell overlay: no modal, no
/// titlebar, no reserved strip, at the window's current logical size.
pub(crate) fn publish_no_overlay() {
    let plat = jfn_platform_abi::get();
    jfn_input::publish_shell_state(crate::state::shell_state(
        plat.window_owner().source().snapshot().extent,
        crate::state::ChromeInputs::default(),
        false,
    ));
}

/// Opens the process's wgpu device, allocates the shell overlay surface,
/// claims it for direct presentation, installs the shell input sink, the about
/// handler and the decorations listener, and spawns the render actor.
///
/// Returns [`SurfaceHandle::NONE`] when the machine has no usable adapter or
/// the render thread could not start; in that case the surface is freed again
/// and [`shell_surface`] stays `NONE`, so nothing restacks or resizes an
/// overlay that has no renderer.
///
/// Must run after `Platform::init` and before `CefInitialize`.
pub fn shell_start() -> SurfaceHandle {
    if STARTED.set(()).is_err() {
        return shell_surface();
    }
    if jfn_gpu_paint::Surfaces::init(None).is_none() {
        tracing::info!("shell: no usable GPU adapter; overlay disabled");
        publish_no_overlay();
        return SurfaceHandle::NONE;
    }

    let plat = jfn_platform_abi::get();
    let surface = plat.alloc_surface(Visibility::Hidden);
    if surface == SurfaceHandle::NONE {
        tracing::error!("shell: overlay surface allocation failed");
        publish_no_overlay();
        return SurfaceHandle::NONE;
    }
    jfn_platform_abi::stack::occupy(Plane::ShellOverlay, surface);
    *SURFACE.lock() = Some(surface);

    // Declares that we present to it ourselves: from here the backend attaches
    // no buffer, grabs no input, and drops every present for this surface.
    let _claimed = plat.surface_window_target(surface);

    let Some(actor) = Actor::spawn(surface, CHANNEL.get_or_init(Channel::new)) else {
        tracing::error!("shell: render actor failed to start");
        *SURFACE.lock() = None;
        jfn_platform_abi::stack::vacate(Plane::ShellOverlay);
        plat.free_surface(surface);
        publish_no_overlay();
        return SurfaceHandle::NONE;
    };
    *ACTOR.lock() = Some(actor);

    jfn_bringup::subscribe(bringup_changed);
    jfn_gpu_paint::refresh::subscribe(refresh_changed);
    jfn_input::install_shell(Box::new(router_sink::ShellSink));
    chrome::set_listener(Box::new(|inputs| post(Work::Chrome(inputs))));
    jfn_playback::chrome::subscribe_chrome(push_playback_chrome);
    jfn_color::theme::jfn_theme_color_subscribe(|rgb| {
        post(Work::ChromeBackground(theme::from_rgb(rgb)));
    });
    jfn_platform_abi::set_decorations_listener(push_decorations);
    jfn_platform_abi::subscribe_window_changed(push_window_state);
    push_decorations();
    push_window_state();
    push_playback_chrome();

    surface
}

/// Joins the render actor — which drops the swapchain — and frees the surface
/// only if that join succeeded, so no swapchain outlives the window it was
/// built on. A render thread still alive at the bound keeps both; the process
/// is exiting either way.
///
/// The [`Actor`] is taken out of its slot under the lock and the lock released
/// before the join, so a [`post`] racing shutdown — a bring-up transition
/// raised on `TID_UI` — queues into the channel instead of blocking for the
/// length of the drain.
pub fn shell_shutdown() {
    let surface = SURFACE.lock().take();
    let actor = ACTOR.lock().take();
    let joined = actor.is_none_or(Actor::join);
    if !joined {
        tracing::warn!("shell: render thread still alive; leaving the overlay surface allocated");
        return;
    }
    if let Some(surface) = surface {
        jfn_platform_abi::stack::vacate(Plane::ShellOverlay);
        jfn_platform_abi::get().free_surface(surface);
    }
}

pub fn shell_surface() -> SurfaceHandle {
    SURFACE.lock().unwrap_or(SurfaceHandle::NONE)
}

/// Loads the bundled font into the process font system on its own thread, so
/// the scan overlaps mpv bring-up instead of gating first paint.
///
/// The handle is joined before `CefInitialize`: fontdb's directory walk must
/// not run while Chromium is manipulating process file descriptors.
///
/// This is the only place in the process that builds the font system before a
/// frame is drawn; the shell overlay's own text resolves through
/// [`theme::FONT`], so no glyph the overlay draws depends on the scan's result.
pub fn shell_warm_fonts() -> std::thread::JoinHandle<()> {
    let warm = || {
        jfn_fonts::warm(FONT);
        signal_fonts_ready();
    };
    match std::thread::Builder::new()
        .name("jfn-shell-fonts".to_owned())
        .spawn(warm)
    {
        Ok(handle) => handle,
        Err(e) => {
            tracing::warn!("shell: font warm-up thread failed: {e}");
            // Nothing will load the font now; releasing the gate keeps the
            // render thread drawing rather than waiting for a load that will
            // never land.
            signal_fonts_ready();
            // A handle that is already done, so the caller's join is a no-op
            // rather than a special case.
            std::thread::spawn(|| {})
        }
    }
}

/// Blocks until [`shell_warm_fonts`] has loaded the bundled font. The render
/// thread calls it once, before its first draw: reaching text shaping first
/// would pay the system scan on the drawing thread, and shaping before the load
/// lands would leave a fallback family in every cached paragraph until the next
/// tree rebuild.
pub fn wait_fonts_ready() {
    let (lock, ready) = &FONTS_READY;
    let mut loaded = lock.lock();
    while !*loaded {
        ready.wait(&mut loaded);
    }
}

fn signal_fonts_ready() {
    let (lock, ready) = &FONTS_READY;
    *lock.lock() = true;
    ready.notify_all();
}

/// Opens the combined overlay on its About tab. Installed as
/// [`jfn_platform_abi::set_about_handler`].
pub fn shell_open_about() {
    post(Work::OpenAbout);
}

/// Opens client settings. Work posted before the render thread starts remains
/// queued in the process-wide shell channel.
pub fn shell_open_client_settings() {
    post(Work::OpenClientSettings);
}

const FONT: &[u8] = include_bytes!("../assets/NotoSans-Regular.ttf");

/// Posts `work` to the render actor. Never drops it: an actor that has not
/// started yet finds it queued in the channel it takes at spawn.
pub(crate) fn post(work: Work) {
    CHANNEL.get_or_init(Channel::new).post(work);
}

/// Subscribed into bring-up at [`shell_start`]; wakes the pass that re-reads
/// its screen.
fn bringup_changed() {
    post(Work::BringUpChanged);
}

/// Subscribed into the refresh report at [`shell_start`]; wakes the pass, which
/// now has a cadence to animate the spinner on.
fn refresh_changed() {
    post(Work::Redraw);
}

fn push_playback_chrome() {
    let state = jfn_playback::chrome::chrome_state();
    chrome::set_video_active(state.video_active);
    chrome::set_osd_visible(state.osd_visible);
}

fn push_decorations() {
    let client_side = matches!(
        jfn_platform_abi::get().effective_decorations(),
        jfn_platform_abi::EffectiveDecorations::ClientSide
    );
    chrome::set_client_side_decorations(client_side);
}

fn push_window_state() {
    let plat = jfn_platform_abi::get();
    let snap = plat.window_owner().source().snapshot();
    chrome::set_fullscreen(snap.fullscreen);
    let Some(extent) = snap.extent else { return };
    post(Work::Resize { extent });
    let surface = shell_surface();
    if surface != SurfaceHandle::NONE {
        // The overlay spans the whole window: the reserved strip is the web
        // layer's inset, not the overlay's.
        plat.surface_resize(
            surface,
            SurfaceSize {
                extent,
                logical_top: 0,
                physical_top: 0,
            },
        );
    }
}
