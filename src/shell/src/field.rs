//! The shell's own text field.
//!
//! Every text field the shell overlay draws is one of these. It is built on
//! iced's public editor primitives — [`iced_core::text::Editor`],
//! [`iced_core::text::editor::State`] and
//! [`iced_core::text::paragraph::Plain`] — and owns its own undo history, so
//! the history is the widget's rather than the editor's private one.
//!
//! The field is one line: [`Act::Paste`] and every inserted character run
//! through [`jfn_input::text::one_line`], and the Enter key submits rather
//! than breaking the line.

use std::any::Any;
use std::sync::Arc;

use iced_core::Renderer as _;
use iced_core::layout::{self, Layout};
use iced_core::text::editor::{self, Action, Binding, Cursor, Edit, Editor as _, KeyPress};
use iced_core::text::paragraph::Plain;
use iced_core::text::{self, LineHeight, Renderer as _, Text, Wrapping};
use iced_core::widget::Id;
use iced_core::widget::operation::Focusable;
use iced_core::widget::{self, Widget};
use iced_core::{
    Background, Border, Color, Element, Event, Length, Padding, Pixels, Point, Rectangle, Shell,
    Size, Vector, alignment, clipboard, mouse, renderer, window,
};

use jfn_platform_abi::DisplayBackend;

use crate::theme::Theme;

type Renderer = iced_wgpu::Renderer;
type TextEditor = <Renderer as text::Renderer>::Editor;
type Font = <Renderer as text::Renderer>::Font;

/// The field's fill, border and text colours.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Style {
    pub background: Background,
    pub border: Border,
    pub placeholder: Color,
    pub value: Color,
    pub selection: Color,
}

/// One edit applied to a named field, focused or not.
#[derive(Clone, PartialEq, Debug)]
pub enum Act {
    Undo,
    Redo,
    Cut,
    Copy,
    /// Inserted through [`jfn_input::text::one_line`].
    Paste(String),
    SelectAll,
    /// Places the caret at the window point and collapses the selection.
    PlaceCaret(Point),
    /// Selects the word under the window point.
    SelectWord(Point),
}

/// One step of the field's own undo history: the text and the caret as they
/// stood before the edit that recorded it.
#[derive(Clone, Debug)]
struct Step {
    text: String,
    cursor: Cursor,
}

/// One field's editor, its focus and caret, and its undo history.
///
/// The widget hands it to [`iced_core::widget::Operation::custom`], which is
/// how the shell reads and edits a field iced's own operations cannot reach.
pub struct State {
    editor: TextEditor,
    focus: editor::State,
    placeholder: Plain<<Renderer as text::Renderer>::Paragraph>,
    /// The widget's layout bounds, in window coordinates, as the last layout
    /// pass placed them. A window point an [`Act`] carries is resolved
    /// against these.
    bounds: Rectangle,
    /// The editor's origin within [`State::bounds`].
    origin: Vector,
    undo: Vec<Step>,
    redo: Vec<Step>,
    /// Bumped by every change that altered the selected text.
    selection_generation: u64,
    /// The editor holds a value the model has not been told about.
    dirty: bool,
    /// An operation removed focus; its update is delivered on the next event
    /// pass, when the widget has a shell to publish through.
    operation_unfocus: bool,
}

impl Default for State {
    fn default() -> Self {
        State {
            editor: TextEditor::with_text(""),
            focus: editor::State::new(),
            placeholder: Plain::default(),
            bounds: Rectangle::default(),
            origin: Vector::ZERO,
            undo: Vec::new(),
            redo: Vec::new(),
            selection_generation: 0,
            dirty: false,
            operation_unfocus: false,
        }
    }
}

impl State {
    /// The value; the placeholder is not part of it.
    pub fn is_empty(&self) -> bool {
        self.editor.is_empty()
    }

    /// The selected text; `None` when the selection is empty.
    pub fn selection(&self) -> Option<String> {
        self.editor.copy()
    }

    /// An undo step was recorded and not yet taken back.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// An undo step was taken back and not yet put back.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn is_focused(&self) -> bool {
        self.focus.is_focused()
    }

    /// Bumped by every change that altered the selected text, a selection
    /// replaced by an identical one included, the empty selection included.
    pub fn selection_generation(&self) -> u64 {
        self.selection_generation
    }

    /// Runs `change` on the editor, marking the value unpublished when it
    /// altered the text and bumping the selection generation when it altered
    /// the selection.
    fn change(&mut self, change: impl FnOnce(&mut TextEditor)) {
        let text = self.editor.text();
        let selection = self.editor.copy();
        change(&mut self.editor);
        self.dirty |= self.editor.text() != text;
        if self.editor.copy() != selection {
            self.selection_generation = self.selection_generation.wrapping_add(1);
        }
    }

    /// Whether the editor holds a value the model has not been told about,
    /// taking the mark with it.
    fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    fn take_operation_unfocus(&mut self) -> bool {
        std::mem::take(&mut self.operation_unfocus)
    }

    /// The editor's origin within the widget's layout bounds.
    pub fn origin(&self) -> Vector {
        self.origin
    }

    /// The caret, in the coordinates of the editor's origin.
    pub fn caret(&self) -> Point {
        match self.editor.selection() {
            editor::Selection::Caret(position) => position,
            editor::Selection::Range(ranges) => ranges.last().map_or(Point::ORIGIN, |range| {
                Point::new(range.x + range.width, range.y)
            }),
        }
    }

    /// The selection's rectangles, in the coordinates of the editor's origin.
    pub fn selection_bounds(&self) -> Vec<Rectangle> {
        match self.editor.selection() {
            editor::Selection::Caret(_) => Vec::new(),
            editor::Selection::Range(ranges) => ranges,
        }
    }

    /// Applies `act`, recording an undo step for every edit that changed the
    /// text, and returns the text `Cut` and `Copy` produced.
    ///
    /// The history holds one step per changed edit, coalesces none and is
    /// unbounded, matching what iced's own editor records; `Undo` and `Redo`
    /// restore a step rather than reaching the editor's private history.
    pub fn act(&mut self, act: &Act) -> Option<String> {
        match act {
            Act::Undo => {
                self.undo_step();
                None
            }
            Act::Redo => {
                self.redo_step();
                None
            }
            Act::Copy => self.editor.copy(),
            Act::Cut => {
                let selection = self.editor.copy()?;
                self.edit(Edit::Backspace);
                Some(selection)
            }
            Act::Paste(text) => {
                self.edit(Edit::Paste(Arc::new(jfn_input::text::one_line(text))));
                None
            }
            Act::SelectAll => {
                self.change(|editor| editor.perform(Action::SelectAll));
                None
            }
            Act::PlaceCaret(at) => {
                let at = self.local(*at);
                self.change(|editor| {
                    editor.perform(Action::Click(at, mouse::click::Kind::Single));
                });
                None
            }
            Act::SelectWord(at) => {
                let at = self.local(*at);
                self.change(|editor| {
                    editor.perform(Action::Click(at, mouse::click::Kind::Single));
                    editor.perform(Action::SelectWord);
                });
                None
            }
        }
    }

    /// A window point in the coordinates of the editor's origin.
    fn local(&self, at: Point) -> Point {
        at - Vector::new(self.bounds.x, self.bounds.y) - self.origin
    }

    /// Performs one editing action, recording an undo step when it changed the
    /// text.
    fn edit(&mut self, edit: Edit) {
        let before = Step {
            text: self.editor.text(),
            cursor: self.editor.cursor(),
        };
        self.change(|editor| editor.perform(Action::Edit(edit)));
        if self.editor.text() != before.text {
            self.undo.push(before);
            self.redo.clear();
        }
    }

    fn undo_step(&mut self) {
        let Some(step) = self.undo.pop() else {
            return;
        };
        let displaced = self.restore(step);
        self.redo.push(displaced);
    }

    fn redo_step(&mut self) {
        let Some(step) = self.redo.pop() else {
            return;
        };
        let displaced = self.restore(step);
        self.undo.push(displaced);
    }

    /// Puts `step` into the editor and returns the state it displaced.
    fn restore(&mut self, step: Step) -> Step {
        let displaced = Step {
            text: self.editor.text(),
            cursor: self.editor.cursor(),
        };
        self.change(|editor| {
            editor.overwrite(&step.text);
            editor.move_to(step.cursor);
        });
        displaced
    }

    /// Replaces the whole value without recording a step: the caller's value is
    /// authoritative, a rebuilt widget tree is not an edit, and a value the
    /// model replaced leaves no step to take back.
    fn adopt(&mut self, value: &str) {
        if self.editor.text() == value {
            return;
        }
        self.change(|editor| editor.overwrite(value));
        self.dirty = false;
        self.undo.clear();
        self.redo.clear();
    }
}

/// The binding a key press resolves to inside a field.
///
/// `Escape` resolves to none, so it leaves the field's caret and focus intact
/// and reaches the modal stack as an ignored event.
///
/// Redo is Ctrl+Y on Windows, Wayland and X11, Ctrl+Shift+Z on Wayland and
/// X11, and Command+Shift+Z on macOS; Command+Shift+Z is redo before it is
/// undo.
///
/// Every other press resolves through
/// [`iced_core::text::editor::Binding::from_key_press`].
pub fn binding<Message>(backend: DisplayBackend, key_press: KeyPress) -> Option<Binding<Message>> {
    use iced_core::keyboard::{Key, key::Named};

    if !key_press.is_focused {
        return None;
    }
    if matches!(key_press.key.as_ref(), Key::Named(Named::Escape)) {
        return None;
    }
    let latin = key_press.key.to_latin(key_press.physical_key);
    let modifiers = key_press.modifiers;
    let shift_redo = matches!(
        backend,
        DisplayBackend::Wayland | DisplayBackend::X11 | DisplayBackend::MacOS
    );
    let y_redo = matches!(
        backend,
        DisplayBackend::Windows | DisplayBackend::Wayland | DisplayBackend::X11
    );
    if modifiers.command() {
        if shift_redo && modifiers.shift() && latin == Some('z') {
            return Some(Binding::Redo);
        }
        if y_redo && latin == Some('y') {
            return Some(Binding::Redo);
        }
    }
    Binding::from_key_press(key_press)
}

/// A shell field. Every text field the shell overlay draws is one of these.
pub struct Field<'a, Message> {
    id: Id,
    placeholder: &'a str,
    value: &'a str,
    on_input: Option<Box<dyn Fn(String) -> Message + 'a>>,
    on_submit: Option<Message>,
    on_unfocus: Option<Message>,
    padding: Padding,
    size: Option<Pixels>,
    width: Length,
}

/// Every shell field carries an [`Id`]; there is no unnamed one.
pub fn field<'a, Message>(id: Id, placeholder: &'a str, value: &'a str) -> Field<'a, Message> {
    Field {
        id,
        placeholder,
        value,
        on_input: None,
        on_submit: None,
        on_unfocus: None,
        padding: Padding::new(0.0),
        size: None,
        width: Length::Fill,
    }
}

impl<'a, Message: Clone + 'a> Field<'a, Message> {
    pub fn on_input(mut self, on_input: impl Fn(String) -> Message + 'a) -> Field<'a, Message> {
        self.on_input = Some(Box::new(on_input));
        self
    }

    pub fn on_submit(mut self, message: Message) -> Field<'a, Message> {
        self.on_submit = Some(message);
        self
    }

    /// Publishes `message` after this field loses keyboard focus.
    pub fn on_unfocus(mut self, message: Message) -> Field<'a, Message> {
        self.on_unfocus = Some(message);
        self
    }

    pub fn padding(mut self, padding: Padding) -> Field<'a, Message> {
        self.padding = padding;
        self
    }

    pub fn size(mut self, size: impl Into<Pixels>) -> Field<'a, Message> {
        self.size = Some(size.into());
        self
    }

    pub fn width(mut self, width: Length) -> Field<'a, Message> {
        self.width = width;
        self
    }

    fn text_size(&self, renderer: &Renderer) -> Pixels {
        self.size.unwrap_or_else(|| renderer.default_size())
    }

    fn font(&self, renderer: &Renderer) -> Font {
        renderer.default_font()
    }

    /// The text the placeholder is drawn from, laid out to `bounds`.
    fn placeholder_text(&self, renderer: &Renderer, bounds: Size) -> Text<&'a str, Font> {
        Text {
            content: self.placeholder,
            bounds,
            size: self.text_size(renderer),
            line_height: LineHeight::default(),
            font: self.font(renderer),
            align_x: text::Alignment::Default,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Advanced,
            wrapping: Wrapping::None,
            ellipsis: text::Ellipsis::None,
            hint_factor: renderer.hint_factor(),
        }
    }

    /// Publishes the editor's value when a change has not reached the model
    /// yet. The model holds the field's value and the editor is only where it
    /// is edited, so a key press, an [`Act`] a menu queued and an [`Act`] a
    /// middle press queued all publish here, once per pass.
    fn commit(&self, state: &mut State, shell: &mut Shell<'_, Message>) {
        let Some(on_input) = &self.on_input else {
            return;
        };
        if !state.take_dirty() {
            return;
        }
        shell.publish(on_input(state.editor.text()));
    }

    /// Applies one editor action. Enter submits the edited value rather than
    /// breaking the line, undo and redo take the field's own history, and every
    /// insertion stays on one line.
    fn apply(&self, state: &mut State, action: Action, shell: &mut Shell<'_, Message>) {
        match action {
            Action::Edit(Edit::Enter) => {
                self.commit(state, shell);
                if let Some(message) = self.on_submit.clone() {
                    shell.publish(message);
                }
            }
            Action::Edit(Edit::Undo) => state.undo_step(),
            Action::Edit(Edit::Redo) => state.redo_step(),
            Action::Edit(Edit::Paste(text)) => {
                state.edit(Edit::Paste(Arc::new(jfn_input::text::one_line(&text))));
            }
            Action::Edit(Edit::Insert(c)) => {
                if !c.is_control() {
                    state.edit(Edit::Insert(c));
                }
            }
            Action::Edit(edit) => state.edit(edit),
            action => state.change(|editor| editor.perform(action)),
        }
    }

    fn apply_update(
        &self,
        state: &mut State,
        update: editor::Update<Message>,
        shell: &mut Shell<'_, Message>,
    ) {
        match update {
            editor::Update::Action(action) => self.apply(state, action, shell),
            editor::Update::Copy(content) => {
                shell.write_clipboard(clipboard::Content::Text(content));
            }
            editor::Update::Paste => shell.read_clipboard(clipboard::Kind::Text),
            editor::Update::RedrawAt(at) => shell.request_redraw_at(at),
            editor::Update::Custom(message) => shell.publish(message),
            editor::Update::Sequence(updates) => {
                for update in updates {
                    self.apply_update(state, update, shell);
                }
            }
            editor::Update::Focus | editor::Update::InputMethod => shell.request_redraw(),
            editor::Update::Unfocus => {
                self.commit(state, shell);
                shell.request_redraw();
                if let Some(message) = self.on_unfocus.clone() {
                    shell.publish(message);
                }
            }
            editor::Update::Release => {}
        }
    }
}

struct OperationFocus<'a> {
    state: &'a mut editor::State,
    operation_unfocus: &'a mut bool,
}

impl Focusable for OperationFocus<'_> {
    fn is_focused(&self) -> bool {
        self.state.is_focused()
    }

    fn focus(&mut self) {
        self.state.focus();
    }

    fn unfocus(&mut self) {
        if self.state.is_focused() {
            self.state.unfocus();
            *self.operation_unfocus = true;
        }
    }
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for Field<'_, Message> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<State>()
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: Length::Shrink,
        }
    }

    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<State>();
        let text_size = self.text_size(renderer);
        let line_height = f32::from(LineHeight::default().to_absolute(text_size));
        let limits = limits.width(self.width);
        let inner = limits.shrink(self.padding).max();
        let text_bounds = Size::new(inner.width, line_height);

        state.adopt(self.value);
        state.editor.update(
            text_bounds,
            self.font(renderer),
            text_size,
            LineHeight::default(),
            Wrapping::None,
            text::Alignment::Default,
            renderer.hint_factor(),
            &mut text::highlighter::PlainText,
        );
        let _ = state
            .placeholder
            .update(self.placeholder_text(renderer, text_bounds));

        let node = layout::Node::new(
            limits
                .height(line_height + self.padding.y())
                .max()
                .expand(Size::ZERO),
        );
        state.origin = Vector::new(self.padding.left, self.padding.top);
        node
    }

    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let backend = jfn_platform_abi::get().display();
        let state = tree.state.downcast_mut::<State>();
        state.bounds = layout.bounds();
        if state.take_operation_unfocus() {
            self.apply_update(state, editor::Update::Unfocus, shell);
        }
        if self.on_input.is_none() {
            return;
        }
        let State { editor, focus, .. } = state;
        let update = focus.update(
            &*editor,
            event,
            layout.bounds(),
            self.padding,
            cursor,
            |key_press| binding(backend, key_press),
        );
        if let Some(update) = update {
            self.apply_update(state, update, shell);
        }
        if matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
            self.commit(state, shell);
            shell.request_input_method(&state.focus.input_method(
                &state.editor,
                layout.bounds().shrink(self.padding).position(),
            ));
        }
    }

    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _defaults: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        let style = crate::theme::field_style(state.focus.is_focused());
        let bounds = layout.bounds();

        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: style.border,
                ..renderer::Quad::default()
            },
            style.background,
        );

        let text_bounds = bounds.shrink(self.padding);
        if state.editor.is_empty() && !self.placeholder.is_empty() {
            renderer.fill_paragraph(
                state.placeholder.raw(),
                text_bounds.position(),
                style.placeholder,
                text_bounds,
            );
        }
        state.focus.draw(
            &state.editor,
            renderer,
            text_bounds.position(),
            *viewport,
            editor::Style {
                value: style.value,
                selection: style.selection,
            },
        );
    }

    fn mouse_interaction(
        &self,
        _tree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) && self.on_input.is_some() {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::default()
        }
    }

    fn operate(
        &mut self,
        tree: &mut widget::Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        let state = tree.state.downcast_mut::<State>();
        state.bounds = layout.bounds();
        {
            let mut focus = OperationFocus {
                state: &mut state.focus,
                operation_unfocus: &mut state.operation_unfocus,
            };
            operation.focusable(Some(&self.id), layout.bounds(), &mut focus);
        }
        operation.custom(Some(&self.id), layout.bounds(), state as &mut dyn Any);
    }
}

impl<'a, Message: Clone + 'a> From<Field<'a, Message>> for Element<'a, Message, Theme, Renderer> {
    fn from(field: Field<'a, Message>) -> Self {
        Element::new(field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_core::keyboard::key::{Code, Named, NativeCode, Physical};
    use iced_core::keyboard::{Key, Modifiers};
    use iced_core::text::editor::Motion;

    use crate::fields::Snapshot;

    /// The modifier the platform's own shortcuts are held with, as iced
    /// resolves it: Command on macOS, Ctrl everywhere else.
    const COMMAND: Modifiers = Modifiers::COMMAND;

    const BACKENDS: [DisplayBackend; 4] = [
        DisplayBackend::Wayland,
        DisplayBackend::X11,
        DisplayBackend::Windows,
        DisplayBackend::MacOS,
    ];

    fn chord(key: Key, modifiers: Modifiers) -> KeyPress {
        KeyPress {
            key: key.clone(),
            modified_key: key,
            physical_key: Physical::Unidentified(NativeCode::Unidentified),
            modifiers,
            text: None,
            is_focused: true,
        }
    }

    fn character(c: char, modifiers: Modifiers) -> KeyPress {
        chord(Key::Character(c.to_string().into()), modifiers)
    }

    fn named(name: Named, modifiers: Modifiers) -> KeyPress {
        chord(Key::Named(name), modifiers)
    }

    fn bind(backend: DisplayBackend, key_press: KeyPress) -> Option<Binding<()>> {
        binding(backend, key_press)
    }

    fn layout_empty(state: &mut State) {
        state.editor.update(
            Size::new(200.0, 20.0),
            Font::DEFAULT,
            Pixels(16.0),
            LineHeight::default(),
            Wrapping::None,
            text::Alignment::Default,
            Some(1.0),
            &mut text::highlighter::PlainText,
        );
    }

    #[derive(Clone, PartialEq, Debug)]
    enum Message {
        Input(String),
        Unfocus,
    }

    fn operation_unfocus_messages() -> Vec<Message> {
        let field = field(Id::unique(), "", "")
            .on_input(Message::Input)
            .on_unfocus(Message::Unfocus);
        let mut state = State::default();
        state.focus.focus();
        state.edit(Edit::Insert('x'));
        let mut operation_focus = OperationFocus {
            state: &mut state.focus,
            operation_unfocus: &mut state.operation_unfocus,
        };
        operation_focus.unfocus();
        assert!(state.take_operation_unfocus());

        let mut messages = iced_core::shell::Bus::new();
        let waker = iced_core::shell::Waker::new(|| {});
        let mut shell = Shell::new(&window::Headless, waker, &mut messages);
        field.apply_update(&mut state, editor::Update::Unfocus, &mut shell);
        drop(shell);
        messages.drain().collect()
    }

    #[test]
    fn operation_unfocus_commits_input_before_the_configured_unfocus_message() {
        assert_eq!(
            operation_unfocus_messages(),
            vec![Message::Input("x".into()), Message::Unfocus]
        );
    }

    #[test]
    fn an_empty_default_state_has_layout_caret_and_selection_state() {
        let mut state = State::default();
        layout_empty(&mut state);

        assert_eq!(state.caret(), Point::ORIGIN);
        assert!(state.selection_bounds().is_empty());
        assert_eq!(state.selection(), None);
    }

    #[test]
    fn an_empty_laid_out_state_can_be_snapshotted_focused() {
        let mut state = State::default();
        layout_empty(&mut state);
        state.focus.focus();

        let bounds = Rectangle::new(Point::new(10.0, 20.0), Size::new(200.0, 20.0));
        let snapshot = Snapshot::of(Id::unique(), bounds, &state);

        assert!(snapshot.focused);
        assert!(snapshot.empty);
        assert_eq!(snapshot.caret, bounds.position());
        assert!(snapshot.selection_bounds.is_empty());
    }

    #[test]
    fn escape_resolves_to_no_binding() {
        for backend in BACKENDS {
            assert_eq!(
                bind(backend, named(Named::Escape, Modifiers::empty())),
                None
            );
        }
    }

    #[test]
    fn an_unfocused_field_resolves_nothing() {
        let mut key_press = character('c', COMMAND);
        key_press.is_focused = false;
        for backend in BACKENDS {
            assert_eq!(bind(backend, key_press.clone()), None);
        }
    }

    #[test]
    fn the_clipboard_chords_resolve_on_every_backend() {
        for backend in BACKENDS {
            assert_eq!(bind(backend, character('c', COMMAND)), Some(Binding::Copy));
            assert_eq!(bind(backend, character('x', COMMAND)), Some(Binding::Cut));
            assert_eq!(bind(backend, character('v', COMMAND)), Some(Binding::Paste));
            assert_eq!(
                bind(backend, character('a', COMMAND)),
                Some(Binding::SelectAll)
            );
            assert_eq!(bind(backend, character('z', COMMAND)), Some(Binding::Undo));
        }
    }

    #[test]
    fn a_non_latin_layout_resolves_from_the_physical_key() {
        let mut key_press = character('\u{0441}', COMMAND);
        key_press.physical_key = Physical::Code(Code::KeyC);
        for backend in BACKENDS {
            assert_eq!(bind(backend, key_press.clone()), Some(Binding::Copy));
        }
    }

    #[test]
    fn redo_binds_to_each_platforms_own_chord() {
        for backend in [DisplayBackend::Wayland, DisplayBackend::X11] {
            assert_eq!(bind(backend, character('y', COMMAND)), Some(Binding::Redo));
            assert_eq!(
                bind(backend, character('z', COMMAND | Modifiers::SHIFT)),
                Some(Binding::Redo)
            );
        }
        assert_eq!(
            bind(DisplayBackend::Windows, character('y', COMMAND)),
            Some(Binding::Redo)
        );
        assert_eq!(
            bind(
                DisplayBackend::Windows,
                character('z', COMMAND | Modifiers::SHIFT)
            ),
            Some(Binding::Undo)
        );
        assert_eq!(
            bind(
                DisplayBackend::MacOS,
                character('z', COMMAND | Modifiers::SHIFT)
            ),
            Some(Binding::Redo)
        );
    }

    #[test]
    fn the_caret_and_selection_chords_resolve_on_every_backend() {
        for backend in BACKENDS {
            assert_eq!(
                bind(backend, named(Named::Home, Modifiers::empty())),
                Some(Binding::Move(Motion::Home))
            );
            assert_eq!(
                bind(backend, named(Named::End, Modifiers::empty())),
                Some(Binding::Move(Motion::End))
            );
            assert_eq!(
                bind(backend, named(Named::Home, Modifiers::SHIFT)),
                Some(Binding::Select(Motion::Home))
            );
            assert_eq!(
                bind(backend, named(Named::End, Modifiers::SHIFT)),
                Some(Binding::Select(Motion::End))
            );
            assert_eq!(
                bind(backend, named(Named::ArrowLeft, Modifiers::SHIFT)),
                Some(Binding::Select(Motion::Left))
            );
            assert_eq!(
                bind(backend, named(Named::ArrowRight, Modifiers::SHIFT)),
                Some(Binding::Select(Motion::Right))
            );
        }
    }
}
