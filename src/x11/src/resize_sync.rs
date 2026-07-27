//! Resize-sync completion: fact-based, isolated from the counter protocol.
//!
//! The WM drives interactive resize with `_NET_WM_SYNC_REQUEST`: it hands us a
//! counter value, resizes us, then stalls until we write that value back. We must
//! answer only once our surfaces have actually reached the new size.
//!
//! Completion is read from ground truth — are the overlays the right size? — not
//! waited on as an event. [`ResizeSync`] is the outstanding obligation; the
//! counter itself lives behind [`ResizeAckSignal`] so neither the obligation nor
//! the recorded sizes know the protocol.

/// How to tell the WM a resize completed. The implementor owns whatever protocol
/// state the ack needs; `acknowledge` consumes it, so an ack can fire only once.
pub trait ResizeAckSignal {
    fn acknowledge(self: Box<Self>);
}

/// The sole holder of the `_NET_WM_SYNC_REQUEST` counter. Writing the requested
/// value into the XSync counter is the basic resize contract — not the extended
/// `_NET_WM_FRAME_DRAWN` timing protocol.
pub struct SyncCounterAck {
    pub counter: u32,
    pub hi: i32,
    pub lo: u32,
}

impl ResizeAckSignal for SyncCounterAck {
    fn acknowledge(self: Box<Self>) {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::sync::{ConnectionExt as _, Int64};
        // Set the counter on the geometry/top-level connection — the same one the
        // overlay configures were flushed on — so the WM sees a consistent order.
        let Some(conn) = crate::geometry::toplevel_conn() else {
            return;
        };
        let _ = conn.sync_set_counter(
            self.counter,
            Int64 {
                hi: self.hi,
                lo: self.lo,
            },
        );
        let _ = conn.flush();
    }
}

/// An outstanding resize obligation. `target` is `None` until the parent's resize
/// `ConfigureNotify` lands and names the size the overlays must reach, so acking
/// against pre-resize geometry is impossible by construction.
pub struct ResizeSync {
    pub signal: Box<dyn ResizeAckSignal>,
    pub target: Option<(i32, i32)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyncState {
    Idle,
    Waiting,
    Acked,
}

/// True once every participating overlay sits at `target`. A non-participating
/// (hidden/unmapped) overlay never gates; an already-at-target overlay settles
/// without any further `ConfigureNotify`, which is what makes a no-op place safe.
pub fn all_settled(
    overlays: impl IntoIterator<Item = (bool, Option<(i32, i32)>)>,
    target: (i32, i32),
) -> bool {
    overlays
        .into_iter()
        .all(|(participating, size)| !participating || size == Some(target))
}

/// Advance an outstanding obligation. Acks (consuming the signal) exactly when a
/// target is known and `settled` holds; otherwise reports where it is stuck. Only
/// `Waiting` is a live obligation — `Idle`/`Acked` need no further action.
pub fn drive(sync: &mut Option<ResizeSync>, settled: bool) -> SyncState {
    match sync.as_ref() {
        None => SyncState::Idle,
        Some(rs) if rs.target.is_none() => SyncState::Waiting,
        Some(_) if !settled => SyncState::Waiting,
        Some(_) => {
            if let Some(rs) = sync.take() {
                rs.signal.acknowledge();
            }
            SyncState::Acked
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct FakeSignal(Rc<Cell<u32>>);
    impl ResizeAckSignal for FakeSignal {
        fn acknowledge(self: Box<Self>) {
            self.0.set(self.0.get() + 1);
        }
    }

    fn sync_with(target: Option<(i32, i32)>, calls: &Rc<Cell<u32>>) -> Option<ResizeSync> {
        Some(ResizeSync {
            signal: Box::new(FakeSignal(calls.clone())),
            target,
        })
    }

    const T: (i32, i32) = (800, 600);

    #[test]
    fn none_is_idle() {
        let mut s: Option<ResizeSync> = None;
        assert_eq!(drive(&mut s, true), SyncState::Idle);
    }

    #[test]
    fn no_target_waits_and_never_acks() {
        let calls = Rc::new(Cell::new(0));
        let mut s = sync_with(None, &calls);
        assert_eq!(drive(&mut s, true), SyncState::Waiting);
        assert_eq!(calls.get(), 0);
        assert!(s.is_some());
    }

    #[test]
    fn unsettled_target_waits() {
        let calls = Rc::new(Cell::new(0));
        let mut s = sync_with(Some(T), &calls);
        assert_eq!(drive(&mut s, false), SyncState::Waiting);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn settled_target_acks_exactly_once() {
        let calls = Rc::new(Cell::new(0));
        let mut s = sync_with(Some(T), &calls);
        assert_eq!(drive(&mut s, true), SyncState::Acked);
        assert_eq!(calls.get(), 1);
        // Obligation consumed: a second drive is Idle and cannot ack again.
        assert_eq!(drive(&mut s, true), SyncState::Idle);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn supersede_replaces_prior_obligation() {
        let old = Rc::new(Cell::new(0));
        let mut s = sync_with(Some((100, 100)), &old);
        // A newer resize request replaces the old obligation entirely.
        let new = Rc::new(Cell::new(0));
        s.replace(ResizeSync {
            signal: Box::new(FakeSignal(new.clone())),
            target: Some(T),
        });
        assert_eq!(drive(&mut s, true), SyncState::Acked);
        assert_eq!(old.get(), 0);
        assert_eq!(new.get(), 1);
    }

    #[test]
    fn all_at_target_settles() {
        assert!(all_settled([(true, Some(T)), (true, Some(T))], T));
    }

    // No-op place emits no ConfigureNotify, but the recorded size already equals
    // target, so it settles with no wait — the freeze regression.
    #[test]
    fn overlay_already_at_target_settles() {
        assert!(all_settled([(true, Some(T))], T));
    }

    #[test]
    fn overlay_at_old_size_gates() {
        assert!(!all_settled([(true, Some((640, 480))), (true, Some(T))], T));
    }

    #[test]
    fn non_participating_never_gates() {
        assert!(all_settled([(false, None), (false, Some((1, 1)))], T));
    }

    #[test]
    fn missing_size_gates_while_participating() {
        assert!(!all_settled([(true, None)], T));
    }
}
