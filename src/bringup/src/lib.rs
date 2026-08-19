//! The whole path from process start to jellyfin-web on screen, in one value.
//!
//! Bring-up owns the URL, the probe that resolves it, the navigation it issues,
//! the pixel witness that ends the connect screen and the shell occupant that
//! shows it. Nothing else in the process holds a piece of that state: the
//! connect screen renders [`screen`], the web overlay runs [`take_requests`],
//! and both answer with an [`Event`].

#![deny(clippy::let_underscore_must_use)]

use std::time::{Duration, Instant};

use parking_lot::Mutex;

pub use jfn_gpu_paint::Presented;

/// The connect screen's spinner shows for at least this long before a failure
/// view replaces it.
///
/// Source: `dev/requirements/the-boot-lifecycle-nobody-owns.md`.
pub const SPINNER_FLOOR: Duration = Duration::from_secs(1);

/// The connect screen's fade-out, once bring-up has its witness.
///
/// Source: `dev/requirements/the-boot-lifecycle-nobody-owns.md`.
pub const FADE: Duration = Duration::from_millis(500);

/// One navigation bring-up issued. A witness naming any other belongs to a
/// different navigation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Navigation(u64);

impl Navigation {
    /// Mints a navigation for a test in another crate.
    ///
    /// Bring-up is the only party that issues navigations, and no normal build
    /// enables `test-support`: a caller that is not a test cannot name one it
    /// did not receive.
    #[cfg(feature = "test-support")]
    pub fn for_test(id: u64) -> Navigation {
        Navigation(id)
    }
}

/// Proof that the web overlay presented a frame produced after `navigation`.
///
/// Built from the commit proof, which only a surface's allocator mints at the
/// commit site, so nothing that merely claims a page loaded can make one.
#[derive(Clone, Copy, Debug)]
pub struct Operational {
    navigation: Navigation,
}

impl Operational {
    /// Consumes the commit proof: only a commit site holds one.
    pub fn witnessed(navigation: Navigation, presented: Presented) -> Operational {
        let _consumed: Presented = presented;
        Operational { navigation }
    }

    pub fn navigation(self) -> Navigation {
        self.navigation
    }
}

/// Everything that reaches bring-up from outside it.
#[derive(Clone, Debug)]
pub enum Event {
    /// The user edited the server field.
    UrlEdited(String),
    /// The user asked to connect to the URL bring-up holds.
    Connect,
    /// Escape reached the connect screen.
    Cancel,
    /// The user dismissed the failure view.
    DismissFailure,
    /// Probe `cycle` resolved the canonical base URL of a Jellyfin server.
    Resolved { cycle: u64, base: String },
    /// Probe `cycle` resolved no Jellyfin server.
    Unresolved { cycle: u64 },
    /// The main frame of `navigation` failed to load.
    NavigationFailed(Navigation),
    /// The web overlay presented a frame produced after its navigation.
    Operational(Operational),
    /// Time advanced to `now`.
    Tick(Instant),
}

/// Work bring-up hands to the CEF side, which executes it and answers with an
/// [`Event`]. Nothing else produces one.
#[derive(Clone, Debug)]
pub enum Request {
    /// Resolve `url` to a canonical base URL and confirm a Jellyfin server
    /// answers there, citing `cycle` in the answer.
    Probe { cycle: u64, url: String },
    /// Load `url` in the web overlay as `navigation`, and stamp every frame
    /// produced afterwards with it.
    Navigate { navigation: Navigation, url: String },
    /// Drop `navigation`: the web overlay stops stamping frames with it and
    /// replaces the document it loaded with a blank one.
    Abandon { navigation: Navigation },
}

/// What bring-up asks the shell overlay to show. The connect screen renders
/// this and holds none of it.
#[derive(Clone, Debug)]
pub enum Screen {
    /// The form, seeded with the URL bring-up knows.
    Form { url: String },
    /// The logo and spinner, up since `since`.
    Working { since: Instant },
    /// The failure view.
    Failed,
    /// The witness arrived; the screen fades from `fade_from`.
    Retiring { fade_from: Instant },
    /// Bring-up is done with the screen.
    Gone,
}

/// The one bring-up. Its states are the whole flow; nothing outside this file
/// holds a piece of it.
enum BringUp {
    /// No probe in flight; the form shows `url`.
    Asking { url: String },
    /// Probe `cycle` is resolving `url`. `unresolved` holds a failure the
    /// spinner floor has not yet let show.
    Probing {
        url: String,
        cycle: u64,
        since: Instant,
        unresolved: bool,
    },
    /// `navigation` is loading and no frame produced after it has been
    /// presented.
    Loading {
        base: String,
        navigation: Navigation,
        since: Instant,
    },
    /// The probe or the navigation failed; `url` is what the form returns to.
    Failed { url: String },
    /// The witness arrived; the connect screen is fading out.
    Retiring {
        navigation: Navigation,
        fade_from: Instant,
    },
    /// jellyfin-web owns the screen.
    Serving { navigation: Navigation },
}

impl BringUp {
    /// The navigation this state names; `None` where it names none.
    fn navigation(&self) -> Option<Navigation> {
        match self {
            BringUp::Loading { navigation, .. }
            | BringUp::Retiring { navigation, .. }
            | BringUp::Serving { navigation } => Some(*navigation),
            BringUp::Asking { .. } | BringUp::Probing { .. } | BringUp::Failed { .. } => None,
        }
    }
}

struct State {
    bringup: BringUp,
    cycles: u64,
    navigations: u64,
    requests: Vec<Request>,
    subscribers: Vec<fn()>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// The process's one bring-up, started at first touch from the saved server
/// URL: a URL already known probes without waiting to be asked again.
fn with<R>(f: impl FnOnce(&mut State) -> R) -> (R, bool) {
    let mut slot = STATE.lock();
    let state = slot.get_or_insert_with(|| {
        let url = jfn_config::server_url();
        let mut state = State {
            bringup: BringUp::Asking { url },
            cycles: 0,
            navigations: 0,
            requests: Vec::new(),
            subscribers: Vec::new(),
        };
        state.connect();
        state
    });
    let before = state.stamp();
    let out = f(state);
    (out, state.stamp() != before)
}

impl State {
    /// Changes when bring-up changed: its state, the navigation that state
    /// names, or the work it has produced.
    fn stamp(&self) -> (u64, u64, usize, u8, u64) {
        let (phase, navigation) = match self.bringup {
            BringUp::Asking { .. } => (0, 0),
            BringUp::Probing {
                unresolved: false, ..
            } => (1, 0),
            BringUp::Probing {
                unresolved: true, ..
            } => (2, 0),
            BringUp::Loading { navigation, .. } => (3, navigation.0),
            BringUp::Failed { .. } => (4, 0),
            BringUp::Retiring { navigation, .. } => (5, navigation.0),
            BringUp::Serving { navigation } => (6, navigation.0),
        };
        (
            self.cycles,
            self.navigations,
            self.requests.len(),
            phase,
            navigation,
        )
    }

    /// Replaces bring-up's state with `next`, retiring what the state it leaves
    /// still owes: a probe in flight has its cycle retired, and a navigation
    /// `next` does not name is abandoned, so no state that shows the form or the
    /// failure view sits over the document that navigation loaded. Every state
    /// change goes through here; nothing else writes `bringup`.
    fn enter(&mut self, next: BringUp) {
        // A new cycle retires the answer the probe left behind still owes.
        if matches!(self.bringup, BringUp::Probing { .. }) {
            self.cycles += 1;
        }
        if let Some(navigation) = self.bringup.navigation()
            && next.navigation() != Some(navigation)
        {
            self.requests.push(Request::Abandon { navigation });
        }
        self.bringup = next;
    }

    /// Starts a probe of the URL bring-up holds. An empty URL leaves the form
    /// where it is: there is nothing to resolve.
    fn connect(&mut self) {
        let url = match &self.bringup {
            BringUp::Asking { url } => url.clone(),
            BringUp::Probing { url, .. } => url.clone(),
            BringUp::Loading { base, .. } => base.clone(),
            BringUp::Failed { url, .. } => url.clone(),
            BringUp::Retiring { .. } | BringUp::Serving { .. } => jfn_config::server_url(),
        };
        if url.trim().is_empty() {
            self.enter(BringUp::Asking { url });
            return;
        }
        self.cycles += 1;
        let cycle = self.cycles;
        self.enter(BringUp::Probing {
            url: url.clone(),
            cycle,
            since: Instant::now(),
            unresolved: false,
        });
        self.requests.push(Request::Probe { cycle, url });
    }

    /// The resolved base URL is saved before the navigation is issued: the URL
    /// the next boot probes is the one this one reached.
    fn navigate(&mut self, base: String) {
        jfn_config::set_server_url(&base);
        jfn_config::settings_save_async();
        self.navigations += 1;
        let navigation = Navigation(self.navigations);
        let since = match self.bringup {
            BringUp::Probing { since, .. } => since,
            _ => Instant::now(),
        };
        self.enter(BringUp::Loading {
            base: base.clone(),
            navigation,
            since,
        });
        self.requests.push(Request::Navigate {
            navigation,
            url: base,
        });
    }

    /// `true` when this transition unlocked the buffered theme colour, which
    /// the caller applies with the lock released.
    fn advance(&mut self, event: Event) -> bool {
        match event {
            Event::UrlEdited(url) => {
                // A navigation has begun; editing the field starts a fresh
                // bring-up rather than editing the one that is serving.
                let editing = matches!(
                    self.bringup,
                    BringUp::Asking { .. }
                        | BringUp::Loading { .. }
                        | BringUp::Retiring { .. }
                        | BringUp::Serving { .. }
                );
                if editing {
                    self.enter(BringUp::Asking { url });
                }
            }
            Event::Connect => self.connect(),
            Event::Cancel => {
                let url = match &self.bringup {
                    BringUp::Probing { url, .. } => Some(url.clone()),
                    BringUp::Loading { base, .. } => Some(base.clone()),
                    BringUp::Asking { .. }
                    | BringUp::Failed { .. }
                    | BringUp::Retiring { .. }
                    | BringUp::Serving { .. } => None,
                };
                if let Some(url) = url {
                    self.enter(BringUp::Asking { url });
                }
            }
            Event::DismissFailure => {
                if let BringUp::Failed { url } = &self.bringup {
                    let url = url.clone();
                    self.enter(BringUp::Asking { url });
                }
            }
            Event::Resolved { cycle, base } => {
                if matches!(self.bringup, BringUp::Probing { cycle: live, .. } if live == cycle) {
                    self.navigate(base);
                }
            }
            Event::Unresolved { cycle } => {
                if let BringUp::Probing {
                    url,
                    cycle: live,
                    since,
                    unresolved,
                } = &mut self.bringup
                    && *live == cycle
                {
                    if Instant::now().saturating_duration_since(*since) < SPINNER_FLOOR {
                        *unresolved = true;
                    } else {
                        let url = url.clone();
                        self.enter(BringUp::Failed { url });
                    }
                }
            }
            Event::NavigationFailed(navigation) => {
                if let BringUp::Loading {
                    base,
                    navigation: live,
                    ..
                } = &self.bringup
                    && *live == navigation
                {
                    let url = base.clone();
                    self.enter(BringUp::Failed { url });
                }
            }
            // `Operational` is honoured from `Loading` alone: an error page that
            // paints leaves the failure view where it is, and the navigation
            // that never becomes operational leaves the connect screen on
            // screen.
            Event::Operational(operational) => {
                let live = match self.bringup {
                    BringUp::Loading { navigation, .. } => Some(navigation),
                    _ => None,
                };
                if live == Some(operational.navigation()) {
                    self.enter(BringUp::Retiring {
                        navigation: operational.navigation(),
                        fade_from: Instant::now(),
                    });
                    return true;
                }
            }
            Event::Tick(now) => self.tick(now),
        }
        false
    }

    fn tick(&mut self, now: Instant) {
        match &self.bringup {
            BringUp::Probing {
                url,
                since,
                unresolved: true,
                ..
            } if now.saturating_duration_since(*since) >= SPINNER_FLOOR => {
                let url = url.clone();
                self.enter(BringUp::Failed { url });
            }
            BringUp::Retiring {
                navigation,
                fade_from,
            } if now.saturating_duration_since(*fade_from) >= FADE => {
                let navigation = *navigation;
                self.enter(BringUp::Serving { navigation });
            }
            _ => {}
        }
    }

    fn screen(&self) -> Screen {
        match &self.bringup {
            BringUp::Asking { url } => Screen::Form { url: url.clone() },
            BringUp::Probing { since, .. } | BringUp::Loading { since, .. } => {
                Screen::Working { since: *since }
            }
            BringUp::Failed { .. } => Screen::Failed,
            BringUp::Retiring { fade_from, .. } => Screen::Retiring {
                fade_from: *fade_from,
            },
            BringUp::Serving { .. } => Screen::Gone,
        }
    }

    fn deadline(&self) -> Option<Instant> {
        match &self.bringup {
            BringUp::Probing {
                since,
                unresolved: true,
                ..
            } => Some(*since + SPINNER_FLOOR),
            BringUp::Retiring { fade_from, .. } => Some(*fade_from + FADE),
            _ => None,
        }
    }
}

/// Advances the process's one bring-up.
///
/// Total over every (state, event) pair. The transitions that are not identity:
/// `Connect` over a non-empty URL starts a probe;
/// `Cancel` returns the form from a probe in flight and from a navigation
/// loading;
/// `Resolved` for the live cycle saves the resolved URL and issues the
/// navigation;
/// `Unresolved` for the live cycle raises the failure view once the spinner
/// floor has elapsed;
/// `NavigationFailed` for the live navigation raises the failure view;
/// `Operational` naming the loading navigation begins the connect screen's
/// fade;
/// `Tick` ends a fade that has run out and promotes a held probe failure;
/// `UrlEdited` and `Connect` after a navigation began start a fresh bring-up
/// with its own navigation and witness.
///
/// Every one of them that stops naming a navigation abandons it, so no later
/// witness or failure naming it reaches the screen and no document it loaded
/// outlives it. The fade and jellyfin-web owning the screen keep the
/// navigation, because both still name it.
pub fn advance(event: Event) {
    let (unlocked, changed) = with(|state| state.advance(event));
    if unlocked {
        jfn_color::theme::jfn_theme_color_on_connect_dismissed();
    }
    if changed {
        notify();
    }
}

/// What the shell overlay shows right now.
pub fn screen() -> Screen {
    with(|state| state.screen()).0
}

/// When bring-up next needs a frame: the end of a fade, or the spinner floor a
/// held probe failure waits out. `None` when it needs none.
pub fn deadline() -> Option<Instant> {
    with(|state| state.deadline()).0
}

/// The requests produced since the last call, oldest first.
pub fn take_requests() -> Vec<Request> {
    with(|state| std::mem::take(&mut state.requests)).0
}

/// Registers `on_change`, called after every advance that changed bring-up and
/// once at registration, so a subscriber that arrives after a transition still
/// sees it.
pub fn subscribe(on_change: fn()) {
    let ((), _changed) = with(|state| state.subscribers.push(on_change));
    on_change();
}

/// Runs every subscriber with the lock released: each one reads bring-up back.
fn notify() {
    let subscribers = STATE
        .lock()
        .as_ref()
        .map(|state| state.subscribers.clone())
        .unwrap_or_default();
    for on_change in subscribers {
        on_change();
    }
}
