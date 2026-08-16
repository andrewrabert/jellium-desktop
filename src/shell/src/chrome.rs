//! The titlebar view and the state the rest of the process pushes into it.

use parking_lot::Mutex;

use crate::state::ChromeInputs;

static INPUTS: Mutex<ChromeInputs> = Mutex::new(ChromeInputs {
    client_side_decorations: false,
    fullscreen: false,
    video_active: false,
    osd_visible: false,
});

type Listener = Box<dyn Fn(ChromeInputs) + Send + Sync>;

static LISTENER: Mutex<Option<Listener>> = Mutex::new(None);

pub fn inputs() -> ChromeInputs {
    *INPUTS.lock()
}

/// Registered once by `shell_start`; fired on every change.
pub fn set_listener(f: Listener) {
    *LISTENER.lock() = Some(f);
}

/// jellyfin-web's video OSD visibility, pushed from `jfn_playback::chrome`.
pub fn set_osd_visible(visible: bool) {
    update(|i| i.osd_visible = visible);
}

/// jellyfin-web is playing video, pushed from `jfn_playback::chrome`.
pub fn set_video_active(active: bool) {
    update(|i| i.video_active = active);
}

pub fn set_client_side_decorations(client_side: bool) {
    update(|i| i.client_side_decorations = client_side);
}

pub fn set_fullscreen(fullscreen: bool) {
    update(|i| i.fullscreen = fullscreen);
}

fn update(f: impl FnOnce(&mut ChromeInputs)) {
    let changed = {
        let mut inputs = INPUTS.lock();
        let before = *inputs;
        f(&mut inputs);
        (*inputs != before).then_some(*inputs)
    };
    if let Some(now) = changed
        && let Some(l) = LISTENER.lock().as_ref()
    {
        l(now);
    }
}

use iced_core::{Alignment, Element, Length, Point, Rectangle, Size, mouse};
use iced_widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke};
use iced_widget::{button, container, row, space};

use crate::theme::{self, Theme};

/// `csd.js`'s `button { width: 46px }`.
const CONTROL_WIDTH: f32 = 46.0;

/// Logical width of the minimize/maximize/close strip, published into
/// [`jfn_input::ShellState`] so the router can tell a control press from a
/// drag.
pub const CONTROLS_LOGICAL_WIDTH: i32 = 3 * CONTROL_WIDTH as i32;

/// The icons' `viewBox="0 0 11 11"`, drawn at `svg { width: 11px }`.
const ICON: f32 = 11.0;

/// `svg { stroke-width: 1.2 }`.
const ICON_STROKE: f32 = 1.2;

#[derive(Clone, Copy, Debug)]
pub enum Message {
    Minimize,
    ToggleMaximize,
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Icon {
    Minimize,
    Maximize,
    Close,
}

/// The widget tree holds only the three window controls, which act on release
/// like every button. Dragging and resizing are press gestures and never reach
/// the tree — `jfn_input::ShellInput::window_gesture` performs them.
#[derive(Default)]
pub struct Titlebar;

impl Titlebar {
    pub fn new() -> Titlebar {
        Titlebar
    }

    pub fn view(&self) -> Element<'_, Message, Theme, iced_wgpu::Renderer> {
        let height = Length::Fixed(jfn_platform_abi::TITLEBAR_LOGICAL_HEIGHT as f32);
        container(
            row![
                space::horizontal(),
                control(
                    Icon::Minimize,
                    Message::Minimize,
                    theme::ButtonClass::Chrome
                ),
                control(
                    Icon::Maximize,
                    Message::ToggleMaximize,
                    theme::ButtonClass::Chrome
                ),
                control(Icon::Close, Message::Close, theme::ButtonClass::Close),
            ]
            .align_y(Alignment::Center)
            .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(height)
        .class(theme::ContainerClass::Chrome)
        .into()
    }

    pub fn update(&mut self, message: Message) {
        if let Message::Close = message {
            jfn_playback::shutdown::jfn_shutdown_initiate();
            return;
        }
        let Some(controls) = jfn_platform_abi::get().titlebar_controls() else {
            return;
        };
        match message {
            Message::Minimize => controls.minimize(),
            Message::ToggleMaximize => controls.toggle_maximize(),
            Message::Close => {}
        }
    }
}

fn control(
    icon: Icon,
    message: Message,
    class: theme::ButtonClass,
) -> Element<'static, Message, Theme, iced_wgpu::Renderer> {
    button(
        Canvas::new(icon)
            .width(Length::Fixed(ICON))
            .height(Length::Fixed(ICON)),
    )
    .on_press(message)
    .width(Length::Fixed(CONTROL_WIDTH))
    .height(Length::Fill)
    .padding(0)
    .class(class)
    .into()
}

impl<Message> canvas::Program<Message, Theme, iced_wgpu::Renderer> for Icon {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced_wgpu::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<iced_wgpu::Renderer>> {
        let mut frame = Frame::new(renderer, Size::new(bounds.width, bounds.height));
        let stroke = Stroke::default()
            .with_color(theme::CHROME_TEXT)
            .with_width(ICON_STROKE);
        let path = match self {
            Icon::Minimize => Path::line(Point::new(1.0, 6.0), Point::new(10.0, 6.0)),
            Icon::Maximize => Path::rectangle(Point::new(1.5, 1.5), Size::new(8.0, 8.0)),
            Icon::Close => Path::new(|b| {
                b.move_to(Point::new(1.5, 1.5));
                b.line_to(Point::new(9.5, 9.5));
                b.move_to(Point::new(9.5, 1.5));
                b.line_to(Point::new(1.5, 9.5));
            }),
        };
        frame.stroke(&path, stroke);
        vec![frame.into_geometry()]
    }
}
