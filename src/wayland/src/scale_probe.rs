use std::env;
use std::num::NonZeroU32;

use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_output;
use wayland_client::{Connection, QueueHandle};

use jfn_platform_abi::WindowPos;

use crate::scale::Scale120;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ScaleProbeError {
    #[error("no Wayland session")]
    NoWaylandSession,
    #[error("probe connection failed")]
    Connection,
    #[error("no usable output")]
    NoUsableOutput,
    #[error("probe timed out")]
    Timeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeTarget {
    Point { x: i32, y: i32 },
    FirstOutput,
}

impl ProbeTarget {
    pub(crate) fn at(position: Option<WindowPos>) -> ProbeTarget {
        match position {
            Some(p) => ProbeTarget::Point { x: p.x, y: p.y },
            None => ProbeTarget::FirstOutput,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputCandidate {
    logical_pos: (i32, i32),
    logical_size: (i32, i32),
    mode: (i32, i32),
    swaps_axes: bool,
}

impl OutputCandidate {
    pub(crate) fn new(
        logical_pos: (i32, i32),
        logical_size: (i32, i32),
        mode: (i32, i32),
        swaps_axes: bool,
    ) -> Self {
        Self {
            logical_pos,
            logical_size,
            mode,
            swaps_axes,
        }
    }

    fn contains(&self, x: i32, y: i32) -> bool {
        let (lx, ly) = self.logical_pos;
        let (lw, lh) = self.logical_size;
        x >= lx && x < lx.saturating_add(lw) && y >= ly && y < ly.saturating_add(lh)
    }

    fn scale(&self) -> Option<Scale120> {
        let physical_w = if self.swaps_axes {
            self.mode.1
        } else {
            self.mode.0
        };
        let physical_w = u32::try_from(physical_w).ok()?;
        let logical_w = NonZeroU32::new(u32::try_from(self.logical_size.0).ok()?)?;
        Scale120::from_physical_logical(physical_w, logical_w)
    }
}

pub(crate) fn select_scale(
    outputs: &[OutputCandidate],
    target: ProbeTarget,
) -> Result<Scale120, ScaleProbeError> {
    if let ProbeTarget::Point { x, y } = target
        && let Some(scale) = outputs
            .iter()
            .filter(|o| o.contains(x, y))
            .find_map(OutputCandidate::scale)
    {
        return Ok(scale);
    }
    outputs
        .iter()
        .find_map(OutputCandidate::scale)
        .ok_or(ScaleProbeError::NoUsableOutput)
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_dispatch2!(State);
delegate_registry!(State);

fn transform_swaps_axes(t: wl_output::Transform) -> bool {
    matches!(
        t,
        wl_output::Transform::_90
            | wl_output::Transform::_270
            | wl_output::Transform::Flipped90
            | wl_output::Transform::Flipped270
    )
}

fn collect_candidates() -> Result<Vec<OutputCandidate>, ScaleProbeError> {
    if env::var_os("WAYLAND_DISPLAY").is_none() && env::var_os("WAYLAND_SOCKET").is_none() {
        return Err(ScaleProbeError::NoWaylandSession);
    }

    let conn = Connection::connect_to_env().map_err(|_| ScaleProbeError::Connection)?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|_| ScaleProbeError::Connection)?;
    let qh = queue.handle();

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
    };

    queue
        .roundtrip(&mut state)
        .map_err(|_| ScaleProbeError::Connection)?;
    queue
        .roundtrip(&mut state)
        .map_err(|_| ScaleProbeError::Connection)?;

    let mut candidates = Vec::new();
    for output in state.output_state.outputs() {
        let Some(info) = state.output_state.info(&output) else {
            continue;
        };
        let (Some(pos), Some(size)) = (info.logical_position, info.logical_size) else {
            continue;
        };
        let Some(mode) = info
            .modes
            .iter()
            .find(|m| m.current)
            .or_else(|| info.modes.first())
        else {
            continue;
        };
        candidates.push(OutputCandidate::new(
            pos,
            size,
            mode.dimensions,
            transform_swaps_axes(info.transform),
        ));
    }
    Ok(candidates)
}

pub(crate) fn probe_scale(target: ProbeTarget) -> Result<Scale120, ScaleProbeError> {
    select_scale(&collect_candidates()?, target)
}

pub(crate) fn probe_scale_bounded(
    target: ProbeTarget,
    timeout: std::time::Duration,
) -> Result<Scale120, ScaleProbeError> {
    let (tx, rx) = crossbeam_channel::bounded::<Result<Scale120, ScaleProbeError>>(1);
    std::thread::Builder::new()
        .name("wl-scale-probe".into())
        .spawn(move || {
            let _ = tx.send(probe_scale(target));
        })
        .map_err(|_| ScaleProbeError::Connection)?;
    rx.recv_timeout(timeout)
        .map_err(|_| ScaleProbeError::Timeout)?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn landscape() -> OutputCandidate {
        OutputCandidate::new((0, 0), (2560, 1440), (3840, 2160), false)
    }

    fn portrait() -> OutputCandidate {
        OutputCandidate::new((2560, 0), (1440, 2560), (3840, 2160), true)
    }

    fn scale_of(r: Result<Scale120, ScaleProbeError>) -> Option<f64> {
        r.ok().map(|s| s.scale().as_f64())
    }

    const ONE_AND_A_HALF: Option<f64> = Some(1.5);

    #[test]
    fn first_output_uses_first_usable() {
        assert_eq!(
            scale_of(select_scale(
                &[landscape(), portrait()],
                ProbeTarget::FirstOutput
            )),
            ONE_AND_A_HALF
        );
    }

    #[test]
    fn point_selects_containing_output() {
        let outs = [landscape(), portrait()];
        assert_eq!(
            scale_of(select_scale(&outs, ProbeTarget::Point { x: 100, y: 100 })),
            ONE_AND_A_HALF
        );
        assert_eq!(
            scale_of(select_scale(&outs, ProbeTarget::Point { x: 2560, y: 0 })),
            ONE_AND_A_HALF
        );
    }

    #[test]
    fn rotated_output_uses_swapped_mode_axis() {
        assert_eq!(
            scale_of(select_scale(&[portrait()], ProbeTarget::FirstOutput)),
            ONE_AND_A_HALF
        );
    }

    #[test]
    fn point_outside_every_output_falls_back_to_first() {
        assert_eq!(
            scale_of(select_scale(
                &[landscape()],
                ProbeTarget::Point { x: -5000, y: -5000 }
            )),
            ONE_AND_A_HALF
        );
    }

    #[test]
    fn no_outputs_is_an_error() {
        assert_eq!(
            select_scale(&[], ProbeTarget::FirstOutput),
            Err(ScaleProbeError::NoUsableOutput)
        );
    }

    #[test]
    fn degenerate_geometry_is_skipped_not_divided_by() {
        let zero_logical = OutputCandidate::new((0, 0), (0, 0), (3840, 2160), false);
        let negative_mode = OutputCandidate::new((0, 0), (2560, 1440), (-1, -1), false);
        assert_eq!(
            select_scale(&[zero_logical, negative_mode], ProbeTarget::FirstOutput),
            Err(ScaleProbeError::NoUsableOutput)
        );
        assert_eq!(
            scale_of(select_scale(
                &[zero_logical, landscape()],
                ProbeTarget::FirstOutput
            )),
            ONE_AND_A_HALF
        );
    }

    #[test]
    fn unusable_containing_output_falls_back() {
        let broken_at_origin = OutputCandidate::new((0, 0), (2560, 1440), (0, 0), false);
        assert_eq!(
            scale_of(select_scale(
                &[broken_at_origin, portrait()],
                ProbeTarget::Point { x: 10, y: 10 }
            )),
            ONE_AND_A_HALF
        );
    }

    #[test]
    fn huge_extents_do_not_overflow_containment() {
        let o = OutputCandidate::new((i32::MAX - 10, 0), (i32::MAX, 100), (100, 100), false);
        assert!(o.contains(i32::MAX - 1, 50));
    }
}
