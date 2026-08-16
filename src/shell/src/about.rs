//! The about panel, ported from the former `web/about.js`.

use std::path::{Path, PathBuf};

use iced_core::text::{IntoFragment, Wrapping};
use iced_core::{Alignment, Element, Length, Padding};
use iced_widget::{Text, button, column, container, image, mouse_area, row, stack, text};

use crate::theme::{self, Theme};

#[derive(Clone, Debug)]
pub enum Message {
    Dismiss,
    /// A press inside the panel, absorbed so the backdrop does not dismiss.
    Swallow,
    OpenPath(PathBuf),
}

pub struct About {
    app_version: String,
    cef_version: String,
    config_dir: PathBuf,
    log_file: Option<PathBuf>,
}

impl Default for About {
    fn default() -> Self {
        Self::new()
    }
}

impl About {
    /// Rows: app version, CEF version, config directory, current log file.
    /// The two path rows are absolute and clickable.
    pub fn new() -> About {
        let log_path = jfn_logging::active_path();
        About {
            app_version: jfn_cef::APP_VERSION_FULL.to_owned(),
            cef_version: format!("{}", jfn_cef::cef_version()),
            config_dir: absolute(jfn_paths::config_dir()),
            log_file: (!log_path.is_empty()).then(|| absolute(PathBuf::from(log_path))),
        }
    }

    /// `rgba(0, 0, 0, 0.5)`: jellyfin-web stays visible and dimmed behind the
    /// panel, as it was when the panel lived in the page.
    pub fn backdrop(&self) -> iced_core::Color {
        iced_core::Color {
            a: 0.5,
            ..iced_core::Color::BLACK
        }
    }

    pub fn view(&self) -> Element<'_, Message, Theme, iced_wgpu::Renderer> {
        let mut rows = column![
            self.row("App version", &self.app_version, None),
            self.row("CEF version", &self.cef_version, None),
            self.row(
                "Config directory",
                &self.config_dir.to_string_lossy(),
                Some(self.config_dir.clone()),
            ),
        ]
        .spacing(8);
        if let Some(log) = &self.log_file {
            rows = rows.push(self.row(
                "Current log file",
                &log.to_string_lossy(),
                Some(log.clone()),
            ));
        }

        let panel = column![
            image(crate::logo::handle()).width(Length::Fixed(crate::logo::ABOUT_WIDTH)),
            rows,
        ]
        .spacing(16)
        .width(Length::Fixed(460.0));

        // The card is the bubble: it fixes the panel's width, so no content
        // length widens it, and clips its content, so no glyph reaches past its
        // fill.
        let card = container(panel)
            .padding(Padding::from([18, 24]))
            .clip(true)
            .class(theme::ContainerClass::Card);

        // The upper stack layer is the card's own size, so aligning the button
        // right and top puts it in the card's corner.
        let close = container(
            button(text("\u{00d7}").size(18))
                .on_press(Message::Dismiss)
                .class(theme::ButtonClass::Chrome),
        )
        .align_right(Length::Fill)
        .align_top(Length::Fill);

        // The backdrop is a window-filling interactive region that publishes
        // `Dismiss` on press; the panel body captures presses first, so a click
        // inside never dismisses.
        let body = mouse_area(stack![card, close]).on_press(Message::Swallow);

        mouse_area(
            container(body)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Alignment::Center)
                .align_y(Alignment::Center)
                .class(theme::ContainerClass::Backdrop),
        )
        .on_press(Message::Dismiss)
        .into()
    }

    fn row<'a>(
        &self,
        label: &'a str,
        value: &str,
        path: Option<PathBuf>,
    ) -> Element<'a, Message, Theme, iced_wgpu::Renderer> {
        let value = value.to_owned();
        let value: Element<'a, Message, Theme, iced_wgpu::Renderer> = match path {
            Some(p) => button(wrapped(value).class(Some(theme::LINK)))
                .on_press(Message::OpenPath(p))
                .class(theme::ButtonClass::Chrome)
                .padding(0)
                .into(),
            None => wrapped(value).into(),
        };
        row![
            container(wrapped(label).class(Some(theme::MUTED))).width(Length::Fixed(140.0)),
            value,
        ]
        .into()
    }

    /// Opens the row's path through `Platform::open_path`.
    pub fn open(&self, path: &Path) {
        jfn_platform_abi::get().open_path(path);
    }
}

/// Panel text that wraps on a word where it can and inside a token where it
/// cannot: a path is one unbreakable word, and word wrapping alone paints it
/// past the width its column was given.
fn wrapped<'a>(content: impl IntoFragment<'a>) -> Text<'a, Theme, iced_wgpu::Renderer> {
    text(content).wrapping(Wrapping::WordOrGlyph)
}

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path,
    }
}
