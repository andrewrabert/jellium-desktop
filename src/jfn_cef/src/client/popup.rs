use cef::rc::Rc;
use cef::{ImplBrowserHost, ImplTask, KeyEvent, Task, WrapTask, sys, wrap_task};
use jfn_platform_abi::{MenuOpenError, WindowStateError};
use std::sync::Arc;

use crate::platform_ops::{MenuDelivery, MenuItem, MenuRequest, MenuSelection};

use super::{Inner, PopupAt, PopupState};

fn options_as_items(options: &[String]) -> Vec<MenuItem> {
    options
        .iter()
        .enumerate()
        .map(|(i, label)| MenuItem {
            id: i as i32,
            label: label.clone(),
            enabled: true,
            separator: false,
        })
        .collect()
}

// Windows virtual-key codes CEF expects in KeyEvent::windows_key_code.
const VK_RETURN: i32 = 0x0D;
const VK_ESCAPE: i32 = 0x1B;
const VK_UP: i32 = 0x26;
const VK_DOWN: i32 = 0x28;

impl Inner {
    pub(super) fn dropdown(&self) -> MenuDelivery<'_> {
        self.surface()
            .platform()
            .menu_delivery(jfn_platform_abi::MenuKind::Dropdown)
    }
    fn reset_popup_state(p: &mut PopupState) {
        p.size_received = false;
        p.options_received = false;
        p.options.clear();
        p.selected_idx = -1;
        p.selectable.clear();
        p.rect = None;
        p.anchor = None;
        p.requested = false;
    }

    fn popup_at(&self, x: i32, y: i32) -> Result<PopupAt, WindowStateError> {
        match self.dropdown() {
            MenuDelivery::Host(_) => jfn_input::web_to_window_point(
                jfn_platform_abi::WebPoint::from_view(f64::from(x), f64::from(y)),
            )
            .map(|(point, basis)| PopupAt::Window(point, basis)),
            MenuDelivery::Composited | MenuDelivery::Page => Ok(PopupAt::View(x, y)),
        }
    }

    fn report_popup_at(e: &WindowStateError) {
        match e {
            WindowStateError::NoWindow
            | WindowStateError::WebOffsetUnstated
            | WindowStateError::NoPlatform => {}
            e => jfn_logging::log(
                jfn_logging::Category::Cef,
                jfn_logging::Level::Error,
                &format!("popup position: window state: {e}"),
            ),
        }
    }

    pub(crate) fn on_popup_show(&self, show: bool) {
        {
            let mut p = self.popup.lock();
            p.visible = show;
            Self::reset_popup_state(&mut p);
        }
        if !show {
            Self::report_hide(self.hide_dropdown());
            return;
        }
        self.send_process_message_named("getPopupOptions");
    }

    pub(crate) fn on_popup_size(self: &Arc<Self>, x: i32, y: i32, w: i32, h: i32) {
        let at = match self.popup_at(x, y) {
            Ok(at) => Some(at),
            Err(e) => {
                Self::report_popup_at(&e);
                None
            }
        };
        {
            let mut p = self.popup.lock();
            if p.requested {
                return;
            }
            p.rect = at;
            p.w = w;
            p.h = h;
            p.size_received = true;
        }
        if let Err(e) = self.try_show_popup() {
            Self::report_menu_open(e);
        }
    }

    pub(crate) fn set_popup_options(
        self: &Arc<Self>,
        opts: Vec<String>,
        selected: i32,
        selectable: Vec<i32>,
        anchor: Option<(i32, i32)>,
    ) {
        let anchor = anchor.map(|(x, y)| self.popup_at(x, y));
        if let Some(Err(e)) = &anchor {
            Self::report_popup_at(e);
        }
        {
            let mut p = self.popup.lock();
            p.options = opts;
            p.selected_idx = selected;
            p.selectable = selectable;
            match anchor {
                Some(Err(_)) => p.rect = None,
                Some(Ok(at)) => p.anchor = Some(at),
                None => {}
            }
            p.options_received = true;
        }
        if let Err(e) = self.try_show_popup() {
            Self::report_menu_open(e);
        }
    }

    fn report_menu_open(e: MenuOpenError) {
        if e != MenuOpenError::NothingSelectable {
            jfn_logging::log(
                jfn_logging::Category::Cef,
                jfn_logging::Level::Error,
                &format!("dropdown menu: {e}"),
            );
        }
    }

    fn try_show_popup(self: &Arc<Self>) -> Result<(), MenuOpenError> {
        let (at, w, h, opts, selected, selectable) = {
            let mut p = self.popup.lock();
            if !p.visible || !p.size_received || !p.options_received {
                return Ok(());
            }
            p.size_received = false;
            p.options_received = false;
            p.requested = true;
            let rect = p.rect.take();
            let Some(at) = p.anchor.take().or(rect) else {
                return Ok(());
            };
            (
                at,
                p.w,
                p.h,
                p.options.clone(),
                p.selected_idx,
                p.selectable.clone(),
            )
        };

        let inner = Arc::clone(self);
        let on_selected = MenuSelection::new(move |idx| {
            let authority = Arc::clone(&inner.session);
            authority.dispatch(|| {
                let mut task = DispatchPopupTask::new(inner, idx, selected, selectable.clone());
                if crate::ui_task::post_ui(&mut task).is_err() {
                    jfn_logging::log(
                        jfn_logging::Category::Cef,
                        jfn_logging::Level::Error,
                        &format!("dropdown selection {idx}: the UI thread rejected the task"),
                    );
                }
            });
        });
        match (self.dropdown(), at) {
            (MenuDelivery::Host(host), PopupAt::Window(anchor, basis)) => {
                let opened = host.open(MenuRequest {
                    items: options_as_items(&opts),
                    anchor,
                    basis,
                    width: w,
                    initial: selected,
                    on_selected,
                });
                match opened {
                    Ok(handle) => self.popup.lock().menu = Some(handle),
                    Err(e) => {
                        self.popup.lock().menu = None;
                        return Err(e);
                    }
                }
            }
            (MenuDelivery::Composited, PopupAt::View(x, y)) => {
                self.surface().popup_show(x, y, w, h)
            }
            _ => {}
        }
        Ok(())
    }

    fn hide_dropdown(&self) -> Result<(), jfn_platform_abi::MenuHideError> {
        match self.dropdown() {
            MenuDelivery::Host(host) => {
                let handle = self.popup.lock().menu.take();
                handle.map_or(Ok(()), |handle| host.hide(handle))
            }
            MenuDelivery::Composited => {
                self.surface().popup_hide();
                Ok(())
            }
            MenuDelivery::Page => Ok(()),
        }
    }

    pub(crate) fn on_deactivated(&self) {
        let was_visible = {
            let mut p = self.popup.lock();
            let was = p.visible;
            if was {
                p.visible = false;
                Self::reset_popup_state(&mut p);
            }
            was
        };
        if !was_visible {
            return;
        }
        Self::report_hide(self.hide_dropdown());
    }

    fn report_hide(hidden: Result<(), jfn_platform_abi::MenuHideError>) {
        if let Err(e) = hidden {
            jfn_logging::log(
                jfn_logging::Category::Cef,
                jfn_logging::Level::Error,
                &format!("dropdown menu hide: {e}"),
            );
        }
    }

    pub(super) fn popup_rect(&self) -> (i32, i32) {
        let p = self.popup.lock();
        (p.w, p.h)
    }

    // CEF OSR has no "set selected index" API for <select>: the popup is a real
    // RenderWidget that must be driven by forwarded input so Blink commits and
    // closes it cleanly (which is what lets it reopen). We render the menu
    // ourselves, then replay the user's pick into CEF's still-open popup —
    // arrow-key to the chosen row + Enter to commit, or Escape to cancel.
    fn dispatch_popup_selection(&self, idx: i32, current: i32, selectable: &[i32]) {
        // Blink already closed the popup: a replayed Escape or arrow would land
        // on the page instead.
        if !self.popup.lock().visible {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };

        let send_key = |code: i32| {
            let down = KeyEvent {
                type_: sys::cef_key_event_type_t::KEYEVENT_RAWKEYDOWN.into(),
                windows_key_code: code,
                native_key_code: code,
                ..KeyEvent::default()
            };
            let up = KeyEvent {
                type_: sys::cef_key_event_type_t::KEYEVENT_KEYUP.into(),
                windows_key_code: code,
                native_key_code: code,
                ..KeyEvent::default()
            };
            host.send_key_event(Some(&down));
            host.send_key_event(Some(&up));
        };

        if idx < 0 {
            send_key(VK_ESCAPE);
            return;
        }

        // Arrow stepping is in selectable-option space (Blink skips disabled
        // rows), so map both the popup's current highlight and the target into
        // that space and step by the difference.
        let pos = |opt: i32| selectable.iter().position(|&v| v == opt);
        let from = pos(current).unwrap_or(0) as i32;
        let Some(to) = pos(idx) else {
            // Target isn't selectable (shouldn't happen) — just cancel cleanly.
            send_key(VK_ESCAPE);
            return;
        };
        let delta = to as i32 - from;
        let step = if delta >= 0 { VK_DOWN } else { VK_UP };
        for _ in 0..delta.abs() {
            send_key(step);
        }
        send_key(VK_RETURN);
    }
}

wrap_task! {
    struct DispatchPopupTask {
        inner: Arc<Inner>,
        index: i32,
        current: i32,
        selectable: Vec<i32>,
    }
    impl Task {
        fn execute(&self) {
            self.inner.session.dispatch(|| self.inner.dispatch_popup_selection(
                self.index,
                self.current,
                &self.selectable,
            ));
        }
    }
}
