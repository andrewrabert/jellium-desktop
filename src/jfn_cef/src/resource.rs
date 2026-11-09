//! `app://` scheme handler.
//!
//! Embedded resources are included at compile time from `src/web/*`.

use cef::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---- embedded resources ----------------------------------------------------

struct Embedded {
    bytes: &'static [u8],
    mime: &'static str,
}

macro_rules! embedded {
    ($name:literal, $mime:literal) => {
        (
            $name,
            Embedded {
                bytes: include_bytes!(concat!("../../web/", $name)),
                mime: $mime,
            },
        )
    };
}

// URL key is the path after the `app://` scheme (no leading slash).
static RESOURCES: &[(&str, Embedded)] = &[
    embedded!("input-plugin.js", "application/javascript"),
    embedded!("mpv-audio-player.js", "application/javascript"),
    embedded!("mpv-player-base.js", "application/javascript"),
    embedded!("mpv-video-player.js", "application/javascript"),
    embedded!("native-shim.js", "application/javascript"),
    embedded!("select-menu.js", "application/javascript"),
];

fn lookup(url_path: &str) -> Option<&'static Embedded> {
    // URL key has the "resources/" prefix; strip it to match RESOURCES.
    let name = url_path.strip_prefix("resources/")?;
    RESOURCES.iter().find(|(n, _)| *n == name).map(|(_, r)| r)
}

// ---- SchemeHandlerFactory --------------------------------------------------

#[derive(Clone)]
pub(crate) struct JfnSchemeFactory;

wrap_scheme_handler_factory! {
    pub(crate) struct JfnSchemeFactoryBuilder { inner: JfnSchemeFactory, }

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _scheme_name: Option<&CefString>,
            request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            let request = request?;
            let url_uf = request.url();
            let url = crate::cef_string::userfree_to_string(&url_uf);

            // Strip scheme prefix and query/fragment.
            let after_scheme = url
                .find("://")
                .map(|p| &url[p + 3..])
                .unwrap_or(&url);
            let url_path = after_scheme
                .split(['?', '#'])
                .next()
                .unwrap_or("")
                .to_string();

            let (bytes, mime): (Vec<u8>, &'static str) = if let Some(r) = lookup(&url_path) {
                (r.bytes.to_vec(), r.mime)
            } else {
                jfn_logging::log(
                    jfn_logging::Category::Resource,
                    jfn_logging::Level::Warn,
                    &format!("EmbeddedScheme not found: {url_path}"),
                );
                return None;
            };

            Some(
                JfnResourceHandlerBuilder::new(JfnResourceHandler {
                    bytes: Arc::new(bytes),
                    mime,
                    offset: Arc::new(AtomicUsize::new(0)),
                }),
            )
        }
    }
}

// ---- ResourceHandler -------------------------------------------------------

#[derive(Clone)]
pub(crate) struct JfnResourceHandler {
    bytes: Arc<Vec<u8>>,
    mime: &'static str,
    offset: Arc<AtomicUsize>,
}

wrap_resource_handler! {
    pub(crate) struct JfnResourceHandlerBuilder { inner: JfnResourceHandler, }

    impl ResourceHandler {
        fn open(
            &self,
            _request: Option<&mut Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            if let Some(h) = handle_request { *h = 1; }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            let len = self.inner.bytes.len() as i64;
            if let Some(rsp) = response {
                rsp.set_status(200);
                rsp.set_status_text(Some(&CefString::from("OK")));
                rsp.set_mime_type(Some(&CefString::from(self.inner.mime)));
            }
            if let Some(rl) = response_length { *rl = len; }
        }

        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: ::std::os::raw::c_int,
            bytes_read: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut ResourceReadCallback>,
        ) -> ::std::os::raw::c_int {
            let offset = self.inner.offset.load(Ordering::Relaxed);
            let total = self.inner.bytes.len();
            if offset >= total {
                if let Some(br) = bytes_read { *br = 0; }
                return 0;
            }
            let remaining = total - offset;
            let n = remaining.min(bytes_to_read as usize);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.inner.bytes.as_ptr().add(offset),
                    data_out,
                    n,
                );
            }
            self.inner.offset.store(offset + n, Ordering::Relaxed);
            if let Some(br) = bytes_read { *br = n as i32; }
            1
        }
    }
}

// ---- registration ----------------------------------------------------------

pub(crate) fn register() {
    let scheme = CefString::from("app");
    let domain = CefString::from("");
    register_scheme_handler_factory(
        Some(&scheme),
        Some(&domain),
        Some(&mut JfnSchemeFactoryBuilder::new(JfnSchemeFactory)),
    );
}
