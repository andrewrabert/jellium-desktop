use cef::{Browser, CefString, ImplBrowser, ImplBrowserHost, ImplFrame};
use std::sync::Arc;

use super::{BLANK, Inner, Painting, PendingNavigation};
use crate::platform_ops;

impl Inner {
    pub(crate) fn create(self: &Arc<Self>, url: &str) -> bool {
        self.session
            .dispatch(|| self.cef_create_browser(url))
            .unwrap_or(false)
    }

    pub(super) fn deliver_deferred_navigation(&self) {
        let Some(browser) = self.browser.lock().browser.clone() else {
            return;
        };
        let Some(frame) = browser.main_frame() else {
            return;
        };

        let mut pending = self.deferred_navigation.pending.lock();
        let Some(load) = pending.first() else {
            return;
        };
        let url = match load {
            PendingNavigation::Page { navigation, url } => {
                *self.painting.lock() = Painting::Awaiting {
                    navigation: *navigation,
                    base: url.clone(),
                };
                url.as_str()
            }
            PendingNavigation::Blank => {
                *self.painting.lock() = Painting::None;
                BLANK
            }
        };
        frame.load_url(Some(&CefString::from(url)));
        pending.clear();
    }

    pub(crate) fn handle_on_after_created(self: &Arc<Self>, browser: Browser) {
        let formatted = format!("CefLayer::OnAfterCreated name={}", self.name_str());
        jfn_logging::log(
            jfn_logging::Category::Cef,
            jfn_logging::Level::Debug,
            &formatted,
        );
        self.browser.lock().browser = Some(browser.clone());
        if !self.session.is_active() {
            if let Some(host) = browser.host() {
                host.close_browser(1);
            }
            return;
        }
        self.paint_scheduler.during_resize(self, || {
            if let Some(host) = browser.host() {
                host.notify_screen_info_changed();
                host.was_resized();
            }
        });

        let created = self.created_callback.lock().clone();
        if let Some(callback) = created {
            callback();
        }

        self.deliver_deferred_navigation();
    }

    pub(crate) fn handle_on_before_close(self: &Arc<Self>) {
        {
            let mut state = self.browser.lock();
            state.browser = None;
            state.applied = None;
        }
        self.paint_scheduler.before_close();
        jfn_logging::log(
            jfn_logging::Category::Cef,
            jfn_logging::Level::Debug,
            &format!("OnBeforeClose name={}", self.name_str()),
        );
        self.menu_reset();
        self.on_deactivated();

        if self.session.is_active() {
            let _ = self.create("");
        }
    }

    pub(crate) fn on_before_popup(&self, url: &str) -> bool {
        if url.is_empty() || url.starts_with('-') {
            return true;
        }
        if let Some(platform) = platform_ops::ops() {
            platform.open_external_url(url);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Weak};

    use crate::client::{BLANK, Painting, PendingNavigation};

    struct LifecycleModel {
        browser: Option<Arc<()>>,
        surface: Arc<()>,
    }

    fn model() -> (Arc<LifecycleModel>, Weak<()>) {
        let surface = Arc::new(());
        let observer = Arc::downgrade(&surface);
        (
            Arc::new(LifecycleModel {
                browser: None,
                surface,
            }),
            observer,
        )
    }

    #[test]
    fn on_after_created_retains_only_the_browser_command_handle() {
        let (mut owner, _surface) = model();
        let browser = Arc::new(());
        let browser_observer = Arc::downgrade(&browser);
        let owner = Arc::get_mut(&mut owner);
        assert!(owner.is_some());
        if let Some(owner) = owner {
            owner.browser = Some(browser);
        }
        assert!(browser_observer.upgrade().is_some());
    }

    #[test]
    fn session_rollback_revokes_browser_creation_without_global_shutdown() {
        let session = crate::runtime::Session::new();
        assert!(session.register_overlay());
        assert_eq!(session.dispatch(|| "create"), Some("create"));
        session.revoke();
        assert!(!session.is_active());
        assert_eq!(session.dispatch(|| "recreate"), None);
    }

    #[test]
    fn on_before_close_breaks_the_browser_client_cycle() {
        let browser = Arc::new(());
        let observer = Arc::downgrade(&browser);
        let mut command = Some(browser);
        assert!(command.is_some());
        command = None;
        assert!(command.is_none());
        assert!(observer.upgrade().is_none());
    }

    #[test]
    fn reset_recreates_from_the_on_before_close_callback_owner() {
        let (owner, _surface) = model();
        let callback_owner = Arc::clone(&owner);
        let recreated_client = Arc::clone(&callback_owner);
        assert!(Arc::ptr_eq(&owner, &recreated_client));
    }

    #[test]
    fn recreation_reuses_the_surface_owner_without_copying_its_handle() {
        let (owner, _surface) = model();
        let callback_owner = Arc::clone(&owner);
        assert!(Arc::ptr_eq(&owner.surface, &callback_owner.surface));
    }

    #[test]
    fn rejected_recreation_releases_after_the_old_on_before_close() {
        let (owner, surface) = model();
        drop(owner);
        assert!(surface.upgrade().is_none());
    }

    #[derive(Debug, Eq, PartialEq)]
    enum DeferredValue {
        Absent,
        Page(crate::Navigation, String),
        Blank,
    }

    fn deferred_value(deferred: &crate::client::DeferredNavigation) -> DeferredValue {
        match deferred.pending.lock().as_slice() {
            [] => DeferredValue::Absent,
            [PendingNavigation::Page { navigation, url }] => {
                DeferredValue::Page(*navigation, url.clone())
            }
            [PendingNavigation::Blank] => DeferredValue::Blank,
            _ => unreachable!("the deferred slot contains at most one load"),
        }
    }

    fn deliver_model(
        deferred: &crate::client::DeferredNavigation,
        has_browser: bool,
        has_main_frame: bool,
        delivered: &mut Vec<DeferredValue>,
    ) {
        if !has_browser || !has_main_frame {
            return;
        }
        let value = deferred_value(deferred);
        if value != DeferredValue::Absent {
            delivered.push(value);
            deferred.pending.lock().clear();
        }
    }

    #[test]
    fn absence_is_distinct_from_empty_and_about_blank_page_urls() {
        let deferred = crate::client::DeferredNavigation::new();
        assert_eq!(deferred_value(&deferred), DeferredValue::Absent);
        deferred.navigate(crate::Navigation::new(1), "");
        assert_eq!(
            deferred_value(&deferred),
            DeferredValue::Page(crate::Navigation::new(1), String::new())
        );
        deferred.navigate(crate::Navigation::new(2), BLANK);
        assert_eq!(
            deferred_value(&deferred),
            DeferredValue::Page(crate::Navigation::new(2), BLANK.to_owned())
        );
    }

    #[test]
    fn newest_deferred_page_replaces_the_older_page() {
        let deferred = crate::client::DeferredNavigation::new();
        deferred.navigate(crate::Navigation::new(1), "old");
        deferred.navigate(crate::Navigation::new(2), "new");
        assert_eq!(
            deferred_value(&deferred),
            DeferredValue::Page(crate::Navigation::new(2), "new".to_owned())
        );
    }

    #[test]
    fn matching_abandon_replaces_a_deferred_page_with_blank() {
        let deferred = crate::client::DeferredNavigation::new();
        let navigation = crate::Navigation::new(1);
        deferred.navigate(navigation, "page");
        deferred.abandon(navigation);
        assert_eq!(deferred_value(&deferred), DeferredValue::Blank);
    }

    #[test]
    fn nonmatching_abandon_preserves_the_effective_deferred_page() {
        let deferred = crate::client::DeferredNavigation::new();
        deferred.navigate(crate::Navigation::new(1), "page");
        deferred.abandon(crate::Navigation::new(2));
        assert_eq!(
            deferred_value(&deferred),
            DeferredValue::Page(crate::Navigation::new(1), "page".to_owned())
        );
    }

    #[test]
    fn browser_without_a_main_frame_retains_the_deferred_load() {
        let deferred = crate::client::DeferredNavigation::new();
        deferred.navigate(crate::Navigation::new(1), "page");
        deliver_model(&deferred, true, false, &mut Vec::new());
        assert_ne!(deferred_value(&deferred), DeferredValue::Absent);
    }

    #[test]
    fn real_frame_delivery_consumes_the_deferred_load_once() {
        let deferred = crate::client::DeferredNavigation::new();
        deferred.navigate(crate::Navigation::new(1), "page");
        let mut delivered = Vec::new();
        deliver_model(&deferred, true, true, &mut delivered);
        deliver_model(&deferred, true, true, &mut delivered);
        assert_eq!(delivered.len(), 1);
        assert_eq!(deferred_value(&deferred), DeferredValue::Absent);
    }

    #[test]
    fn abandon_clears_frame_and_failure_identity_before_blank_delivery() {
        let navigation = crate::Navigation::new(1);
        let mut painting = Painting::Awaiting {
            navigation,
            base: "page".to_owned(),
        };
        let deferred = crate::client::DeferredNavigation::new();
        deferred.navigate(navigation, "page");
        deferred.abandon(navigation);
        if painting.names(navigation) {
            painting = Painting::None;
        }
        assert_eq!(painting.witness(), None);
        assert_eq!(painting.navigation_of("page"), None);
        assert_eq!(deferred_value(&deferred), DeferredValue::Blank);
    }

    #[test]
    fn navigation_deferred_before_client_creation_reaches_the_created_browser() {
        let deferred = crate::client::DeferredNavigation::new();
        deferred.navigate(crate::Navigation::new(1), "page");
        let client_slot = Arc::clone(&deferred);
        let mut delivered = Vec::new();
        deliver_model(&client_slot, true, true, &mut delivered);
        assert_eq!(
            delivered,
            vec![DeferredValue::Page(
                crate::Navigation::new(1),
                "page".to_owned()
            )]
        );
    }
}
