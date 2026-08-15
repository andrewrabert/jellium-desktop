use cef::rc::Rc;
use cef::*;
use std::os::raw::{c_int, c_void};
use std::sync::Arc;

use crate::cef_string::userfree_to_string;
use crate::client::Inner;
use crate::platform_ops::{MENU_DISMISSED, MenuDelivery, MenuItem, MenuKind, MenuRequest};

const STRIP_ACCEL_KEEP: u8 = b'&';

fn strip_accelerator(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b != STRIP_ACCEL_KEEP {
            out.push(b as char);
        }
    }
    if s.is_ascii() {
        return out;
    }
    s.chars().filter(|c| *c != '&').collect()
}

wrap_context_menu_handler! {
    pub struct JfnContextMenuHandlerBuilder {
        inner: Arc<Inner>,
    }

    impl ContextMenuHandler {
        fn on_before_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _params: Option<&mut ContextMenuParams>,
            model: Option<&mut MenuModel>,
        ) {
            let Some(m) = model else { return };
            m.remove(MenuId::PRINT.get_raw() as c_int);
            m.remove(MenuId::VIEW_SOURCE.get_raw() as c_int);
            let reload_id: c_int = MenuId::RELOAD.get_raw() as c_int;
            if m.index_of(reload_id) < 0 {
                m.add_item(reload_id, Some(&CefString::from("Reload")));
            }
            loop {
                let n = m.count();
                if n == 0 {
                    break;
                }
                let t: sys::cef_menu_item_type_t = m.type_at(n - 1).into();
                if t == sys::cef_menu_item_type_t::MENUITEMTYPE_SEPARATOR {
                    m.remove_at(n - 1);
                } else {
                    break;
                }
            }
            if self.inner.has_context_menu_builder() {
                m.add_separator();
                // C++ thunk uses CefMenuModelCToCpp::Wrap which adopts one ref.
                unsafe { Rc::add_ref(m) };
                let raw = ImplMenuModel::get_raw(m) as *mut c_void;
                self.inner.invoke_context_menu_builder(raw);
            }
        }
        fn run_context_menu(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            params: Option<&mut ContextMenuParams>,
            model: Option<&mut MenuModel>,
            callback: Option<&mut RunContextMenuCallback>,
        ) -> c_int {
            let (Some(_browser), Some(params), Some(model), Some(callback)) =
                (browser, params, model, callback)
            else {
                return 0;
            };
            if model.count() == 0 {
                callback.cancel();
                return 1;
            }
            let MenuDelivery::Host(host) =
                jfn_platform_abi::menu_delivery(MenuKind::ContextMenu)
            else {
                callback.cancel();
                return 1;
            };
            let Some(session) = self.inner.menu_open() else {
                callback.cancel();
                return 1;
            };
            self.inner.store_pending_menu_callback(callback.clone());

            let mut items = Vec::with_capacity(model.count());
            for i in 0..model.count() {
                let t: sys::cef_menu_item_type_t = model.type_at(i).into();
                if t == sys::cef_menu_item_type_t::MENUITEMTYPE_SEPARATOR {
                    items.push(MenuItem {
                        id: 0,
                        label: String::new(),
                        enabled: false,
                        separator: true,
                    });
                } else {
                    let raw_label = userfree_to_string(&model.label_at(i));
                    items.push(MenuItem {
                        id: model.command_id_at(i),
                        label: strip_accelerator(&raw_label),
                        enabled: model.is_enabled_at(i) != 0,
                        separator: false,
                    });
                }
            }

            host.open(MenuRequest {
                items,
                x: params.xcoord(),
                // The anchor is in the web overlay's own view space; the app menu
                // is placed in window space, so the strip the overlay was sized
                // below is added back.
                y: params.ycoord() + self.inner.view_top(),
                width: 0,
                initial: MENU_DISMISSED,
                on_selected: self.inner.menu_selection_callback(session),
            });
            1
        }
    }
}
