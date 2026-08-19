//! Effect interpreters for [`super::reduce`].

use wayland_client::protocol::wl_surface::WlSurface;

use super::{Effect, LayerId};
use crate::wl_state::PlatformSurface;

pub trait SceneSink {
    fn apply(&mut self, effect: &Effect);
}

#[cfg(test)]
#[derive(Default)]
pub struct RecordingSink {
    pub effects: Vec<Effect>,
}

#[cfg(test)]
impl SceneSink for RecordingSink {
    fn apply(&mut self, effect: &Effect) {
        self.effects.push(*effect);
    }
}

fn layer_ptr(id: LayerId) -> *mut PlatformSurface {
    id.0 as *mut PlatformSurface
}

// The synchronized subsurface stays owned by its PlatformSurface (the raw object
// never escapes); only the sibling surface handle is cloned out for restacking.
fn layer_surface(id: LayerId) -> Option<WlSurface> {
    let p = layer_ptr(id);
    if p.is_null() {
        return None;
    }
    // SAFETY: LayerId is a live PlatformSurface address (removed from the scene
    // before the box is freed), dereferenced only under the wl_state lock.
    let s = unsafe { &*p };
    s.surface.as_ref().map(|sr| sr.as_arg().clone())
}

pub struct WlSink {
    rt: &'static crate::runtime::WlRuntime,
}

impl WlSink {
    pub fn new(rt: &'static crate::runtime::WlRuntime) -> Self {
        Self { rt }
    }

    fn place_above(&mut self, layer: LayerId, below: LayerId) {
        let p = layer_ptr(layer);
        if p.is_null() {
            return;
        }
        // SAFETY: see `layer_surface` — live address, accessed under the lock.
        let s = unsafe { &*p };
        let Some(sub) = s.subsurface.as_ref() else {
            return;
        };
        if let Some(surf) = layer_surface(below) {
            sub.place_above(&surf);
        }
    }

    fn pin_video_bottom(&mut self) {
        self.rt.proxy().pin_video_bottom();
    }
}

impl SceneSink for WlSink {
    fn apply(&mut self, effect: &Effect) {
        match *effect {
            Effect::PlaceAbove { layer, below } => self.place_above(layer, below),
            Effect::PinVideoBottom => self.pin_video_bottom(),
            Effect::CommitParent => self.rt.root().request_present(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Effect, LayerId, Scene, SceneEvent, reduce};
    use super::{RecordingSink, SceneSink};

    #[test]
    fn recording_sink_captures_add_sequence() {
        let mut scene = Scene::default();
        let mut sink = RecordingSink::default();
        for ev in [
            SceneEvent::LayerAdded(LayerId(1)),
            SceneEvent::LayerAdded(LayerId(2)),
        ] {
            for e in reduce(&mut scene, ev) {
                sink.apply(&e);
            }
        }
        assert_eq!(
            sink.effects,
            vec![
                Effect::PinVideoBottom,
                Effect::CommitParent,
                Effect::PinVideoBottom,
                Effect::PlaceAbove {
                    layer: LayerId(2),
                    below: LayerId(1)
                },
                Effect::CommitParent,
            ]
        );
    }
}
