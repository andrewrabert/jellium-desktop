//! The shell overlay's modal stack, and the total function that advances it.

use std::time::Instant;

use iced_core::widget::Id;
use iced_core::{Color, Element};

use jfn_bringup::Screen;

use crate::actor::Deadline;
use crate::connect::Connect;
use crate::settings_overlay::{Outcome as OverlayOutcome, SettingsOverlay, Tab};
use crate::theme::Theme;

/// The shell overlay's modal views, bottom first. The top is drawn.
pub struct Stack {
    views: Vec<View>,
}

/// Stable identity of the top modal, independent of its active tab and initial
/// focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Identity {
    Connect,
    SettingsOverlay,
}

/// One modal view.
pub enum View {
    Connect(Connect),
    SettingsOverlay(Box<SettingsOverlay>),
}

/// Everything that can change the stack.
#[derive(Clone, Debug)]
pub enum Transition {
    /// The native macOS About command selected the About tab.
    OpenAbout,
    /// A shared app menu or the web UI asked for client settings.
    OpenClientSettings,
    /// Escape reached the stack unhandled.
    Escape,
    /// A message the top view published.
    Message(Message),
    /// Time advanced to `now`.
    Tick(Instant),
}

/// A message a modal view publishes.
#[derive(Clone, Debug)]
pub enum Message {
    Connect(crate::connect::Message),
    SettingsOverlay(crate::settings_overlay::Message),
}

impl Default for Stack {
    fn default() -> Self {
        Self::empty()
    }
}

impl Stack {
    pub fn empty() -> Stack {
        Stack { views: Vec::new() }
    }

    pub fn occupied(&self) -> bool {
        !self.views.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn testing_settings() -> Stack {
        Stack {
            views: vec![View::SettingsOverlay(Box::new(SettingsOverlay::testing(
                Tab::Settings,
            )))],
        }
    }

    fn top(&self) -> Option<&View> {
        self.views.last()
    }

    fn connect_index(&self) -> Option<usize> {
        self.views
            .iter()
            .position(|view| matches!(view, View::Connect(_)))
    }

    fn overlay_mut(&mut self) -> Option<&mut SettingsOverlay> {
        self.views.iter_mut().find_map(|view| match view {
            View::SettingsOverlay(overlay) => Some(overlay.as_mut()),
            View::Connect(_) => None,
        })
    }

    pub fn settings_overlay_mut(&mut self) -> Option<&mut SettingsOverlay> {
        self.overlay_mut()
    }

    pub fn active_settings_tab(&self) -> Option<Tab> {
        self.views.iter().find_map(|view| match view {
            View::SettingsOverlay(overlay) => Some(overlay.active()),
            View::Connect(_) => None,
        })
    }

    /// Total over every (stack, transition) pair.
    pub fn advance(&mut self, transition: Transition) {
        match transition {
            Transition::OpenAbout => self.open_overlay(Tab::About),
            Transition::OpenClientSettings => self.open_overlay(Tab::Settings),
            Transition::Escape => match self.top() {
                Some(View::Connect(_)) => jfn_bringup::advance(jfn_bringup::Event::Cancel),
                Some(View::SettingsOverlay(_)) => {
                    if let Some(View::SettingsOverlay(overlay)) = self.views.last_mut() {
                        overlay.dismiss();
                    }
                    self.views.pop();
                }
                None => {}
            },
            Transition::Message(message) => self.deliver(message),
            Transition::Tick(now) => jfn_bringup::advance(jfn_bringup::Event::Tick(now)),
        }
    }

    fn open_overlay(&mut self, tab: Tab) {
        if let Some(overlay) = self.overlay_mut() {
            overlay.select(tab);
        } else {
            #[cfg(test)]
            let overlay = SettingsOverlay::testing(tab);
            #[cfg(not(test))]
            let overlay = SettingsOverlay::new(tab);
            self.views.push(View::SettingsOverlay(Box::new(overlay)));
        }
    }

    /// The top view alone sees a message; one addressed to a view beneath it is
    /// dropped rather than acted on behind the one that has the screen.
    fn deliver(&mut self, message: Message) {
        match (self.views.last_mut(), message) {
            (Some(View::Connect(_)), Message::Connect(m)) => {
                jfn_bringup::advance(match m {
                    crate::connect::Message::UrlEdited(url) => jfn_bringup::Event::UrlEdited(url),
                    crate::connect::Message::Submit => jfn_bringup::Event::Connect,
                    crate::connect::Message::DismissFailure => jfn_bringup::Event::DismissFailure,
                });
            }
            (Some(View::SettingsOverlay(overlay)), Message::SettingsOverlay(message)) => {
                match overlay.update(message) {
                    OverlayOutcome::None => {}
                    OverlayOutcome::Dismiss => {
                        self.views.pop();
                    }
                    OverlayOutcome::ResetSavedServer => {
                        jfn_bringup::advance(jfn_bringup::Event::UrlEdited(String::new()));
                        self.views.pop();
                    }
                }
            }
            _ => {}
        }
    }

    /// Places the connect screen at the bottom while `screen` shows one, and
    /// removes it from wherever it sits once `screen` is [`Screen::Gone`]. The
    /// connect screen enters and leaves the stack here and nowhere else.
    pub fn reconcile(&mut self, screen: &Screen) {
        match (screen, self.connect_index()) {
            (Screen::Gone, Some(at)) => {
                self.views.remove(at);
            }
            (Screen::Gone, None) => {}
            (_, Some(_)) => {}
            (_, None) => self.views.insert(0, View::Connect(Connect::new())),
        }
    }

    pub fn view<'a>(
        &'a self,
        screen: &'a Screen,
    ) -> Element<'a, Message, Theme, iced_wgpu::Renderer> {
        match self.top() {
            Some(View::Connect(connect)) => connect.view(screen).map(Message::Connect),
            Some(View::SettingsOverlay(overlay)) => overlay.view().map(Message::SettingsOverlay),
            None => iced_widget::space::horizontal().into(),
        }
    }

    /// The top view's backdrop; transparent when the stack is empty, so
    /// jellyfin-web shows through everywhere no widget draws.
    pub fn backdrop(&self, chrome: Color, screen: &Screen) -> Color {
        match self.top() {
            Some(View::Connect(connect)) => connect.backdrop(chrome, screen),
            Some(View::SettingsOverlay(overlay)) => overlay.backdrop(),
            None => Color::TRANSPARENT,
        }
    }

    /// Every view's deadline, not just the top one's.
    pub fn deadline(&self, screen: &Screen) -> Deadline {
        self.views
            .iter()
            .fold(Deadline::none(), |deadline, view| match view {
                View::Connect(connect) => deadline.merge(connect.deadline(screen)),
                View::SettingsOverlay(_) => deadline,
            })
    }

    /// The stable identity of the currently rendered top modal.
    pub fn identity(&self) -> Option<Identity> {
        match self.top() {
            Some(View::Connect(_)) => Some(Identity::Connect),
            Some(View::SettingsOverlay(_)) => Some(Identity::SettingsOverlay),
            None => None,
        }
    }

    /// The focus target to use only when the top modal identity changes.
    pub fn initial_focus(&self, screen: &Screen) -> Option<Id> {
        match self.top() {
            Some(View::Connect(connect)) => connect.focus_target(screen),
            Some(View::SettingsOverlay(overlay)) => overlay.focus_target(),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Identity, Message, Stack, Transition, View};
    use crate::settings_overlay::{Message as OverlayMessage, SettingsOverlay, Tab};

    #[test]
    fn each_opener_selects_its_tab() {
        let mut stack = Stack::empty();
        stack.advance(Transition::OpenClientSettings);
        assert_eq!(stack.active_settings_tab(), Some(Tab::Settings));
        assert_eq!(stack.identity(), Some(Identity::SettingsOverlay));

        let mut stack = Stack::empty();
        stack.advance(Transition::OpenAbout);
        assert_eq!(stack.active_settings_tab(), Some(Tab::About));
        assert_eq!(stack.identity(), Some(Identity::SettingsOverlay));
    }

    #[test]
    fn retargeting_keeps_the_owned_settings() {
        let mut stack = Stack {
            views: vec![View::SettingsOverlay(Box::new(SettingsOverlay::testing(
                Tab::Settings,
            )))],
        };
        stack
            .settings_overlay_mut()
            .expect("overlay")
            .settings_mut()
            .audio_passthrough = "draft".to_owned();

        stack.advance(Transition::OpenAbout);
        stack.advance(Transition::OpenClientSettings);

        let overlay = stack.settings_overlay_mut().expect("same overlay");
        assert_eq!(overlay.active(), Tab::Settings);
        assert_eq!(overlay.settings().audio_passthrough, "draft");
    }

    #[test]
    fn escape_closes_the_single_overlay() {
        let mut stack = Stack {
            views: vec![View::SettingsOverlay(Box::new(SettingsOverlay::testing(
                Tab::About,
            )))],
        };
        stack.advance(Transition::Escape);
        assert!(!stack.occupied());
    }

    #[test]
    fn backdrop_and_x_dismiss_messages_close_the_single_overlay() {
        for tab in [Tab::Settings, Tab::About] {
            let mut stack = Stack {
                views: vec![View::SettingsOverlay(Box::new(SettingsOverlay::testing(
                    tab,
                )))],
            };
            stack.advance(Transition::Message(Message::SettingsOverlay(
                OverlayMessage::Dismiss,
            )));
            assert!(!stack.occupied());
        }
    }
}
