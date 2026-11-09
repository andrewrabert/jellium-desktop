use cef::*;
use std::os::raw::c_int;
use std::sync::Arc;

use crate::client::Inner;
use crate::platform_ops;

wrap_render_handler! {
    pub struct JfnRenderHandlerBuilder {
        inner: Arc<Inner>,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(r) = rect else { return };
            let (w, h) = self.inner.view_size();
            r.x = 0;
            r.y = 0;
            r.width = w;
            r.height = h;
        }
        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> c_int {
            let Some(si) = screen_info else { return 0 };
            let Some((scale, w, h)) = self.inner.screen_info_values() else {
                return 0;
            };
            si.device_scale_factor = scale.as_f32();
            si.rect = Rect { x: 0, y: 0, width: w, height: h };
            si.available_rect = si.rect.clone();
            1
        }
        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: c_int) {
            self.inner.on_popup_show(show != 0);
        }
        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let Some(r) = rect else { return };
            self.inner.on_popup_size(r.x, r.y, r.width, r.height);
        }
        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: c_int,
            height: c_int,
        ) {
            let kind: sys::cef_paint_element_type_t = type_.into();
            let is_popup = match kind {
                sys::cef_paint_element_type_t::PET_POPUP => true,
                sys::cef_paint_element_type_t::PET_VIEW => false,
                _ => return,
            };
            let rects: Vec<platform_ops::JfnRect> = dirty_rects
                .map(|d| {
                    d.iter()
                        .map(|r| platform_ops::JfnRect { x: r.x, y: r.y, w: r.width, h: r.height })
                        .collect()
                })
                .unwrap_or_default();
            self.inner.on_paint(is_popup, &rects, buffer, width, height);
        }
        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            let kind: sys::cef_paint_element_type_t = type_.into();
            let is_popup = match kind {
                sys::cef_paint_element_type_t::PET_POPUP => true,
                sys::cef_paint_element_type_t::PET_VIEW => false,
                _ => return,
            };
            let Some(info) = info else { return };
            self.inner.on_accelerated_paint(is_popup, info);
        }
    }
}
