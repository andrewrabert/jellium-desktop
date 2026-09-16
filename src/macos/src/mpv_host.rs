use std::ffi::c_int;

use jfn_platform_abi::{MpvHost, VoWait, WindowDecorations};
use objc2_metal::{MTLCreateSystemDefaultDevice, MTLDevice, MTLGPUFamily};

fn metal_has_mac2_family() -> bool {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {}

    match MTLCreateSystemDefaultDevice() {
        Some(device) => device.supportsFamily(MTLGPUFamily::Mac2),
        None => true,
    }
}

pub struct MacosMpvHost;

impl MpvHost for MacosMpvHost {
    fn prepare(&self, _configured: Option<WindowDecorations>) {
        unsafe {
            let key = c"MPVBUNDLE";
            let val = c"true";
            libc::setenv(key.as_ptr(), val.as_ptr(), 1);

            if metal_has_mac2_family() {
                tracing::debug!(
                    target: "Platform",
                    "Metal Mac2 family present; keeping MoltenVK MTLHeap path"
                );
            } else {
                let key = c"MVK_CONFIG_USE_MTLHEAP";
                let val = c"0";
                libc::setenv(key.as_ptr(), val.as_ptr(), 1);
                tracing::info!(
                    target: "Platform",
                    "legacy Metal GPU without Mac2 family; disabled MoltenVK MTLHeap (MVK_CONFIG_USE_MTLHEAP=0)"
                );
            }
        }
    }

    fn run_vo_wait(&self, pump: &mut dyn FnMut(VoWait) -> std::ops::ControlFlow<()>) {
        unsafe {
            jfn_mpv::api::jfn_mpv_set_wakeup_callback(
                crate::macos_mpv_wakeup_cb,
                std::ptr::null_mut(),
            );
        }
        loop {
            crate::macos_pump();
            if let std::ops::ControlFlow::Break(()) = pump(VoWait::Drain) {
                break;
            }
            crate::backend::macos_wait_for_source();
        }
        jfn_mpv::api::jfn_mpv_clear_wakeup_callback();
    }

    fn logical_content_size(&self) -> Option<jfn_platform_abi::LogicalSize> {
        let mut w: c_int = 0;
        let mut h: c_int = 0;
        if crate::init::jfn_macos_query_logical_content_size(&mut w, &mut h) {
            Some(jfn_platform_abi::LogicalSize { w, h })
        } else {
            None
        }
    }
}
