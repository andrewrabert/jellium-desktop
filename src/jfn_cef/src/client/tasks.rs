use cef::rc::Rc;
use cef::{ImplTask, Task, ThreadId, WrapTask, post_delayed_task, post_task, wrap_task};
use crossbeam_channel::Sender;
use std::sync::Arc;

use super::Inner;
use jfn_playback::shutdown::jfn_shutting_down;

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
        target: i32,
    }
    impl Task {
        fn execute(&self) {
            self.inner.apply_set_refresh(self.target);
        }
    }
}

pub(super) fn post_set_refresh(inner: Arc<Inner>, target: i32) {
    let mut task = SetRefreshTask::new(inner, target);
    let _ = post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct ResetCreateTask {
        inner: Arc<Inner>,
    }
    impl Task {
        fn execute(&self) {
            // Creating a browser during shutdown races CefShutdown teardown
            // and hangs.
            if jfn_shutting_down() {
                return;
            }
            self.inner.create("");
        }
    }
}

pub(super) fn post_reset_create(inner: Arc<Inner>) {
    let mut task = ResetCreateTask::new(inner);
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
    struct CloseAndCollectTask {
        inner: Arc<Inner>,
        tx: Sender<Arc<Inner>>,
    }
    impl Task {
        fn execute(&self) {
            // A browser dying mid-menu must not strand the session slot. The
            // close runs after the send is prepared so the waiter always holds
            // the `Arc` whose `close_cv` it parks on.
            self.inner.menu_reset();
            let inner = Arc::clone(&self.inner);
            let _ = self.tx.send(Arc::clone(&inner));
            inner.close_browser_force();
        }
    }
}

/// Post the one close-and-collect task onto TID_UI. MUST run off TID_UI.
pub(crate) fn post_close_and_collect(inner: Arc<Inner>, tx: Sender<Arc<Inner>>) {
    let mut task = CloseAndCollectTask::new(inner, tx);
    assert!(
        post_task(ThreadId::UI, Some(&mut task)) != 0,
        "TID_UI post during shutdown — CEF UI thread invariant broken"
    );
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
