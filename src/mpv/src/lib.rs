#![warn(unsafe_op_in_unsafe_fn)]

pub mod sys;

mod command;
mod error;
mod event;
mod event_loop;
mod handle;
mod log;
mod node;
mod property;

pub mod api;
pub mod boot;
pub mod capabilities;
pub mod color;
pub mod probe;

pub use command::Command;
pub use error::{Error, Result};
pub use event::{EndFileReason, Event, LogMessage, ObserveId, PropertyValue, ReplyUserdata};
pub use event_loop::EventLoop;
pub use handle::{Handle, WakeupCallback};
pub use log::{LogLevel, forward_to_tracing as forward_log_to_tracing};
pub use node::{Node, NodeArray, NodeMap};
pub use property::Format;
