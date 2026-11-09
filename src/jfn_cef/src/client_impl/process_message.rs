use cef::{ImplListValue, ImplProcessMessage, ProcessMessage};
use std::os::raw::c_int;
use std::sync::Arc;

use crate::cef_string::userfree_to_string;
use crate::client::Inner;
use crate::ipc::BrowserMessage;

pub(super) fn on_process_message_received(
    inner: &Arc<Inner>,
    message: Option<&mut ProcessMessage>,
) -> c_int {
    let Some(msg) = message else { return 0 };
    let name = userfree_to_string(&msg.name());
    let args = msg.argument_list();
    match name.as_str() {
        "popupOptions" => {
            if let Some(args) = args {
                let opts = if let Some(list) = args.list(0) {
                    let n = list.size();
                    let mut v = Vec::with_capacity(n);
                    for i in 0..n {
                        v.push(userfree_to_string(&list.string(i)));
                    }
                    v
                } else {
                    Vec::new()
                };
                let selected = args.int(1);
                let selectable = if let Some(list) = args.list(2) {
                    let n = list.size();
                    let mut v = Vec::with_capacity(n);
                    for i in 0..n {
                        v.push(list.int(i));
                    }
                    v
                } else {
                    Vec::new()
                };
                let anchor = (args.int(5) != 0).then(|| (args.int(3), args.int(4)));
                inner.set_popup_options(opts, selected, selectable, anchor);
            }
            1
        }
        _ => {
            let message = BrowserMessage::new(name, args);
            if inner.invoke_message_handler(message) {
                1
            } else {
                0
            }
        }
    }
}
