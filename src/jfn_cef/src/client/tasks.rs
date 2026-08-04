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
        tx: Sender<Vec<Arc<Inner>>>,
    }
    impl Task {
        fn execute(&self) {
            let _ = self.tx.send(crate::browsers::jfn_browsers_close_and_snapshot());
        }
    }
}

pub(crate) fn jfn_cef_post_close_and_collect(tx: Sender<Vec<Arc<Inner>>>) {
    let mut task = CloseAndCollectTask::new(tx);
    assert!(
        post_task(ThreadId::UI, Some(&mut task)) != 0,
        "TID_UI post during shutdown — CEF UI thread invariant broken"
    );
}

wrap_task! {
    struct SetHiddenAllTask {
        hidden: bool,
    }
    impl Task {
        fn execute(&self) {
            crate::browsers::jfn_browsers_apply_hidden_all(self.hidden);
        }
    }
}

pub(crate) fn jfn_cef_post_set_hidden_all(hidden: bool) {
    let mut task = SetHiddenAllTask::new(hidden);
    let _ = post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct PushCsdStateAllTask {}
    impl Task {
        fn execute(&self) {
            crate::browsers::jfn_browsers_apply_csd_state_all();
        }
    }
}

pub(crate) fn jfn_cef_post_csd_state_all() {
    let mut task = PushCsdStateAllTask::new();
    let _ = post_task(ThreadId::UI, Some(&mut task));
}
