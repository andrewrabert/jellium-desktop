//! The shell overlay's modal stack, and the total function that advances it.

use std::time::Instant;

use iced_core::widget::Id;
use iced_core::{Color, Element};

use jfn_bringup::Screen;

use crate::about::About;
use crate::actor::Deadline;
use crate::connect::Connect;
use crate::theme::Theme;

/// The shell overlay's modal views, bottom first. The top is drawn.
pub struct Stack {
    views: Vec<View>,
}

/// One modal view.
pub enum View {
    Connect(Connect),
    About(About),
}

/// Everything that can change the stack.
#[derive(Clone, Debug)]
pub enum Transition {
    /// The context menu asked for the about panel.
    OpenAbout,
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
    About(crate::about::Message),
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

    fn top(&self) -> Option<&View> {
        self.views.last()
    }

    fn connect_index(&self) -> Option<usize> {
        self.views
            .iter()
            .position(|view| matches!(view, View::Connect(_)))
    }

    /// Total over every (stack, transition) pair.
    ///
    /// `OpenAbout` pushes the panel over whatever is showing, and is identity
    /// when the panel is already the top. `Escape` pops the top, except over
    /// the connect screen, where it asks bring-up to cancel and pops nothing.
    /// `Message` reaches the top view alone, and a connect message becomes a
    /// bring-up event.
    pub fn advance(&mut self, transition: Transition) {
        match transition {
            Transition::OpenAbout => {
                if !matches!(self.top(), Some(View::About(_))) {
                    self.views.push(View::About(About::new()));
                }
            }
            Transition::Escape => match self.top() {
                Some(View::Connect(_)) => jfn_bringup::advance(jfn_bringup::Event::Cancel),
                Some(View::About(_)) => {
                    self.views.pop();
                }
                None => {}
            },
            Transition::Message(message) => self.deliver(message),
            Transition::Tick(now) => jfn_bringup::advance(jfn_bringup::Event::Tick(now)),
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
            (Some(View::About(about)), Message::About(m)) => match m {
                crate::about::Message::Dismiss => {
                    self.views.pop();
                }
                crate::about::Message::Swallow => {}
                crate::about::Message::OpenPath(path) => about.open(&path),
            },
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
            Some(View::About(about)) => about.view().map(Message::About),
            None => iced_widget::space::horizontal().into(),
        }
    }

    /// The top view's backdrop; transparent when the stack is empty, so
    /// jellyfin-web shows through everywhere no widget draws.
    pub fn backdrop(&self, chrome: Color, screen: &Screen) -> Color {
        match self.top() {
            Some(View::Connect(connect)) => connect.backdrop(chrome, screen),
            Some(View::About(about)) => about.backdrop(),
            None => Color::TRANSPARENT,
        }
    }

    /// Every view's deadline, not just the top one's: a connect screen under
    /// the about panel keeps advancing while the panel covers it.
    pub fn deadline(&self, screen: &Screen) -> Deadline {
        self.views
            .iter()
            .fold(Deadline::none(), |deadline, view| match view {
                View::Connect(connect) => deadline.merge(connect.deadline(screen)),
                View::About(_) => deadline,
            })
    }

    pub fn focus_target(&self, screen: &Screen) -> Option<Id> {
        match self.top() {
            Some(View::Connect(connect)) => connect.focus_target(screen),
            Some(View::About(_)) | None => None,
        }
    }
}
