use std::sync::{Arc, Weak};

use jfn_platform_abi::{MENU_DISMISSED, MenuJsBridge, MenuSelectionFn};

use super::Inner;

pub(crate) struct ClientMenuJsBridge {
    inner: Weak<Inner>,
}

impl ClientMenuJsBridge {
    pub(crate) fn install(inner: &Arc<Inner>) {
        jfn_platform_abi::install_menu_js_bridge(Arc::new(ClientMenuJsBridge {
            inner: Arc::downgrade(inner),
        }));
    }
}

impl MenuJsBridge for ClientMenuJsBridge {
    fn open(&self, script: String, on_selected: MenuSelectionFn) {
        let Some(inner) = self.inner.upgrade() else {
            on_selected(MENU_DISMISSED);
            return;
        };
        inner.park_menu_selection(on_selected);
        inner.exec_js(&script);
    }
}
