//! Keyboard focus and activation for Client Settings controls.

use iced_core::Renderer as _;
use iced_core::keyboard::{self, Key, key::Named};
use iced_core::widget::operation::{Focusable, Scrollable};
use iced_core::widget::{self, Id, Operation, Tree};
use iced_core::{
    Background, Element, Event, Layout, Length, Point, Rectangle, Shell, Size, Vector, layout,
    mouse, overlay, renderer,
};
use iced_widget::pick_list;

use crate::theme::{self, Theme};

type Renderer = iced_wgpu::Renderer;

/// The direction in which Settings keyboard focus moves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Forward,
    Backward,
}

#[derive(Debug, Default)]
struct Focus(bool);

impl Focusable for Focus {
    fn is_focused(&self) -> bool {
        self.0
    }

    fn focus(&mut self) {
        self.0 = true;
    }

    fn unfocus(&mut self) {
        self.0 = false;
    }
}

#[derive(Debug, Default)]
struct State {
    focus: Focus,
    select_open: bool,
    highlight: usize,
    selected: Option<usize>,
}

impl State {
    fn close_on_newly_captured_left_press(
        &mut self,
        event: &Event,
        was_captured: bool,
        is_captured: bool,
    ) {
        if !was_captured
            && is_captured
            && matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            )
        {
            self.select_open = false;
        }
    }
}

enum Keyboard<Message> {
    Action(Message),
    Select {
        messages: Vec<Message>,
        selected: Option<usize>,
    },
}

struct Control<'a, Message> {
    id: Id,
    content: Element<'a, Message, Theme, Renderer>,
    keyboard: Keyboard<Message>,
}

/// Makes an action or checkbox keyboard-focusable and activatable with Enter
/// or Space.
pub fn action<'a, Message: Clone + 'a>(
    id: Id,
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    message: Message,
) -> Element<'a, Message, Theme, Renderer> {
    Element::new(Control {
        id,
        content: content.into(),
        keyboard: Keyboard::Action(message),
    })
}

/// Makes a named tab keyboard-focusable and activatable with the same action
/// semantics as the other shell controls.
pub fn tab<'a, Message: Clone + 'a>(
    id: Id,
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    message: Message,
) -> Element<'a, Message, Theme, Renderer> {
    action(id, content, message)
}

/// Builds a mouse- and keyboard-capable select. Enter or Space opens it, arrow
/// keys move its cyclic highlight, Enter or Space commits, and Escape cancels.
pub fn select<'a, T, Message>(
    id: Id,
    selected: T,
    options: Vec<T>,
    to_string: impl Fn(&T) -> String + Clone + 'a,
    on_select: impl Fn(T) -> Message + Clone + 'a,
) -> Element<'a, Message, Theme, Renderer>
where
    T: Clone + PartialEq + 'a,
    Message: Clone + 'a,
{
    let selected_index = options.iter().position(|option| option == &selected);
    let messages = options.iter().cloned().map(on_select.clone()).collect();
    let content = pick_list(Some(selected), options, to_string)
        .on_select(on_select)
        .padding(iced_core::Padding::from([8, 10]));

    Element::new(Control {
        id,
        content: content.into(),
        keyboard: Keyboard::Select {
            messages,
            selected: selected_index,
        },
    })
}

impl<Message: Clone> Control<'_, Message> {
    fn click_child(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let event = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
        let cursor = mouse::Cursor::Available(layout.bounds().center());
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            &event,
            layout,
            cursor,
            renderer,
            shell,
            viewport,
        );
    }
}

impl<Message: Clone> widget::Widget<Message, Theme, Renderer> for Control<'_, Message> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<State>()
    }

    fn state(&self) -> widget::tree::State {
        let selected = match &self.keyboard {
            Keyboard::Action(_) => None,
            Keyboard::Select { selected, .. } => *selected,
        };
        widget::tree::State::new(State {
            selected,
            ..State::default()
        })
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
        if let Keyboard::Select { selected, .. } = &self.keyboard {
            let state = tree.state.downcast_mut::<State>();
            if state.selected != *selected {
                state.selected = *selected;
                state.select_open = false;
                state.highlight = selected.unwrap_or(0);
            }
        }
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            shell,
            viewport,
        );

        let state = tree.state.downcast_mut::<State>();
        if let Keyboard::Select { .. } = self.keyboard
            && matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            )
        {
            if state.select_open {
                state.select_open = false;
            } else if cursor.is_over(layout.bounds()) {
                state.select_open = true;
                state.highlight = state.selected.unwrap_or(0);
            }
        }
        if shell.is_event_captured() || !state.focus.is_focused() {
            return;
        }

        let Event::Keyboard(keyboard::Event::KeyPressed { key, .. }) = event else {
            return;
        };
        match &self.keyboard {
            Keyboard::Action(message) if activation(key) => {
                shell.publish(message.clone());
                shell.capture_event();
            }
            Keyboard::Select { messages, selected } if !messages.is_empty() => {
                match select_command(state.select_open, state.highlight, messages.len(), key) {
                    SelectCommand::Open => {
                        state.select_open = true;
                        state.highlight = selected.unwrap_or(0);
                        self.click_child(tree, layout, renderer, shell, viewport);
                        shell.capture_event();
                    }
                    SelectCommand::Move(highlight) => {
                        state.highlight = highlight;
                        shell.request_redraw();
                        shell.capture_event();
                    }
                    SelectCommand::Commit(highlight) => {
                        shell.publish(messages[highlight].clone());
                        state.select_open = false;
                        self.click_child(tree, layout, renderer, shell, viewport);
                        shell.capture_event();
                    }
                    SelectCommand::Cancel => {
                        state.select_open = false;
                        self.click_child(tree, layout, renderer, shell, viewport);
                        shell.capture_event();
                    }
                    SelectCommand::None => {}
                }
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        if tree.state.downcast_ref::<State>().focus.is_focused() {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds(),
                    border: theme::control_focus_border(),
                    ..renderer::Quad::default()
                },
                Background::Color(iced_core::Color::TRANSPARENT),
            );
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let state = tree.state.downcast_mut::<State>();
        operation.focusable(Some(&self.id), layout.bounds(), &mut state.focus);
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let content = self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )?;
        let Keyboard::Select { messages, .. } = &self.keyboard else {
            return Some(content);
        };
        let state = tree.state.downcast_mut::<State>();
        if !state.select_open || messages.is_empty() {
            return Some(content);
        }
        Some(overlay::Element::new(Box::new(SelectOverlay {
            content,
            state,
            total: messages.len(),
        })))
    }
}

struct SelectOverlay<'a, Message> {
    content: overlay::Element<'a, Message, Theme, Renderer>,
    state: &'a mut State,
    total: usize,
}

impl<Message: Clone> overlay::Overlay<Message, Theme, Renderer> for SelectOverlay<'_, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        self.content.as_overlay_mut().layout(renderer, bounds)
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.content
            .as_overlay()
            .draw(renderer, theme, style, layout, cursor);
    }

    fn operate(&mut self, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content
            .as_overlay_mut()
            .operate(layout, renderer, operation);
    }

    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
    ) {
        let Event::Keyboard(keyboard::Event::KeyPressed { key, .. }) = event else {
            let was_captured = shell.is_event_captured();
            self.content
                .as_overlay_mut()
                .update(event, layout, cursor, renderer, shell);
            self.state.close_on_newly_captured_left_press(
                event,
                was_captured,
                shell.is_event_captured(),
            );
            return;
        };
        if let Some(direction) = arrow(key) {
            self.state.highlight = moved_index(self.state.highlight, self.total, direction);
            self.move_rendered_highlight(layout, renderer, shell);
            shell.capture_event();
        } else if activation(key) {
            self.move_rendered_highlight(layout, renderer, shell);
            if let Some(cursor) = highlight_cursor(layout, self.total, self.state.highlight) {
                self.content.as_overlay_mut().update(
                    &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    layout,
                    cursor,
                    renderer,
                    shell,
                );
            }
            self.state.selected = Some(self.state.highlight);
            self.state.select_open = false;
            shell.capture_event();
        } else {
            self.content
                .as_overlay_mut()
                .update(event, layout, cursor, renderer, shell);
        }
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_overlay()
            .mouse_interaction(layout, cursor, renderer)
    }

    fn overlay<'a>(
        &'a mut self,
        layout: Layout<'a>,
        renderer: &Renderer,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_overlay_mut().overlay(layout, renderer)
    }

    fn index(&self) -> f32 {
        self.content.as_overlay().index()
    }
}

impl<Message: Clone> SelectOverlay<'_, Message> {
    fn move_rendered_highlight(
        &mut self,
        layout: Layout<'_>,
        renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
    ) {
        let Some(cursor) = highlight_cursor(layout, self.total, self.state.highlight) else {
            return;
        };
        let position = cursor.position().unwrap_or(Point::ORIGIN);
        self.content.as_overlay_mut().update(
            &Event::Mouse(mouse::Event::CursorMoved { position }),
            layout,
            cursor,
            renderer,
            shell,
        );
        shell.request_redraw();
    }
}

fn highlight_cursor(layout: Layout<'_>, total: usize, highlight: usize) -> Option<mouse::Cursor> {
    let content = layout.children().next()?.bounds();
    Some(mouse::Cursor::Available(highlight_point(
        content, total, highlight,
    )))
}

fn highlight_point(content: Rectangle, total: usize, highlight: usize) -> Point {
    let option_height = content.height / total as f32;
    Point::new(
        content.center_x(),
        content.y + option_height * (highlight as f32 + 0.5),
    )
}

fn activation(key: &Key) -> bool {
    matches!(key.as_ref(), Key::Named(Named::Enter))
        || matches!(key.as_ref(), Key::Character(value) if value == " ")
}

fn arrow(key: &Key) -> Option<Direction> {
    match key.as_ref() {
        Key::Named(Named::ArrowDown | Named::ArrowRight) => Some(Direction::Forward),
        Key::Named(Named::ArrowUp | Named::ArrowLeft) => Some(Direction::Backward),
        _ => None,
    }
}

fn moved_index(current: usize, total: usize, direction: Direction) -> usize {
    match direction {
        Direction::Forward => (current + 1) % total,
        Direction::Backward => (current + total - 1) % total,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SelectCommand {
    None,
    Open,
    Move(usize),
    Commit(usize),
    Cancel,
}

fn select_command(open: bool, highlight: usize, total: usize, key: &Key) -> SelectCommand {
    if !open && activation(key) {
        SelectCommand::Open
    } else if open && let Some(direction) = arrow(key) {
        SelectCommand::Move(moved_index(highlight, total, direction))
    } else if open && activation(key) {
        SelectCommand::Commit(highlight)
    } else if open && matches!(key.as_ref(), Key::Named(Named::Escape)) {
        SelectCommand::Cancel
    } else {
        SelectCommand::None
    }
}

#[derive(Clone)]
struct ScanResult {
    controls: Vec<(Id, Rectangle)>,
    focused: Option<usize>,
    viewport: Id,
    viewport_bounds: Option<Rectangle>,
    direction: Direction,
}

struct Scan {
    result: ScanResult,
}

impl Operation<ScanResult> for Scan {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<ScanResult>)) {
        operate(self);
    }

    fn focusable(&mut self, id: Option<&Id>, bounds: Rectangle, state: &mut dyn Focusable) {
        let Some(id) = id else { return };
        if state.is_focused() {
            self.result.focused = Some(self.result.controls.len());
            state.unfocus();
        }
        self.result.controls.push((id.clone(), bounds));
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        _content_bounds: Rectangle,
        _translation: Vector,
        _state: &mut dyn Scrollable,
    ) {
        if id == Some(&self.result.viewport) {
            self.result.viewport_bounds = Some(bounds);
        }
    }

    fn finish(&self) -> widget::operation::Outcome<ScanResult> {
        widget::operation::Outcome::Some(self.result.clone())
    }
}

struct FocusAndReveal {
    target: Option<Id>,
    viewport: Id,
    viewport_bounds: Option<Rectangle>,
    target_bounds: Option<Rectangle>,
}

impl From<ScanResult> for FocusAndReveal {
    fn from(scan: ScanResult) -> Self {
        let target_index = if scan.controls.is_empty() {
            None
        } else {
            Some(match (scan.focused, scan.direction) {
                (Some(current), Direction::Forward) => (current + 1) % scan.controls.len(),
                (Some(current), Direction::Backward) => {
                    (current + scan.controls.len() - 1) % scan.controls.len()
                }
                (None, Direction::Forward) => 0,
                (None, Direction::Backward) => scan.controls.len() - 1,
            })
        };
        let (target, target_bounds) = target_index
            .map(|index| scan.controls[index].clone())
            .map_or((None, None), |(id, bounds)| (Some(id), Some(bounds)));
        Self {
            target,
            viewport: scan.viewport,
            viewport_bounds: scan.viewport_bounds,
            target_bounds,
        }
    }
}

impl Operation for FocusAndReveal {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn focusable(&mut self, id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Focusable) {
        if id == self.target.as_ref() {
            state.focus();
        } else {
            state.unfocus();
        }
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        state: &mut dyn Scrollable,
    ) {
        if id != Some(&self.viewport) {
            return;
        }
        let (Some(viewport), Some(target)) = (self.viewport_bounds, self.target_bounds) else {
            return;
        };
        let target = target - translation;
        let y = if target.y < viewport.y {
            target.y - viewport.y
        } else if target.y + target.height > viewport.y + viewport.height {
            target.y + target.height - viewport.y - viewport.height
        } else {
            0.0
        };
        if y != 0.0 {
            state.scroll_by(
                widget::operation::scrollable::AbsoluteOffset { x: 0.0, y },
                bounds,
                content_bounds,
            );
        }
    }
}

/// Cyclically moves focus across all rendered named controls and reveals the
/// target in the named Settings viewport.
pub fn move_focus(viewport: Id, direction: Direction) -> impl Operation {
    widget::operation::then(
        Scan {
            result: ScanResult {
                controls: Vec::new(),
                focused: None,
                viewport,
                viewport_bounds: None,
                direction,
            },
        },
        FocusAndReveal::from,
    )
}

/// Captures the platform-reported absolute translation of a named scrollable.
pub struct ScrollOffset {
    viewport: Id,
    offset: Option<widget::operation::scrollable::AbsoluteOffset>,
}

impl ScrollOffset {
    pub fn get(&self) -> Option<widget::operation::scrollable::AbsoluteOffset> {
        self.offset
    }
}

impl Operation for ScrollOffset {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        _bounds: Rectangle,
        _content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn Scrollable,
    ) {
        if id == Some(&self.viewport) {
            self.offset = Some(widget::operation::scrollable::AbsoluteOffset {
                x: translation.x,
                y: translation.y,
            });
        }
    }
}

pub fn scroll_offset(viewport: Id) -> ScrollOffset {
    ScrollOffset {
        viewport,
        offset: None,
    }
}

/// Restores a named scrollable to an exact absolute offset.
pub struct RestoreScroll {
    viewport: Id,
    offset: widget::operation::scrollable::AbsoluteOffset,
}

impl Operation for RestoreScroll {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        _bounds: Rectangle,
        _content_bounds: Rectangle,
        _translation: Vector,
        state: &mut dyn Scrollable,
    ) {
        if id == Some(&self.viewport) {
            state.scroll_to(widget::operation::scrollable::AbsoluteOffset {
                x: Some(self.offset.x),
                y: Some(self.offset.y),
            });
        }
    }
}

pub fn restore_scroll(
    viewport: Id,
    offset: widget::operation::scrollable::AbsoluteOffset,
) -> RestoreScroll {
    RestoreScroll { viewport, offset }
}

/// Operation collecting the ID of the one currently focused named control.
#[derive(Default)]
pub struct FocusedId(Option<Id>);

impl FocusedId {
    pub fn get(&self) -> Option<Id> {
        self.0.clone()
    }
}

impl Operation for FocusedId {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn focusable(&mut self, id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Focusable) {
        if state.is_focused()
            && let Some(id) = id
        {
            self.0 = Some(id.clone());
        }
    }
}

/// Collects the current focused ID when applied to a rendered widget tree.
pub fn focused_id() -> FocusedId {
    FocusedId::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices_traverse_both_directions_and_wrap() {
        assert_eq!(moved_index(0, 3, Direction::Forward), 1);
        assert_eq!(moved_index(2, 3, Direction::Forward), 0);
        assert_eq!(moved_index(2, 3, Direction::Backward), 1);
        assert_eq!(moved_index(0, 3, Direction::Backward), 2);
    }

    #[test]
    fn enter_and_space_activate_while_arrows_do_not() {
        assert!(activation(&Key::Named(Named::Enter)));
        assert!(activation(&Key::Character(" ".into())));
        assert!(!activation(&Key::Named(Named::ArrowDown)));
    }

    #[test]
    fn select_opens_moves_commits_and_cancels() {
        assert_eq!(
            select_command(false, 1, 3, &Key::Named(Named::Enter)),
            SelectCommand::Open
        );
        assert_eq!(
            select_command(true, 1, 3, &Key::Named(Named::ArrowDown)),
            SelectCommand::Move(2)
        );
        assert_eq!(
            select_command(true, 2, 3, &Key::Character(" ".into())),
            SelectCommand::Commit(2)
        );
        assert_eq!(
            select_command(true, 2, 3, &Key::Named(Named::Escape)),
            SelectCommand::Cancel
        );
    }

    #[test]
    fn pointer_current_selection_closes_before_keyboard_reopen_move_and_commit() {
        let mut state = State {
            select_open: true,
            highlight: 1,
            selected: Some(1),
            ..State::default()
        };
        let press = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));

        state.close_on_newly_captured_left_press(&press, false, true);
        assert!(!state.select_open);

        assert_eq!(
            select_command(
                state.select_open,
                state.highlight,
                3,
                &Key::Named(Named::Enter)
            ),
            SelectCommand::Open
        );
        state.select_open = true;
        state.highlight = state.selected.unwrap_or(0);

        let move_command = select_command(
            state.select_open,
            state.highlight,
            3,
            &Key::Named(Named::ArrowDown),
        );
        assert_eq!(move_command, SelectCommand::Move(2));
        if let SelectCommand::Move(highlight) = move_command {
            state.highlight = highlight;
        }

        assert_eq!(
            select_command(
                state.select_open,
                state.highlight,
                3,
                &Key::Named(Named::Enter)
            ),
            SelectCommand::Commit(2)
        );
    }

    #[test]
    fn moved_select_highlight_and_enter_target_the_same_rendered_option() {
        let highlight = moved_index(1, 3, Direction::Forward);
        let bounds = Rectangle::new(Point::new(10.0, 20.0), Size::new(90.0, 60.0));

        assert_eq!(
            highlight_point(bounds, 3, highlight),
            Point::new(55.0, 70.0)
        );
        assert_eq!(
            select_command(true, highlight, 3, &Key::Named(Named::Enter)),
            SelectCommand::Commit(highlight)
        );
    }

    #[test]
    fn scan_unfocuses_before_the_returned_target_operation_focuses() {
        let ids = [Id::unique(), Id::unique()];
        let mut states = [Focus(true), Focus(false)];
        let mut scan = Scan {
            result: ScanResult {
                controls: Vec::new(),
                focused: None,
                viewport: Id::unique(),
                viewport_bounds: None,
                direction: Direction::Forward,
            },
        };
        for (id, state) in ids.iter().zip(&mut states) {
            Operation::<ScanResult>::focusable(&mut scan, Some(id), Rectangle::default(), state);
        }

        assert!(states.iter().all(|state| !state.is_focused()));

        let mut target = FocusAndReveal::from(scan.result);
        for (id, state) in ids.iter().zip(&mut states) {
            target.focusable(Some(id), Rectangle::default(), state);
        }
        assert!(states[1].is_focused());
    }

    #[test]
    fn a_focus_move_leaves_exactly_one_rendered_control_focused() {
        let ids = [Id::unique(), Id::unique(), Id::unique()];
        let mut states = [Focus(false), Focus(true), Focus(false)];
        let scan = ScanResult {
            controls: ids
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, id)| {
                    (
                        id,
                        Rectangle::new(
                            iced_core::Point::new(0.0, index as f32 * 20.0),
                            Size::new(100.0, 10.0),
                        ),
                    )
                })
                .collect(),
            focused: Some(1),
            viewport: Id::unique(),
            viewport_bounds: None,
            direction: Direction::Forward,
        };
        let mut operation = FocusAndReveal::from(scan);
        for (id, state) in ids.iter().zip(&mut states) {
            operation.focusable(Some(id), Rectangle::default(), state);
        }

        assert_eq!(states.iter().filter(|state| state.is_focused()).count(), 1);
        assert!(states[2].is_focused());
    }

    #[derive(Default)]
    struct Scroll {
        by: f32,
        restored: Option<widget::operation::scrollable::AbsoluteOffset<Option<f32>>>,
    }

    impl Scrollable for Scroll {
        fn snap_to(&mut self, _offset: widget::operation::scrollable::RelativeOffset<Option<f32>>) {
        }

        fn scroll_to(
            &mut self,
            offset: widget::operation::scrollable::AbsoluteOffset<Option<f32>>,
        ) {
            self.restored = Some(offset);
        }

        fn scroll_by(
            &mut self,
            offset: widget::operation::scrollable::AbsoluteOffset,
            _bounds: Rectangle,
            _content_bounds: Rectangle,
        ) {
            self.by = offset.y;
        }
    }

    #[test]
    fn focus_move_reveals_the_target_in_the_named_viewport() {
        let viewport = Id::unique();
        let bounds = Rectangle::new(iced_core::Point::ORIGIN, Size::new(100.0, 40.0));
        let mut operation = FocusAndReveal {
            target: Some(Id::unique()),
            viewport: viewport.clone(),
            viewport_bounds: Some(bounds),
            target_bounds: Some(Rectangle::new(
                iced_core::Point::new(0.0, 60.0),
                Size::new(100.0, 10.0),
            )),
        };
        let mut scroll = Scroll::default();
        operation.scrollable(
            Some(&viewport),
            bounds,
            Rectangle::new(iced_core::Point::ORIGIN, Size::new(100.0, 100.0)),
            Vector::ZERO,
            &mut scroll,
        );

        assert_eq!(scroll.by, 30.0);
    }

    #[test]
    fn nonzero_translation_reveal_scrolls_only_the_remaining_delta() {
        let viewport = Id::unique();
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(100.0, 40.0));
        let mut operation = FocusAndReveal {
            target: Some(Id::unique()),
            viewport: viewport.clone(),
            viewport_bounds: Some(bounds),
            target_bounds: Some(Rectangle::new(
                Point::new(0.0, 80.0),
                Size::new(100.0, 10.0),
            )),
        };
        let mut scroll = Scroll::default();
        operation.scrollable(
            Some(&viewport),
            bounds,
            Rectangle::new(Point::ORIGIN, Size::new(100.0, 100.0)),
            Vector::new(0.0, 30.0),
            &mut scroll,
        );

        assert_eq!(scroll.by, 20.0);
    }

    #[test]
    fn tabs_participate_in_forward_and_backward_named_traversal() {
        let settings_tab = Id::new("settings-tab");
        let about_tab = Id::new("about-tab");
        let setting = Id::new("setting");
        let scan = |focused, direction| ScanResult {
            controls: [settings_tab.clone(), about_tab.clone(), setting.clone()]
                .into_iter()
                .map(|id| (id, Rectangle::default()))
                .collect(),
            focused,
            viewport: Id::unique(),
            viewport_bounds: None,
            direction,
        };

        assert_eq!(
            FocusAndReveal::from(scan(Some(0), Direction::Forward)).target,
            Some(about_tab.clone())
        );
        assert_eq!(
            FocusAndReveal::from(scan(Some(0), Direction::Backward)).target,
            Some(setting)
        );
    }

    #[test]
    fn platform_scroll_offset_is_captured_and_restored_exactly() {
        let viewport = Id::unique();
        let mut capture = scroll_offset(viewport.clone());
        let mut scroll = Scroll::default();
        capture.scrollable(
            Some(&viewport),
            Rectangle::default(),
            Rectangle::default(),
            Vector::new(3.25, 91.5),
            &mut scroll,
        );
        let offset = capture.get().expect("named viewport offset");
        assert_eq!(
            offset,
            widget::operation::scrollable::AbsoluteOffset { x: 3.25, y: 91.5 }
        );

        let mut restore = restore_scroll(viewport.clone(), offset);
        restore.scrollable(
            Some(&viewport),
            Rectangle::default(),
            Rectangle::default(),
            Vector::ZERO,
            &mut scroll,
        );
        assert_eq!(
            scroll.restored,
            Some(widget::operation::scrollable::AbsoluteOffset {
                x: Some(3.25),
                y: Some(91.5),
            })
        );
    }

    #[test]
    fn focus_ring_is_the_existing_two_pixel_accent() {
        let border = theme::control_focus_border();
        assert_eq!(border.color, theme::ACCENT);
        assert_eq!(border.width, 2.0);
    }
}
