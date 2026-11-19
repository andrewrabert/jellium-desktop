use std::ffi::c_int;

use cef::{Task, ThreadId, post_delayed_task, post_task};

const REJECTED: c_int = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("CEF's UI thread rejected the task")]
pub struct UiTaskRejected;

pub(crate) fn post_ui(task: &mut Task) -> Result<(), UiTaskRejected> {
    accepted(post_task(ThreadId::UI, Some(task)))
}

pub(crate) fn post_ui_delayed(task: &mut Task, delay_ms: i64) -> Result<(), UiTaskRejected> {
    accepted(post_delayed_task(ThreadId::UI, Some(task), delay_ms))
}

fn accepted(posted: c_int) -> Result<(), UiTaskRejected> {
    if posted == REJECTED {
        Err(UiTaskRejected)
    } else {
        Ok(())
    }
}
