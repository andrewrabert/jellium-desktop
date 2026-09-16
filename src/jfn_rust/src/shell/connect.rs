use std::time::Instant;

use iced_core::widget::Id;
use iced_core::{Alignment, Color, Element, Length, Padding};
use iced_widget::{button, column, container, image, text};

use crate::connection::{FADE, Screen};

use crate::shell::actor::Deadline;
use crate::shell::lang::strings;
use crate::shell::theme::{self, Theme};

pub const URL_FIELD: Id = Id::new("shell-connect-url");

#[derive(Clone, Debug)]
pub enum Message {
    UrlEdited(String),
    Submit,
    DismissFailure,
}

pub struct Connect {
    spinner_started: Instant,
}

impl Default for Connect {
    fn default() -> Self {
        Self::new()
    }
}

impl Connect {
    pub fn new() -> Connect {
        Connect {
            spinner_started: Instant::now(),
        }
    }

    pub fn view<'a>(
        &'a self,
        screen: &'a Screen,
    ) -> Element<'a, Message, Theme, iced_wgpu::Renderer> {
        let body: Element<'a, Message, Theme, iced_wgpu::Renderer> = match screen {
            Screen::Failed => self.failure_view(),
            Screen::Working { .. } | Screen::Retiring { .. } => self.spinner_view(screen),
            Screen::Form { url } => self.form_view(url),
            Screen::Gone => iced_widget::space::horizontal().into(),
        };
        container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(16)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .class(theme::ContainerClass::Backdrop)
            .into()
    }

    fn form_view<'a>(&'a self, url: &'a str) -> Element<'a, Message, Theme, iced_wgpu::Renderer> {
        let submit = (!url.trim().is_empty()).then_some(Message::Submit);
        column![
            image(crate::shell::logo::handle())
                .width(Length::Fixed(crate::shell::logo::CONNECT_WIDTH)),
            text(strings().server_host).size(24),
            crate::shell::field::field(URL_FIELD, strings().server_host_help, url)
                .on_input(Message::UrlEdited)
                .on_submit(Message::Submit)
                .padding(Padding::from([10, 13]))
                .size(16),
            button(text(strings().connect).center())
                .on_press_maybe(submit)
                .width(Length::Fill)
                .padding(Padding::from([12, 24])),
        ]
        .spacing(16)
        .width(Length::Fixed(450.0))
        .align_x(Alignment::Center)
        .into()
    }

    fn spinner_view(&self, screen: &Screen) -> Element<'_, Message, Theme, iced_wgpu::Renderer> {
        let o = opacity(screen);
        column![
            image(crate::shell::logo::handle())
                .width(Length::Fixed(crate::shell::logo::CONNECT_WIDTH))
                .opacity(o),
            crate::shell::spinner::Spinner::new(
                fade(theme::ACCENT, o),
                fade(theme::FIELD, o),
                self.spinner_started,
            )
            .view(),
        ]
        .spacing(32)
        .align_x(Alignment::Center)
        .into()
    }

    fn failure_view(&self) -> Element<'_, Message, Theme, iced_wgpu::Renderer> {
        column![
            text(strings().connection_failure).size(24),
            container(text(strings().unable_to_connect).center())
                .width(Length::Fixed(350.0))
                .padding(Padding::from([0, 24])),
            button(text(strings().got_it).center()).on_press(Message::DismissFailure),
        ]
        .spacing(32)
        .align_x(Alignment::Center)
        .into()
    }

    pub fn backdrop(&self, chrome: Color, screen: &Screen) -> Color {
        Color {
            a: opacity(screen),
            ..chrome
        }
    }

    pub fn deadline(&self, screen: &Screen) -> Deadline {
        let spinning = matches!(screen, Screen::Working { .. } | Screen::Retiring { .. });
        match (spinning, jfn_gpu_paint::refresh_interval()) {
            (true, Some(interval)) => Deadline::at(Instant::now() + interval),
            _ => Deadline::none(),
        }
    }

    pub fn focus_target(&self, screen: &Screen) -> Option<Id> {
        matches!(screen, Screen::Form { .. }).then_some(URL_FIELD)
    }
}

fn opacity(screen: &Screen) -> f32 {
    match screen {
        Screen::Retiring { fade_from } => {
            1.0 - (fade_from.elapsed().as_secs_f32() / FADE.as_secs_f32()).clamp(0.0, 1.0)
        }
        _ => 1.0,
    }
}

fn fade(color: Color, opacity: f32) -> Color {
    Color {
        a: color.a * opacity,
        ..color
    }
}
