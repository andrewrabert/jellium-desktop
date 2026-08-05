//! Pure, host-testable compositor bookkeeping for the backends that cannot be
//! built on a Linux dev machine: the macOS (`CAMetalLayer`) surface registry
//! and the resize-transition gate macOS and X11 share.
//!
//! This crate holds that logic as plain value types with no atomics, locks,
//! or OS calls — the OS-bound compositor stores these inside its own
//! `Mutex`/`AtomicBool` and drives the GPU itself. Each type documents which
//! entry points each platform uses; the tests pin their exact behavior.

pub mod stack;
pub mod transition;
