//! MPRIS media-session integration, shared by the Wayland and X11 backends.

#![cfg(target_os = "linux")]

mod projection;
mod sink;

/// MPRIS-backed [`jfn_platform_abi::MediaSink`].
pub struct MprisSink;

impl jfn_platform_abi::MediaSink for MprisSink {
    fn start(&self, instance: &jfn_platform_abi::Instance) {
        sink::start(&format!(".instance_{}", instance.id()));
    }

    fn stop(&self) {
        sink::stop();
    }
}
