//! The about panel, ported from the former `web/about.js`.

use std::path::{Path, PathBuf};

use iced_core::text::{IntoFragment, Wrapping};
use iced_core::widget::Id;
use iced_core::{Alignment, Element, Length, Padding};
use iced_widget::{Text, button, column, container, image, row, text};

use crate::controls;
use crate::theme::{self, Theme};

pub const CONFIG_DIRECTORY_CONTROL: Id = Id::new("shell-about-config-directory");
pub const CURRENT_LOG_CONTROL: Id = Id::new("shell-about-current-log");

const VERSION_LABEL: &str = "Version";
const CEF_LABEL: &str = "CEF";

#[derive(Clone, Debug)]
pub enum Message {
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

    pub fn view(&self) -> Element<'_, Message, Theme, iced_wgpu::Renderer> {
        let mut rows = column![
            self.row(VERSION_LABEL, &self.app_version, None),
            self.row(CEF_LABEL, &self.cef_version, None),
        ]
        .spacing(8);
        for (label, id, path) in self.path_actions() {
            rows = rows.push(self.row(label, &path.to_string_lossy(), Some((id, path.clone()))));
        }

        about_layout(
            image(crate::logo::handle()).width(Length::Fixed(crate::logo::ABOUT_WIDTH)),
            rows,
        )
    }

    fn path_actions(&self) -> Vec<(&'static str, Id, &PathBuf)> {
        let mut actions = vec![(
            "Config directory",
            CONFIG_DIRECTORY_CONTROL,
            &self.config_dir,
        )];
        if let Some(log) = &self.log_file {
            actions.push(("Current log file", CURRENT_LOG_CONTROL, log));
        }
        actions
    }

    fn row<'a>(
        &self,
        label: &'a str,
        value: &str,
        action: Option<(Id, PathBuf)>,
    ) -> Element<'a, Message, Theme, iced_wgpu::Renderer> {
        let value = value.to_owned();
        let value: Element<'a, Message, Theme, iced_wgpu::Renderer> = match action {
            Some((id, path)) => {
                let message = Message::OpenPath(path);
                controls::action(
                    id,
                    button(wrapped(value).class(Some(theme::LINK)))
                        .on_press(message.clone())
                        .class(theme::ButtonClass::Chrome)
                        .padding(0),
                    message,
                )
            }
            None => wrapped(value).into(),
        };
        metadata_row(wrapped(label).class(Some(theme::MUTED)), value)
    }

    /// Opens the row's path through `Platform::open_path`.
    pub fn open(&self, path: &Path) {
        jfn_platform_abi::get().open_path(path);
    }
}

fn about_layout<'a, Message: 'a, Renderer: iced_core::Renderer + 'a>(
    logo: impl Into<Element<'a, Message, Theme, Renderer>>,
    rows: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> Element<'a, Message, Theme, Renderer> {
    column![container(logo).padding(Padding::from([8, 0])), rows.into()]
        .spacing(16)
        .width(Length::Fill)
        .align_x(Alignment::Center)
        .into()
}

fn metadata_row<'a, Message: 'a, Renderer: iced_core::Renderer + 'a>(
    label: impl Into<Element<'a, Message, Theme, Renderer>>,
    value: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> Element<'a, Message, Theme, Renderer> {
    row![
        container(label).width(Length::Fixed(140.0)),
        container(value).width(Length::Fill),
    ]
    .width(Length::Fill)
    .into()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn about(log_file: Option<PathBuf>) -> About {
        About {
            app_version: "app".to_owned(),
            cef_version: "cef".to_owned(),
            config_dir: PathBuf::from("/config"),
            log_file,
        }
    }

    #[test]
    fn version_labels_are_exact() {
        assert_eq!([VERSION_LABEL, CEF_LABEL], ["Version", "CEF"]);
    }

    #[test]
    fn about_layout_fills_rows_and_centers_the_logo() {
        use iced_core::layout::Limits;
        use iced_core::widget::Tree;
        use iced_widget::Space;

        let rows = iced_widget::column![
            metadata_row(Space::new(), Space::new()),
            metadata_row(Space::new(), Space::new()),
        ];
        let mut content: Element<'_, (), Theme, ()> = about_layout(
            Space::new()
                .width(Length::Fixed(crate::logo::ABOUT_WIDTH))
                .height(Length::Fixed(1.0)),
            rows,
        );
        let mut tree = Tree::new(content.as_widget());
        tree.diff(content.as_widget_mut());
        let parent = iced_core::Size::new(632.0, 400.0);
        let node = content.as_widget_mut().layout(
            &mut tree,
            &(),
            &Limits::new(iced_core::Size::ZERO, parent),
        );
        let children = node.children();
        let logo = children[0].bounds();
        let rows = &children[1];
        let first_row = &rows.children()[0];
        let row_children = first_row.children();
        let value = row_children[1].bounds();

        assert_eq!(node.bounds().width, parent.width);
        assert_eq!(rows.bounds().width, parent.width);
        assert_eq!(first_row.bounds().width, parent.width);
        assert_eq!(row_children[0].bounds().width, 140.0);
        assert_eq!(value.x, 140.0);
        assert_eq!(value.x + value.width, first_row.bounds().width);
        assert_eq!(logo.x, (parent.width - crate::logo::ABOUT_WIDTH) / 2.0);
    }

    #[test]
    fn rendered_actions_expose_config_directory_first_without_a_log() {
        let about = about(None);

        assert_eq!(
            about
                .path_actions()
                .into_iter()
                .map(|(_, id, _)| id)
                .collect::<Vec<_>>(),
            [CONFIG_DIRECTORY_CONTROL]
        );
    }

    #[test]
    fn rendered_actions_expose_config_then_current_log_in_visual_tab_order() {
        let about = about(Some(PathBuf::from("/current.log")));

        assert_eq!(
            about
                .path_actions()
                .into_iter()
                .map(|(_, id, _)| id)
                .collect::<Vec<_>>(),
            [CONFIG_DIRECTORY_CONTROL, CURRENT_LOG_CONTROL]
        );
    }
}
