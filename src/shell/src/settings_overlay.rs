//! Persistent tabbed Settings/About overlay.

use iced_core::widget::Id;
use iced_core::widget::operation::scrollable::AbsoluteOffset;
use iced_core::{Alignment, Color, Element, Length, Padding};
use iced_widget::{Container, button, column, container, mouse_area, row, text};

use crate::about::About;
use crate::controls;
use crate::settings::{self, Settings};
use crate::theme::{self, Theme};

pub const SETTINGS_TAB_CONTROL: Id = Id::new("shell-settings-tab");
pub const ABOUT_TAB_CONTROL: Id = Id::new("shell-about-tab");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Settings,
    About,
}

#[derive(Clone, Debug)]
pub enum Message {
    Dismiss,
    Swallow,
    Select(Tab),
    Settings(settings::Message),
    About(crate::about::Message),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    None,
    Dismiss,
    ResetSavedServer,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Restoration {
    pub focus: Option<Id>,
    pub scroll: AbsoluteOffset,
}

pub struct SettingsOverlay {
    settings: Settings,
    about: About,
    active: Tab,
    settings_focus: Option<Id>,
    settings_scroll: AbsoluteOffset,
    restore_settings: bool,
}

impl SettingsOverlay {
    pub fn new(active: Tab) -> Self {
        Self {
            settings: Settings::new(),
            about: About::new(),
            active,
            settings_focus: None,
            settings_scroll: AbsoluteOffset::default(),
            restore_settings: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn testing(active: Tab) -> Self {
        Self {
            settings: Settings::testing(),
            about: About::new(),
            active,
            settings_focus: None,
            settings_scroll: AbsoluteOffset::default(),
            restore_settings: false,
        }
    }

    pub fn active(&self) -> Tab {
        self.active
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    pub fn retain_settings_state(&mut self, focus: Option<Id>, scroll: AbsoluteOffset) {
        self.settings_focus = focus;
        self.settings_scroll = scroll;
    }

    pub fn select(&mut self, tab: Tab) {
        if self.active != tab {
            self.active = tab;
            self.restore_settings = tab == Tab::Settings;
        }
    }

    pub fn restoration(&self) -> Restoration {
        Restoration {
            focus: self.settings_focus.clone(),
            scroll: self.settings_scroll,
        }
    }

    pub fn take_restoration(&mut self) -> Option<Restoration> {
        self.restore_settings.then(|| {
            self.restore_settings = false;
            self.restoration()
        })
    }

    pub fn dismiss(&mut self) -> Outcome {
        let _ = self.settings.dismiss();
        Outcome::Dismiss
    }

    pub fn update(&mut self, message: Message) -> Outcome {
        match message {
            Message::Dismiss => self.dismiss(),
            Message::Swallow => Outcome::None,
            Message::Select(tab) => {
                self.select(tab);
                Outcome::None
            }
            Message::Settings(message) => match self.settings.update(message) {
                settings::Outcome::None => Outcome::None,
                settings::Outcome::Dismiss => Outcome::Dismiss,
                settings::Outcome::ResetSavedServer => Outcome::ResetSavedServer,
            },
            Message::About(crate::about::Message::OpenPath(path)) => {
                self.about.open(&path);
                Outcome::None
            }
        }
    }

    pub fn view(&self) -> Element<'_, Message, Theme, iced_wgpu::Renderer> {
        let settings_tab = controls::tab(
            SETTINGS_TAB_CONTROL,
            button(text(settings::TITLE))
                .on_press(Message::Select(Tab::Settings))
                .class(tab_class(self.active == Tab::Settings)),
            Message::Select(Tab::Settings),
        );
        let about_tab = controls::tab(
            ABOUT_TAB_CONTROL,
            button(text("About"))
                .on_press(Message::Select(Tab::About))
                .class(tab_class(self.active == Tab::About)),
            Message::Select(Tab::About),
        );
        let close = controls::action(
            settings::CLOSE_CONTROL,
            button(text("\u{00d7}").size(18))
                .on_press(Message::Dismiss)
                .class(theme::ButtonClass::Chrome),
            Message::Dismiss,
        );
        let header = row![
            settings_tab,
            about_tab,
            iced_widget::space::horizontal(),
            close
        ]
        .align_y(Alignment::Center)
        .spacing(12);
        let content = match self.active {
            Tab::Settings => self.settings.view().map(Message::Settings),
            Tab::About => self.about.view().map(Message::About),
        };
        let body = column![header, content].spacing(18);
        let card = mouse_area(card(body)).on_press(Message::Swallow);

        mouse_area(
            container(card)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(24)
                .align_x(Alignment::Center)
                .align_y(Alignment::Center)
                .class(theme::ContainerClass::Backdrop),
        )
        .on_press(Message::Dismiss)
        .into()
    }

    pub fn backdrop(&self) -> Color {
        Color {
            a: 0.5,
            ..Color::BLACK
        }
    }

    pub fn focus_target(&self) -> Option<Id> {
        match self.active {
            Tab::Settings => self.settings.focus_target(),
            Tab::About => Some(ABOUT_TAB_CONTROL),
        }
    }
}

fn card<'a, Message: 'a, Renderer>(
    body: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> Container<'a, Message, Theme, Renderer>
where
    Renderer: iced_core::Renderer,
{
    container(body)
        .width(Length::Fixed(680.0))
        .height(Length::FillPortion(9))
        .padding(Padding::from([18, 24]))
        .clip(true)
        .class(theme::ContainerClass::Card)
}

fn tab_class(selected: bool) -> theme::ButtonClass {
    if selected {
        theme::ButtonClass::TabSelected
    } else {
        theme::ButtonClass::Tab
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_core::Widget;
    use iced_core::layout::Limits;
    use iced_core::widget::Tree;
    use iced_widget::Space;

    fn outer_card_bounds(tab: Tab, viewport: iced_core::Size) -> iced_core::Rectangle {
        let body = match tab {
            Tab::Settings => Space::new().width(Length::Fill).height(Length::Fill),
            Tab::About => Space::new()
                .width(Length::Fixed(1.0))
                .height(Length::Fixed(1.0)),
        };
        let mut card = card::<Message, ()>(body);
        let mut tree = Tree::new(&card as &dyn Widget<Message, Theme, ()>);
        card.layout(
            &mut tree,
            &(),
            &Limits::new(iced_core::Size::ZERO, viewport),
        )
        .bounds()
    }

    #[test]
    fn settings_and_about_have_identical_outer_card_bounds() {
        let viewport = iced_core::Size::new(1280.0, 720.0);

        assert_eq!(
            outer_card_bounds(Tab::Settings, viewport),
            outer_card_bounds(Tab::About, viewport)
        );
    }

    #[test]
    fn tab_switches_keep_the_owned_settings_and_request_exact_restoration() {
        let mut overlay = SettingsOverlay::testing(Tab::Settings);
        overlay.settings_mut().device_name = "  living   room  ".to_owned();
        overlay.settings_mut().audio_passthrough = " ac3,eac3 ".to_owned();
        let focus = Id::new("setting-focus");
        let scroll = AbsoluteOffset { x: 4.0, y: 81.5 };
        overlay.retain_settings_state(Some(focus.clone()), scroll);

        overlay.select(Tab::About);
        overlay.select(Tab::Settings);
        overlay.select(Tab::About);
        overlay.select(Tab::Settings);

        assert_eq!(overlay.settings().device_name, "  living   room  ");
        assert_eq!(overlay.settings().audio_passthrough, " ac3,eac3 ");
        assert_eq!(
            overlay.take_restoration(),
            Some(Restoration {
                focus: Some(focus),
                scroll,
            })
        );
        assert_eq!(overlay.take_restoration(), None);
    }

    #[test]
    fn each_initial_tab_has_its_named_focus_target() {
        assert_eq!(
            SettingsOverlay::testing(Tab::Settings).focus_target(),
            Some(settings::AUDIO_PASSTHROUGH_FIELD)
        );
        assert_eq!(
            SettingsOverlay::testing(Tab::About).focus_target(),
            Some(ABOUT_TAB_CONTROL)
        );
    }

    #[test]
    fn selected_and_unselected_tab_states_are_distinct() {
        assert_eq!(tab_class(true), theme::ButtonClass::TabSelected);
        assert_eq!(tab_class(false), theme::ButtonClass::Tab);
        assert_ne!(tab_class(true), tab_class(false));
        assert_eq!(theme::control_focus_border().color, theme::ACCENT);
    }

    #[test]
    fn backdrop_x_and_swallowed_card_have_the_required_outcomes() {
        let mut overlay = SettingsOverlay::testing(Tab::Settings);
        assert_eq!(overlay.backdrop().a, 0.5);
        assert_eq!(overlay.update(Message::Swallow), Outcome::None);
        assert_eq!(overlay.update(Message::Dismiss), Outcome::Dismiss);
    }
}
