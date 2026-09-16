pub mod sink;

use crate::wl_state::WlState;
use sink::SceneSink;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct LayerId(pub usize);

#[derive(Default)]
pub struct Scene {
    order: Vec<LayerId>,
    applied: Vec<LayerId>,
}

pub enum SceneEvent {
    LayerAdded(LayerId),
    LayerRemoved(LayerId),
    Order(Vec<LayerId>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    PlaceAbove { layer: LayerId, below: LayerId },
    PinVideoBottom,
    CommitParent,
}

impl Scene {
    fn has(&self, id: LayerId) -> bool {
        self.order.contains(&id)
    }

    fn restack_effects(&mut self) -> Vec<Effect> {
        if self.order == self.applied {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.order.len() + 2);
        let mut prev: Option<LayerId> = None;
        for &id in &self.order {
            match prev {
                None => out.push(Effect::PinVideoBottom),
                Some(below) => out.push(Effect::PlaceAbove { layer: id, below }),
            }
            prev = Some(id);
        }
        if !out.is_empty() {
            out.push(Effect::CommitParent);
        }
        self.applied = self.order.clone();
        out
    }
}

pub fn reduce(scene: &mut Scene, ev: SceneEvent) -> Vec<Effect> {
    match ev {
        SceneEvent::LayerAdded(id) => {
            if !scene.has(id) {
                scene.order.push(id);
            }
            scene.restack_effects()
        }
        SceneEvent::LayerRemoved(id) => {
            scene.order.retain(|&l| l != id);
            scene.restack_effects()
        }
        SceneEvent::Order(order) => {
            let mut next: Vec<LayerId> = order.into_iter().filter(|id| scene.has(*id)).collect();
            for id in &scene.order {
                if !next.contains(id) {
                    next.push(*id);
                }
            }
            scene.order = next;
            scene.restack_effects()
        }
    }
}

pub(crate) fn dispatch(rt: &'static crate::runtime::WlRuntime, st: &mut WlState, ev: SceneEvent) {
    let effects = reduce(&mut st.scene, ev);
    {
        let mut s = sink::WlSink::new(rt);
        for e in &effects {
            s.apply(e);
        }
    }
    st.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: LayerId = LayerId(1);
    const ABOUT: LayerId = LayerId(2);

    fn add(scene: &mut Scene, id: LayerId) -> Vec<Effect> {
        reduce(scene, SceneEvent::LayerAdded(id))
    }

    #[test]
    fn add_layer_pins_video_below_it_and_commits() {
        let mut s = Scene::default();
        let e = add(&mut s, MAIN);
        assert_eq!(e, vec![Effect::PinVideoBottom, Effect::CommitParent]);
    }

    #[test]
    fn second_layer_stacks_above_first_then_commits() {
        let mut s = Scene::default();
        add(&mut s, MAIN);
        let e = add(&mut s, ABOUT);
        assert_eq!(
            e,
            vec![
                Effect::PinVideoBottom,
                Effect::PlaceAbove {
                    layer: ABOUT,
                    below: MAIN
                },
                Effect::CommitParent,
            ]
        );
    }

    #[test]
    fn every_order_change_ends_in_single_commit_parent() {
        let mut s = Scene::default();
        for ev in [
            SceneEvent::LayerAdded(MAIN),
            SceneEvent::LayerAdded(ABOUT),
            SceneEvent::Order(vec![ABOUT, MAIN]),
            SceneEvent::LayerRemoved(ABOUT),
        ] {
            let e = reduce(&mut s, ev);
            let commits = e.iter().filter(|x| **x == Effect::CommitParent).count();
            assert_eq!(commits, 1, "expected exactly one CommitParent, got {e:?}");
            assert_eq!(e.last(), Some(&Effect::CommitParent));
        }
    }

    #[test]
    fn order_matching_the_applied_one_is_noop() {
        let mut s = Scene::default();
        add(&mut s, MAIN);
        add(&mut s, ABOUT);
        let e = reduce(&mut s, SceneEvent::Order(vec![MAIN, ABOUT]));
        assert_eq!(e, vec![]);
    }

    #[test]
    fn order_reorders_and_commits() {
        let mut s = Scene::default();
        add(&mut s, MAIN);
        add(&mut s, ABOUT);
        let e = reduce(&mut s, SceneEvent::Order(vec![ABOUT, MAIN]));
        assert_eq!(
            e,
            vec![
                Effect::PinVideoBottom,
                Effect::PlaceAbove {
                    layer: MAIN,
                    below: ABOUT
                },
                Effect::CommitParent,
            ]
        );
    }
}
