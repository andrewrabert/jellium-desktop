use std::any::Any;

use iced_core::widget::operation::Focusable;
use iced_core::widget::{Id, Operation};
use iced_core::{Point, Rectangle};
use iced_runtime::user_interface::UserInterface;
use jfn_platform_abi::DisplayBackend;

use crate::shell::field::{Act, State};
use crate::shell::theme::Theme;

#[derive(Clone, PartialEq, Debug)]
pub struct Snapshot {
    pub id: Id,
    pub bounds: Rectangle,
    pub focused: bool,
    pub selection: Option<String>,
    pub empty: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub caret: Point,
    pub selection_bounds: Vec<Rectangle>,
    pub selection_generation: u64,
}

impl Snapshot {
    pub(crate) fn of(id: Id, bounds: Rectangle, state: &State) -> Snapshot {
        let origin = state.origin();
        let translation = iced_core::Vector::new(bounds.x + origin.x, bounds.y + origin.y);
        Snapshot {
            id,
            bounds,
            focused: state.is_focused(),
            selection: state.selection(),
            empty: state.is_empty(),
            can_undo: state.can_undo(),
            can_redo: state.can_redo(),
            caret: state.caret() + translation,
            selection_bounds: state
                .selection_bounds()
                .into_iter()
                .map(|rect| rect + translation)
                .collect(),
            selection_generation: state.selection_generation(),
        }
    }

    pub fn is_over_selection(&self, at: Point) -> bool {
        self.selection_bounds.iter().any(|rect| rect.contains(at))
    }

    pub fn edit_state(&self) -> jfn_input::FieldEdit {
        let has_selection = self.selection.is_some();
        jfn_input::FieldEdit {
            undo: self.can_undo,
            redo: self.can_redo,
            cut: has_selection,
            copy: has_selection,
            select_all: !self.empty,
        }
    }
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Fields(Vec<Snapshot>);

impl Fields {
    pub fn collect<Message>(
        ui: &mut UserInterface<'_, Message, Theme, iced_wgpu::Renderer>,
        renderer: &iced_wgpu::Renderer,
    ) -> Fields {
        let mut collect = Collect(Vec::new());
        ui.operate(renderer, &mut collect);
        Fields(collect.0)
    }

    pub fn at(&self, at: Point) -> Option<&Snapshot> {
        self.0.iter().rev().find(|field| field.bounds.contains(at))
    }

    pub fn focused(&self) -> Option<&Snapshot> {
        self.0.iter().find(|field| field.focused)
    }

    pub fn named(&self, id: &Id) -> Option<&Snapshot> {
        self.0.iter().find(|field| &field.id == id)
    }
}

struct Collect(Vec<Snapshot>);

impl Operation for Collect {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn custom(&mut self, id: Option<&Id>, bounds: Rectangle, state: &mut dyn Any) {
        let (Some(id), Some(state)) = (id, state.downcast_ref::<State>()) else {
            return;
        };
        self.0.push(Snapshot::of(id.clone(), bounds, state));
    }
}

pub struct Apply {
    id: Id,
    focus: bool,
    act: Option<Act>,
    produced: Option<String>,
}

impl Apply {
    pub fn act(id: Id, act: Act) -> Apply {
        Apply {
            id,
            focus: false,
            act: Some(act),
            produced: None,
        }
    }

    pub fn focus(id: Id, act: Option<Act>) -> Apply {
        Apply {
            id,
            focus: true,
            act,
            produced: None,
        }
    }

    pub fn produced(&self) -> Option<&str> {
        self.produced.as_deref()
    }
}

impl Operation for Apply {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn focusable(&mut self, id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Focusable) {
        if !self.focus {
            return;
        }
        if id == Some(&self.id) {
            state.focus();
        } else {
            state.unfocus();
        }
    }

    fn custom(&mut self, id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Any) {
        if id != Some(&self.id) {
            return;
        }
        let Some(state) = state.downcast_mut::<State>() else {
            return;
        };
        let Apply { act, produced, .. } = self;
        let Some(act) = act.as_ref() else {
            return;
        };
        *produced = state.act(act);
    }
}

pub fn press_focuses(backend: DisplayBackend) -> bool {
    matches!(backend, DisplayBackend::Windows | DisplayBackend::MacOS)
}

pub fn press_caret(backend: DisplayBackend, field: &Snapshot, at: Point) -> Option<Act> {
    if field.is_over_selection(at) {
        return None;
    }
    match backend {
        DisplayBackend::Windows => Some(Act::PlaceCaret(at)),
        DisplayBackend::MacOS => Some(Act::SelectWord(at)),
        DisplayBackend::Wayland | DisplayBackend::X11 => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_windows_and_macos_focus_the_field_a_right_press_lands_in() {
        assert!(press_focuses(DisplayBackend::Windows));
        assert!(press_focuses(DisplayBackend::MacOS));
        assert!(!press_focuses(DisplayBackend::Wayland));
        assert!(!press_focuses(DisplayBackend::X11));
    }
}
