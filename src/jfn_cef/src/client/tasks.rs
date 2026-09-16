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
            self.inner.session.dispatch(|| self.inner.apply_pending_resize());
        }
    }
}

pub(super) fn post_apply_resize(inner: Arc<Inner>, delay_ms: i64) {
    let session = Arc::clone(&inner.session);
    session.dispatch(|| {
        let mut task = ApplyResizeTask::new(inner);
        let _ = post_delayed_task(ThreadId::UI, Some(&mut task), delay_ms);
    });
}

wrap_task! {
    struct SetRefreshTask {
        inner: Arc<Inner>,
        target: FrameRate,
    }
    impl Task {
        fn execute(&self) {
            self.inner.session.dispatch(|| self.inner.apply_set_refresh(self.target));
        }
    }
}

pub(super) fn post_set_refresh(inner: Arc<Inner>, target: FrameRate) {
    let session = Arc::clone(&inner.session);
    session.dispatch(|| {
        let mut task = SetRefreshTask::new(inner, target);
        let _ = post_task(ThreadId::UI, Some(&mut task));
    });
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
            self.inner.session.dispatch(|| self.inner.exec_js_focused(&js));
        }
    }
}

pub(super) fn post_paste_js(inner: Arc<Inner>, text: String) {
    let session = Arc::clone(&inner.session);
    session.dispatch(|| {
        let mut task = PasteJsTask::new(inner, text);
        let _ = post_task(ThreadId::UI, Some(&mut task));
    });
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

pub(crate) fn post_close_and_wait(
    inner: Arc<Inner>,
    deadline: std::time::Instant,
) -> Result<(), CloseDeliveryError> {
    let owner_disconnected = inner.owner_disconnection();
    let (delivered, delivery) = crossbeam_channel::bounded(1);
    let mut task = CloseTask::new(inner, delivered);
    let accepted = post_task(ThreadId::UI, Some(&mut task)) != 0;
    drop(task);
    if !accepted {
        return Err(CloseDeliveryError::PostRejected);
    }
    wait_for_close(delivery, owner_disconnected, deadline)
}

fn wait_for_close(
    delivery: crossbeam_channel::Receiver<()>,
    owner_disconnected: crossbeam_channel::Receiver<std::convert::Infallible>,
    deadline: std::time::Instant,
) -> Result<(), CloseDeliveryError> {
    delivery
        .recv_deadline(deadline)
        .map_err(|error| match error {
            crossbeam_channel::RecvTimeoutError::Timeout => {
                CloseDeliveryError::Timeout("close delivery")
            }
            crossbeam_channel::RecvTimeoutError::Disconnected => CloseDeliveryError::TaskCanceled,
        })?;
    match owner_disconnected.recv_deadline(deadline) {
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {}
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
            return Err(CloseDeliveryError::Timeout("browser owner release"));
        }
        Ok(never) => match never {},
    }
    Ok(())
}

wrap_task! {
    struct SetHiddenTask {
        inner: Arc<Inner>,
        hidden: bool,
    }
    impl Task {
        fn execute(&self) {
            self.inner.session.dispatch(|| self.inner.cef_was_hidden(self.hidden));
        }
    }
}

pub(crate) fn post_set_hidden(inner: Arc<Inner>, hidden: bool) {
    let session = Arc::clone(&inner.session);
    session.dispatch(|| {
        let mut task = SetHiddenTask::new(inner, hidden);
        let _ = post_task(ThreadId::UI, Some(&mut task));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn accepted_close_reports_delivery_timeout() {
        let (_sender, delivery) = crossbeam_channel::bounded(1);
        let (_owner, disconnected) = crossbeam_channel::unbounded();
        assert_eq!(
            wait_for_close(delivery, disconnected, Instant::now()),
            Err(CloseDeliveryError::Timeout("close delivery"))
        );
    }

    #[test]
    fn canceled_close_does_not_confirm_drain() {
        let (sender, delivery) = crossbeam_channel::bounded(1);
        drop(sender);
        let (_owner, disconnected) = crossbeam_channel::unbounded();
        assert_eq!(
            wait_for_close(delivery, disconnected, Instant::now()),
            Err(CloseDeliveryError::TaskCanceled)
        );
    }

    #[test]
    fn delivered_close_reports_owner_timeout() {
        let (sender, delivery) = crossbeam_channel::bounded(1);
        assert!(sender.send(()).is_ok());
        let (_owner, disconnected) = crossbeam_channel::unbounded();
        assert_eq!(
            wait_for_close(delivery, disconnected, Instant::now()),
            Err(CloseDeliveryError::Timeout("browser owner release"))
        );
    }

    #[test]
    fn delivered_close_requires_owner_release() {
        let (sender, delivery) = crossbeam_channel::bounded(1);
        assert!(sender.send(()).is_ok());
        let (owner, disconnected) = crossbeam_channel::unbounded();
        drop(owner);
        assert_eq!(
            wait_for_close(
                delivery,
                disconnected,
                Instant::now() + Duration::from_secs(1)
            ),
            Ok(())
        );
    }
}
