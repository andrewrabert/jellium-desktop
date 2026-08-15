//! The about panel, ported from the former `web/about.js`.

use std::path::{Path, PathBuf};

use iced_core::{Alignment, Element, Length, Padding};
use iced_widget::{button, column, container, image, mouse_area, row, text};

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
            row![
                image(crate::logo::handle()).width(Length::Fixed(240.0)),
                button(text("\u{00d7}").size(18))
                    .on_press(Message::Dismiss)
                    .class(theme::ButtonClass::Chrome),
            ]
            .align_y(Alignment::Start)
            .spacing(12),
            rows,
        ]
        .spacing(16)
        .width(Length::Fixed(460.0));

        // The backdrop is a window-filling interactive region that publishes
        // `Dismiss` on press; the panel body captures presses first, so a click
        // inside never dismisses.
        let body = mouse_area(
            container(panel)
                .padding(Padding::from([18, 24]))
                .class(theme::ContainerClass::Card),
        )
        .on_press(Message::Swallow);

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
            Some(p) => button(text(value).class(Some(theme::LINK)))
                .on_press(Message::OpenPath(p))
                .class(theme::ButtonClass::Chrome)
                .padding(0)
                .into(),
            None => text(value).into(),
        };
        row![
            container(text(label).class(Some(theme::MUTED))).width(Length::Fixed(140.0)),
            value,
        ]
        .into()
    }

    /// Opens the row's path through `Platform::open_path`.
    pub fn open(&self, path: &Path) {
        jfn_platform_abi::get().open_path(path);
    }
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
