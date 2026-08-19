//! The overlay's palette, ported from the former `web/overlay.css`.

use iced_core::Color;

/// Every opaque pixel the shell overlay puts on screen belongs to a widget:
/// the frame is always cleared fully transparent, so wherever no widget draws,
/// jellyfin-web shows through. The two colours a frame cannot derive for
/// itself are carried here.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Theme {
    /// The titlebar strip's fill.
    pub chrome_background: Color,
    /// The open modal's backdrop, alpha included.
    pub backdrop: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            chrome_background: BACKGROUND,
            backdrop: Color::TRANSPARENT,
        }
    }
}

/// The buffered theme colour jellyfin-web last reported, [`BACKGROUND`] until
/// it unlocks. The titlebar and the modal backdrop both paint it.
pub fn chrome_background() -> Color {
    from_rgb(jfn_color::theme::jfn_theme_color_current())
}

pub(crate) fn from_rgb(value: u32) -> Color {
    rgb(
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    )
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

/// `#101010`, the same background the app already paints everywhere else.
pub const BACKGROUND: Color = rgb(0x10, 0x10, 0x10);

/// The bundled family, named rather than left to `Family::SansSerif` — which
/// cosmic-text resolves against the system database. Every widget the shell
/// overlay builds takes it as [`iced_core::renderer::Settings::default_font`],
/// so it is the overlay's `Font::DEFAULT` in the only sense that governs what
/// is drawn.
pub const FONT: iced_core::Font = iced_core::Font::new("Noto Sans");

/// `#2b2b2b`, the about card's fill.
pub const CARD: Color = rgb(0x2b, 0x2b, 0x2b);

/// `#555555`, the about card's border.
pub const CARD_BORDER: Color = rgb(0x55, 0x55, 0x55);

/// `#00a4dc`.
pub const ACCENT: Color = rgb(0x00, 0xa4, 0xdc);

/// `#292929`.
pub const FIELD: Color = rgb(0x29, 0x29, 0x29);

/// `rgba(255, 255, 255, 0.8)`.
pub const TEXT: Color = Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 0.8,
};

impl iced_core::widget::text::Catalog for Theme {
    type Class<'a> = Option<Color>;

    fn default<'a>() -> Self::Class<'a> {
        None
    }

    fn style(&self, class: &Self::Class<'_>) -> iced_core::widget::text::Style {
        iced_core::widget::text::Style {
            color: Some(class.unwrap_or(TEXT)),
        }
    }
}

/// [`FIELD`] fill, a two-pixel border that is [`ACCENT`] while focused and
/// [`FIELD`] otherwise, [`TEXT`] value, [`ACCENT`] selection.
pub fn field_style(focused: bool) -> crate::field::Style {
    crate::field::Style {
        background: iced_core::Background::Color(FIELD),
        border: iced_core::Border {
            color: if focused { ACCENT } else { FIELD },
            width: 2.0,
            radius: 3.0.into(),
        },
        placeholder: Color {
            a: 0.3,
            ..Color::WHITE
        },
        value: TEXT,
        selection: ACCENT,
    }
}

/// The focus indicator shared by Settings actions, checkboxes, and selects.
/// It overlays their existing state without replacing its fill or text style.
pub fn control_focus_border() -> iced_core::Border {
    iced_core::Border {
        color: ACCENT,
        width: 2.0,
        radius: 3.0.into(),
    }
}

impl iced_widget::button::Catalog for Theme {
    type Class<'a> = ButtonClass;

    fn default<'a>() -> Self::Class<'a> {
        ButtonClass::Primary
    }

    fn style(
        &self,
        class: &Self::Class<'_>,
        status: iced_widget::button::Status,
    ) -> iced_widget::button::Style {
        use iced_widget::button::Status;
        let base = iced_widget::button::Style {
            background: Some(iced_core::Background::Color(ACCENT)),
            text_color: Color::WHITE,
            border: iced_core::Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 3.0.into(),
            },
            shadow: iced_core::Shadow::default(),
            snap: true,
        };
        match (class, status) {
            (ButtonClass::Primary, Status::Hovered | Status::Pressed) => {
                iced_widget::button::Style {
                    background: Some(iced_core::Background::Color(rgb(0x0c, 0xb0, 0xe8))),
                    ..base
                }
            }
            (ButtonClass::Primary, Status::Disabled) => iced_widget::button::Style {
                background: Some(iced_core::Background::Color(FIELD)),
                text_color: Color {
                    a: 0.6,
                    ..Color::WHITE
                },
                ..base
            },
            (ButtonClass::Primary, Status::Active) => base,
            (ButtonClass::Chrome, Status::Hovered | Status::Pressed) => {
                iced_widget::button::Style {
                    background: Some(iced_core::Background::Color(Color {
                        a: 0.28,
                        ..rgb(0x7f, 0x7f, 0x7f)
                    })),
                    text_color: CHROME_TEXT,
                    border: iced_core::Border::default(),
                    ..base
                }
            }
            (ButtonClass::Close, Status::Hovered | Status::Pressed) => iced_widget::button::Style {
                background: Some(iced_core::Background::Color(rgb(0xc4, 0x2b, 0x1c))),
                text_color: Color::WHITE,
                border: iced_core::Border::default(),
                ..base
            },
            (ButtonClass::TabSelected, _) => iced_widget::button::Style {
                background: Some(iced_core::Background::Color(ACCENT)),
                text_color: Color::WHITE,
                border: iced_core::Border::default(),
                ..base
            },
            (ButtonClass::Tab, Status::Hovered | Status::Pressed) => iced_widget::button::Style {
                background: Some(iced_core::Background::Color(FIELD)),
                text_color: CHROME_TEXT,
                border: iced_core::Border::default(),
                ..base
            },
            (ButtonClass::Chrome | ButtonClass::Close | ButtonClass::Tab, _) => {
                iced_widget::button::Style {
                    background: None,
                    text_color: CHROME_TEXT,
                    border: iced_core::Border::default(),
                    ..base
                }
            }
        }
    }
}

impl iced_widget::checkbox::Catalog for Theme {
    type Class<'a> = ();

    fn default<'a>() -> Self::Class<'a> {}

    fn style(
        &self,
        _class: &Self::Class<'_>,
        status: iced_widget::checkbox::Status,
    ) -> iced_widget::checkbox::Style {
        use iced_widget::checkbox::Status;
        let (checked, highlighted) = match status {
            Status::Active { is_checked } => (is_checked, false),
            Status::Hovered { is_checked } => (is_checked, true),
            Status::Disabled { is_checked } => (is_checked, false),
        };
        iced_widget::checkbox::Style {
            background: iced_core::Background::Color(if checked { ACCENT } else { FIELD }),
            icon_color: Color::WHITE,
            border: iced_core::Border {
                color: if highlighted { ACCENT } else { CARD_BORDER },
                width: 2.0,
                radius: 3.0.into(),
            },
            text_color: Some(TEXT),
        }
    }
}

impl iced_widget::scrollable::Catalog for Theme {
    type Class<'a> = ();

    fn default<'a>() -> Self::Class<'a> {}

    fn style(
        &self,
        _class: &Self::Class<'_>,
        _status: iced_widget::scrollable::Status,
    ) -> iced_widget::scrollable::Style {
        let rail = iced_widget::scrollable::Rail {
            background: Some(iced_core::Background::Color(FIELD)),
            border: iced_core::Border::default(),
            scroller: iced_widget::scrollable::Scroller {
                background: iced_core::Background::Color(CARD_BORDER),
                border: iced_core::Border {
                    radius: 3.0.into(),
                    ..iced_core::Border::default()
                },
            },
        };
        iced_widget::scrollable::Style {
            container: iced_widget::container::Style::default(),
            vertical_rail: rail,
            horizontal_rail: rail,
            gap: None,
            auto_scroll: iced_widget::scrollable::AutoScroll {
                background: iced_core::Background::Color(CARD),
                border: iced_core::Border::default(),
                shadow: iced_core::Shadow::default(),
                icon: TEXT,
            },
        }
    }
}

impl iced_widget::overlay::menu::Catalog for Theme {
    type Class<'a> = ();

    fn default<'a>() -> <Self as iced_widget::overlay::menu::Catalog>::Class<'a> {}

    fn style(
        &self,
        _class: &<Self as iced_widget::overlay::menu::Catalog>::Class<'_>,
    ) -> iced_widget::overlay::menu::Style {
        iced_widget::overlay::menu::Style {
            background: iced_core::Background::Color(FIELD),
            border: iced_core::Border {
                color: CARD_BORDER,
                width: 1.0,
                radius: 3.0.into(),
            },
            text_color: TEXT,
            selected_text_color: Color::WHITE,
            selected_background: iced_core::Background::Color(ACCENT),
            shadow: iced_core::Shadow::default(),
        }
    }
}

impl iced_widget::pick_list::Catalog for Theme {
    type Class<'a> = ();

    fn default<'a>() -> <Self as iced_widget::pick_list::Catalog>::Class<'a> {}

    fn style(
        &self,
        _class: &<Self as iced_widget::pick_list::Catalog>::Class<'_>,
        status: iced_widget::pick_list::Status,
    ) -> iced_widget::pick_list::Style {
        use iced_widget::pick_list::Status;
        let highlighted = matches!(status, Status::Hovered | Status::Opened { .. });
        iced_widget::pick_list::Style {
            text_color: TEXT,
            placeholder_color: Color {
                a: 0.3,
                ..Color::WHITE
            },
            handle_color: TEXT,
            background: iced_core::Background::Color(FIELD),
            border: iced_core::Border {
                color: if highlighted { ACCENT } else { FIELD },
                width: 2.0,
                radius: 3.0.into(),
            },
        }
    }
}

impl iced_widget::container::Catalog for Theme {
    type Class<'a> = ContainerClass;

    fn default<'a>() -> Self::Class<'a> {
        ContainerClass::Transparent
    }

    fn style(&self, class: &Self::Class<'_>) -> iced_widget::container::Style {
        let fill = match class {
            ContainerClass::Transparent => None,
            ContainerClass::Backdrop => Some(self.backdrop),
            ContainerClass::Chrome => Some(self.chrome_background),
            ContainerClass::Card => Some(CARD),
        };
        let background = fill.map(iced_core::Background::Color);
        let border = match class {
            ContainerClass::Card => iced_core::Border {
                color: CARD_BORDER,
                width: 1.0,
                radius: 8.0.into(),
            },
            _ => iced_core::Border::default(),
        };
        iced_widget::container::Style {
            text_color: Some(TEXT),
            background,
            border,
            ..iced_widget::container::Style::default()
        }
    }
}

/// `#8ab4f8`, the about panel's clickable paths.
pub const LINK: Color = rgb(0x8a, 0xb4, 0xf8);

/// `#888`, the about panel's row labels.
pub const MUTED: Color = rgb(0x88, 0x88, 0x88);

/// `#f0f0f0`, the titlebar's icon colour.
pub const CHROME_TEXT: Color = rgb(0xf0, 0xf0, 0xf0);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ButtonClass {
    #[default]
    Primary,
    Chrome,
    Close,
    Tab,
    TabSelected,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ContainerClass {
    #[default]
    Transparent,
    /// The open modal's full-window field, filled with [`Theme::backdrop`].
    Backdrop,
    /// The titlebar strip, filled opaque with [`Theme::chrome_background`].
    Chrome,
    /// The about panel's body: [`CARD`], a one-pixel [`CARD_BORDER`],
    /// eight-pixel corners.
    Card,
}
