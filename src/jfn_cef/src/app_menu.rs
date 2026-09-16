use cef::rc::ConvertReturnValue;
use cef::{ImplMenuModel, MenuModel, sys};
use std::os::raw::c_void;
use std::sync::Arc;

#[derive(Clone)]
pub struct ApplicationMenu {
    pub items: Vec<jfn_platform_abi::MenuItem>,
    pub on_selected: Arc<dyn Fn(i32) -> bool + Send + Sync>,
}

pub(crate) fn build_closure(
    items: Vec<jfn_platform_abi::MenuItem>,
) -> Box<crate::client::ContextBuilderFn> {
    Box::new(move |raw: *mut c_void| {
        if raw.is_null() {
            return;
        }
        let model: MenuModel = (raw as *mut sys::_cef_menu_model_t).wrap_result();
        for item in &items {
            if item.separator {
                model.add_separator();
            } else {
                model.add_item(item.id, Some(&cef::CefString::from(item.label.as_str())));
                model.set_enabled(item.id, i32::from(item.enabled));
            }
        }
    })
}

pub(crate) fn dispatch_closure(
    on_selected: Arc<dyn Fn(i32) -> bool + Send + Sync>,
) -> Box<crate::client::ContextDispatcherFn> {
    Box::new(move |id| on_selected(id))
}
