//! App connection state, owned and updated by the shell actor.
use jfn_cef::Navigation;
use std::time::{Duration, Instant};
pub(crate) const SPINNER_FLOOR: Duration = Duration::from_secs(1);
pub(crate) const FADE: Duration = Duration::from_millis(500);
#[derive(Clone, Debug)]
enum Request {
    CancelProbe,
    /// Resolve `url` to a canonical base URL and confirm a Jellyfin server
    /// answers there, citing `cycle` in the answer.
    Probe {
        cycle: u64,
        url: String,
    },
    /// Load `url` in the web overlay as `navigation`, and stamp every frame
    /// produced afterwards with it.
    Navigate {
        navigation: Navigation,
        url: String,
    },
    /// Drop `navigation`: the web overlay stops stamping frames with it and
    /// replaces the document it loaded with a blank one.
    Abandon {
        navigation: Navigation,
    },
}

#[derive(Clone, Debug)]
pub enum Screen {
    /// The form, seeded with the URL connection knows.
    Form { url: String },
    /// The logo and spinner, up since `since`.
    Working { since: Instant },
    /// The failure view.
    Failed,
    /// The witness arrived; the screen fades from `fade_from`.
    Retiring { fade_from: Instant },
    /// The connection screen is no longer needed.
    Gone,
}

/// Connection phases and the data required by each phase.
enum Phase {
    /// No probe in flight; the form shows `url`.
    Editing { url: String },
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
    Connected { navigation: Navigation },
}

impl Phase {
    /// The navigation this state names; `None` where it names none.
    fn navigation(&self) -> Option<Navigation> {
        match self {
            Phase::Loading { navigation, .. }
            | Phase::Retiring { navigation, .. }
            | Phase::Connected { navigation } => Some(*navigation),
            Phase::Editing { .. } | Phase::Probing { .. } | Phase::Failed { .. } => None,
        }
    }
}

pub(crate) struct Connection {
    phase: Phase,
    cycles: u64,
    navigations: u64,
    requests: Vec<Request>,
    overlay: Option<jfn_cef::WebOverlay>,
}

impl Connection {
    pub(crate) fn new(url: String) -> Self {
        let mut connection = Self {
            phase: Phase::Editing { url },
            cycles: 0,
            navigations: 0,
            requests: Vec::new(),
            overlay: None,
        };
        connection.connect();
        connection
    }

    pub(crate) fn attach(&mut self, overlay: jfn_cef::WebOverlay) {
        self.overlay = Some(overlay);
        self.flush_requests();
    }

    pub(crate) fn flush_requests(&mut self) {
        let Some(overlay) = self.overlay.clone() else {
            return;
        };
        for request in std::mem::take(&mut self.requests) {
            let delivered = match request {
                Request::Probe { cycle, url } => {
                    let delivered = overlay.probe(cycle, &url);
                    if delivered.is_err() {
                        self.probe_failed(cycle);
                    }
                    delivered
                }
                Request::Navigate { navigation, url } => {
                    let delivered = overlay.navigate(navigation, &url);
                    if delivered.is_err() {
                        self.navigation_failed(navigation);
                    }
                    delivered
                }
                Request::Abandon { navigation } => overlay.abandon(navigation),
                Request::CancelProbe => overlay.cancel_probe(),
            };
            if let Err(error) = delivered {
                tracing::error!("CEF connection operation failed: {error}");
                jfn_playback::jfn_shutdown_initiate();
                break;
            }
        }
    }

    /// Cancel work belonging to the old phase before starting the next one.
    fn enter(&mut self, next: Phase) {
        // Invalidate results already queued by the canceled probe.
        if matches!(self.phase, Phase::Probing { .. }) {
            self.cycles += 1;
            self.requests.push(Request::CancelProbe);
        }
        if let Some(navigation) = self.phase.navigation()
            && next.navigation() != Some(navigation)
        {
            self.requests.push(Request::Abandon { navigation });
        }
        self.phase = next;
    }

    /// Starts a probe of the URL connection holds. An empty URL leaves the form
    /// where it is: there is nothing to resolve.
    pub(crate) fn connect(&mut self) {
        let url = match &self.phase {
            Phase::Editing { url } => url.clone(),
            Phase::Probing { url, .. } => url.clone(),
            Phase::Loading { base, .. } => base.clone(),
            Phase::Failed { url, .. } => url.clone(),
            Phase::Retiring { .. } | Phase::Connected { .. } => jfn_config::server_url(),
        };
        if url.trim().is_empty() {
            self.enter(Phase::Editing { url });
            return;
        }
        self.cycles += 1;
        let cycle = self.cycles;
        self.enter(Phase::Probing {
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
        let navigation = Navigation::new(self.navigations);
        let since = match self.phase {
            Phase::Probing { since, .. } => since,
            _ => Instant::now(),
        };
        self.enter(Phase::Loading {
            base: base.clone(),
            navigation,
            since,
        });
        self.requests.push(Request::Navigate {
            navigation,
            url: base,
        });
    }

    pub(crate) fn edit_url(&mut self, url: String) {
        // A navigation has begun; editing the field starts a fresh
        // connection rather than editing the one that is serving.
        let editing = matches!(
            self.phase,
            Phase::Editing { .. }
                | Phase::Loading { .. }
                | Phase::Retiring { .. }
                | Phase::Connected { .. }
        );
        if editing {
            self.enter(Phase::Editing { url });
        }
    }
    pub(crate) fn cancel(&mut self) {
        let url = match &self.phase {
            Phase::Probing { url, .. } => Some(url.clone()),
            Phase::Loading { base, .. } => Some(base.clone()),
            Phase::Editing { .. }
            | Phase::Failed { .. }
            | Phase::Retiring { .. }
            | Phase::Connected { .. } => None,
        };
        if let Some(url) = url {
            self.enter(Phase::Editing { url });
        }
    }
    pub(crate) fn dismiss_failure(&mut self) {
        if let Phase::Failed { url } = &self.phase {
            let url = url.clone();
            self.enter(Phase::Editing { url });
        }
    }
    pub(crate) fn probe_resolved(&mut self, cycle: u64, base: String) {
        if matches!(self.phase, Phase::Probing { cycle: live, .. } if live == cycle) {
            self.navigate(base);
        }
    }
    pub(crate) fn probe_failed(&mut self, cycle: u64) {
        if let Phase::Probing {
            url,
            cycle: live,
            since,
            unresolved,
        } = &mut self.phase
            && *live == cycle
        {
            if Instant::now().saturating_duration_since(*since) < SPINNER_FLOOR {
                *unresolved = true;
            } else {
                let url = url.clone();
                self.enter(Phase::Failed { url });
            }
        }
    }
    pub(crate) fn navigation_failed(&mut self, navigation: Navigation) {
        if let Phase::Loading {
            base,
            navigation: live,
            ..
        } = &self.phase
            && *live == navigation
        {
            let url = base.clone();
            self.enter(Phase::Failed { url });
        }
    }
    pub(crate) fn frame_presented(&mut self, presented: jfn_cef::NavigationPresented) {
        if self.finish_navigation(presented.navigation(), Instant::now()) {
            jfn_color::theme::jfn_theme_color_on_connect_dismissed();
        }
    }

    fn finish_navigation(&mut self, navigation: Navigation, now: Instant) -> bool {
        if matches!(self.phase, Phase::Loading { navigation: live, .. } if live == navigation) {
            self.enter(Phase::Retiring {
                navigation,
                fade_from: now,
            });
            true
        } else {
            false
        }
    }

    pub(crate) fn tick(&mut self, now: Instant) {
        match &self.phase {
            Phase::Probing {
                url,
                since,
                unresolved: true,
                ..
            } if now.saturating_duration_since(*since) >= SPINNER_FLOOR => {
                let url = url.clone();
                self.enter(Phase::Failed { url });
            }
            Phase::Retiring {
                navigation,
                fade_from,
            } if now.saturating_duration_since(*fade_from) >= FADE => {
                let navigation = *navigation;
                self.enter(Phase::Connected { navigation });
            }
            _ => {}
        }
    }

    pub(crate) fn screen(&self) -> Screen {
        match &self.phase {
            Phase::Editing { url } => Screen::Form { url: url.clone() },
            Phase::Probing { since, .. } | Phase::Loading { since, .. } => {
                Screen::Working { since: *since }
            }
            Phase::Failed { .. } => Screen::Failed,
            Phase::Retiring { fade_from, .. } => Screen::Retiring {
                fade_from: *fade_from,
            },
            Phase::Connected { .. } => Screen::Gone,
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        match &self.phase {
            Phase::Probing {
                since,
                unresolved: true,
                ..
            } => Some(*since + SPINNER_FLOOR),
            Phase::Retiring { fade_from, .. } => Some(*fade_from + FADE),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probing() -> (Connection, u64, Instant) {
        let connection = Connection::new("https://server/jellyfin".into());
        let Phase::Probing { cycle, since, .. } = connection.phase else {
            unreachable!()
        };
        (connection, cycle, since)
    }

    fn loading(navigation: Navigation) -> Connection {
        let mut connection = Connection::new(String::new());
        connection.phase = Phase::Loading {
            base: "https://server/jellyfin".into(),
            navigation,
            since: Instant::now(),
        };
        connection
    }

    #[test]
    fn startup_retains_work_until_an_overlay_is_available() {
        let (mut connection, cycle, _) = probing();
        connection.flush_requests();
        assert!(
            matches!(connection.requests.as_slice(), [Request::Probe { cycle: id, .. }] if *id == cycle)
        );
        connection.cancel();
        assert!(matches!(connection.screen(), Screen::Form { .. }));
        assert!(matches!(
            connection.requests.as_slice(),
            [Request::Probe { .. }, Request::CancelProbe]
        ));
        connection.probe_resolved(cycle, "https://stale".into());
        connection.probe_failed(cycle);
        assert!(matches!(connection.screen(), Screen::Form { .. }));
    }

    #[test]
    fn retry_ignores_the_previous_probes_result() {
        let (mut connection, old, _) = probing();
        connection.cancel();
        connection.connect();
        let Phase::Probing { cycle, .. } = connection.phase else {
            unreachable!()
        };
        assert_ne!(old, cycle);
        connection.probe_resolved(old, "https://stale".into());
        connection.probe_failed(old);
        assert!(matches!(
            connection.phase,
            Phase::Probing {
                unresolved: false,
                ..
            }
        ));
        assert!(connection.deadline().is_none());
    }

    #[test]
    fn probe_failure_waits_for_spinner_floor_then_restores_the_url() {
        let (mut connection, cycle, since) = probing();
        connection.probe_failed(cycle);
        assert!(matches!(connection.screen(), Screen::Working { .. }));
        assert_eq!(connection.deadline(), Some(since + SPINNER_FLOOR));
        connection.tick(since + SPINNER_FLOOR);
        assert!(matches!(connection.screen(), Screen::Failed));
        connection.dismiss_failure();
        assert!(
            matches!(connection.screen(), Screen::Form { url } if url == "https://server/jellyfin")
        );
    }

    #[test]
    fn cancel_and_failure_abandon_the_document_and_ignore_late_frames() {
        for cancel in [true, false] {
            let id = Navigation::new(1);
            let mut connection = loading(id);
            connection.navigation_failed(Navigation::new(2));
            assert!(matches!(connection.phase, Phase::Loading { .. }));
            if cancel {
                connection.cancel();
            } else {
                connection.navigation_failed(id);
            }
            assert!(
                matches!(connection.requests.as_slice(), [Request::Abandon { navigation }] if *navigation == id)
            );
            assert!(!connection.finish_navigation(id, Instant::now()));
            assert!(matches!(
                connection.screen(),
                Screen::Form { .. } | Screen::Failed
            ));
        }
    }

    #[test]
    fn only_the_current_navigation_can_start_the_fade() {
        let id = Navigation::new(1);
        let mut connection = loading(id);
        let now = Instant::now();
        assert!(!connection.finish_navigation(Navigation::new(2), now));
        assert!(matches!(connection.screen(), Screen::Working { .. }));
        assert!(connection.finish_navigation(id, now));
        assert!(!connection.finish_navigation(id, now + Duration::from_millis(1)));
        assert_eq!(connection.deadline(), Some(now + FADE));
        connection.tick(now + FADE - Duration::from_millis(1));
        assert!(matches!(connection.screen(), Screen::Retiring { .. }));
        connection.tick(now + FADE);
        assert!(matches!(connection.screen(), Screen::Gone));
        assert!(connection.requests.is_empty());
    }
}
