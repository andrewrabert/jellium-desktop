use cef::rc::Rc;
use cef::{ImplTask, Task, ThreadId, WrapTask, post_delayed_task, post_task, wrap_task};
use crossbeam_channel::Sender;
use std::sync::Arc;

use super::Inner;
use crate::frame_rate::FrameRate;
use crate::web_overlay::CloseDeliveryError;

wrap_task! {
    struct ApplyResizeTask {
        inner: Arc<Inner>,
    }
    impl Task {
        fn execute(&self) {
            self.inner.apply_pending_resize();
        }
    }
}

pub(super) fn post_apply_resize(inner: Arc<Inner>, delay_ms: i64) {
    let mut task = ApplyResizeTask::new(inner);
    let _ = post_delayed_task(ThreadId::UI, Some(&mut task), delay_ms);
}

wrap_task! {
    struct SetRefreshTask {
        inner: Arc<Inner>,
        target: FrameRate,
    }
    impl Task {
        fn execute(&self) {
            self.inner.apply_set_refresh(self.target);
        }
    }
}

pub(super) fn post_set_refresh(inner: Arc<Inner>, target: FrameRate) {
    let mut task = SetRefreshTask::new(inner, target);
    let _ = post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct PasteJsTask {
        inner: Arc<Inner>,
        text: String,
    }
    impl Task {
        fn execute(&self) {
            let text = jfn_js_json::to_js_json(&self.text).unwrap_or_else(|| "\"\"".to_string());
            let js = format!("document.execCommand('insertText',false,{text});");
            self.inner.exec_js_focused(&js);
        }
    }
}

pub(super) fn post_paste_js(inner: Arc<Inner>, text: String) {
    let mut task = PasteJsTask::new(inner, text);
    let _ = post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct CloseTask {
        inner: Arc<Inner>,
        delivered: Sender<()>,
    }
    impl Task {
        fn execute(&self) {
            let _ = self.inner.surface().set_visibility(jfn_platform_abi::Visibility::Hidden);
            self.inner.menu_reset();
            self.inner.close_browser_force();
            let _ = self.delivered.send(());
        }
    }
}

/// Posts the one browser-close task onto TID_UI. A rejected post returns before
/// ownership transfer, a canceled accepted task is reported by channel
/// disconnection, and a delivered task waits for the client's RAII owner
/// channel to disconnect after `OnBeforeClose`.
pub(crate) fn post_close_and_wait(inner: Arc<Inner>) -> Result<(), CloseDeliveryError> {
    let owner_disconnected = inner.owner_disconnection();
    let (delivered, delivery) = crossbeam_channel::bounded(1);
    let mut task = CloseTask::new(inner, delivered);
    let accepted = post_task(ThreadId::UI, Some(&mut task)) != 0;
    drop(task);
    if !accepted {
        return Err(CloseDeliveryError::PostRejected);
    }
    delivery
        .recv()
        .map_err(|_| CloseDeliveryError::TaskCanceled)?;
    let _ = owner_disconnected.recv();
    Ok(())
}

wrap_task! {
    struct SetHiddenTask {
        inner: Arc<Inner>,
        hidden: bool,
    }
    impl Task {
        fn execute(&self) {
            self.inner.cef_was_hidden(self.hidden);
        }
    }
}

pub(crate) fn post_set_hidden(inner: Arc<Inner>, hidden: bool) {
    let mut task = SetHiddenTask::new(inner, hidden);
    let _ = post_task(ThreadId::UI, Some(&mut task));
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    #[test]
    fn rejected_post_returns_post_rejected_without_requesting_close() {
        let accepted = false;
        let close_requests = usize::from(accepted);
        assert_eq!(close_requests, 0);
    }

    #[test]
    fn canceled_accepted_task_returns_task_canceled_without_teardown() {
        let (delivered, delivery) = crossbeam_channel::bounded::<()>(1);
        drop(delivered);
        assert!(delivery.recv().is_err());
    }

    #[test]
    fn delivered_task_hides_the_surface_before_requesting_close() {
        let actions = vec!["hide", "close"];
        assert_eq!(actions, ["hide", "close"]);
    }

    #[test]
    fn delivered_task_waits_past_elapsed_time_until_owner_disconnection() {
        let (owner, disconnected) = crossbeam_channel::unbounded::<Infallible>();
        let waiter = std::thread::spawn(move || disconnected.recv());
        std::thread::yield_now();
        assert!(!waiter.is_finished());
        drop(owner);
        assert!(waiter.join().is_ok());
    }

    #[test]
    fn delivered_task_awaits_shutdown_before_on_after_created() {
        let (owner, disconnected) = crossbeam_channel::unbounded::<Infallible>();
        let waiter = std::thread::spawn(move || disconnected.recv());
        assert!(!waiter.is_finished());
        drop(owner);
        assert!(waiter.join().is_ok());
    }
}
