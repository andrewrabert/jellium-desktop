//! The connect screen's spinner, ported from `overlay.css`'s `spin` keyframes:
//! an 80-pixel ring, 6 pixels thick, one turn per second.

use std::time::Instant;

use iced_core::{Color, Element, Length, Point, Rectangle, Size, mouse};
use iced_widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke};

use crate::theme::Theme;

const DIAMETER: f32 = 80.0;
const THICKNESS: f32 = 6.0;
const TURN: f32 = 1.0;

pub struct Spinner {
    accent: Color,
    track: Color,
    /// Owned by the connect screen, not by the view: a widget-cache rebuild
    /// must not restart the turn.
    started: Instant,
}

impl Spinner {
    pub fn new(accent: Color, track: Color, started: Instant) -> Spinner {
        Spinner {
            accent,
            track,
            started,
        }
    }

    pub fn view<'a, Message: 'a>(self) -> Element<'a, Message, Theme, iced_wgpu::Renderer> {
        Canvas::new(self)
            .width(Length::Fixed(DIAMETER))
            .height(Length::Fixed(DIAMETER))
            .into()
    }
}

impl<Message> canvas::Program<Message, Theme, iced_wgpu::Renderer> for Spinner {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced_wgpu::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<iced_wgpu::Renderer>> {
        let mut frame = Frame::new(renderer, Size::new(DIAMETER, DIAMETER));
        let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);
        let radius = (DIAMETER - THICKNESS) / 2.0;
        frame.stroke(
            &Path::circle(center, radius),
            Stroke::default()
                .with_color(self.track)
                .with_width(THICKNESS),
        );
        let turn = (self.started.elapsed().as_secs_f32() / TURN).fract();
        frame.translate(iced_core::Vector::new(center.x, center.y));
        frame.rotate(iced_core::Radians(turn * std::f32::consts::TAU));
        frame.translate(iced_core::Vector::new(-center.x, -center.y));
        frame.stroke(
            &Path::new(|b| {
                b.arc(canvas::path::Arc {
                    center,
                    radius,
                    start_angle: iced_core::Radians(0.0),
                    end_angle: iced_core::Radians(std::f32::consts::FRAC_PI_2),
                });
            }),
            Stroke::default()
                .with_color(self.accent)
                .with_width(THICKNESS),
        );
        vec![frame.into_geometry()]
    }
}
