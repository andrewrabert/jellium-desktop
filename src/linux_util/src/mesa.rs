//! Mesa driver environment workarounds, applied per CEF runtime version.

/// The CEF major whose Linux shared-texture OSR capturer requests
/// `SCANOUT` buffers with the compositor's full modifier list, letting the
/// driver pick a compressed modifier that then never paints: the first frame
/// capture fails and the web overlay stays on its first frame. See
/// <https://github.com/andrewrabert/jellium-desktop/commit/ec4cba2004989cd9e06a96dc6c21ba121bf396f6>.
const COMPRESSION_BROKEN_CEF_MAJOR: u32 = 156;

/// A Mesa debug variable and the flag in it that keeps the driver off its
/// compressed modifiers. Both variables are comma-separated flag lists
/// (<https://docs.mesa3d.org/envvars.html>), and a driver ignores the other
/// vendor's variable, so every entry is exported on every machine.
struct Workaround {
    var: &'static str,
    flag: &'static str,
}

const WORKAROUNDS: [Workaround; 2] = [
    // radeonsi: Delta Color Compression.
    Workaround {
        var: "AMD_DEBUG",
        flag: "nodcc",
    },
    // iris and anv: Color Control Surface compression.
    Workaround {
        var: "INTEL_DEBUG",
        flag: "noccs",
    },
];

/// Value a debug variable must carry for `cef_major`, given its current
/// value. `None` means leave the variable untouched. An existing value is
/// kept verbatim and `flag` appended to it.
fn debug_value_for(cef_major: u32, current: Option<&str>, flag: &str) -> Option<String> {
    if cef_major != COMPRESSION_BROKEN_CEF_MAJOR {
        return None;
    }
    match current.filter(|s| !s.is_empty()) {
        None => Some(flag.to_owned()),
        Some(cur) if cur.split(',').any(|f| f == flag) => None,
        Some(cur) => Some(format!("{cur},{flag}")),
    }
}

/// Export `AMD_DEBUG=nodcc` and `INTEL_DEBUG=noccs` to this process and
/// everything it spawns (CEF's GPU process, mpv) when the loaded libcef is
/// the affected major. Any other major leaves the environment alone. Must
/// run on the main thread before CEF initialization and mpv boot, so both
/// inherit it. Returns each variable it set, with its new value.
pub fn apply_compression_workarounds(cef_major: u32) -> Vec<(&'static str, String)> {
    let mut applied = Vec::new();
    for Workaround { var, flag } in WORKAROUNDS {
        let current = std::env::var(var).ok();
        if let Some(value) = debug_value_for(cef_major, current.as_deref(), flag) {
            // SAFETY: startup path on the main thread; nothing reads the
            // Mesa debug variables yet.
            unsafe { std::env::set_var(var, &value) };
            applied.push((var, value));
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_broken_major_is_patched() {
        for flag in ["nodcc", "noccs"] {
            assert_eq!(debug_value_for(154, None, flag), None);
            assert_eq!(debug_value_for(157, None, flag), None);
            assert_eq!(debug_value_for(156, None, flag), Some(flag.to_owned()));
        }
    }

    #[test]
    fn existing_flags_are_kept() {
        assert_eq!(
            debug_value_for(156, Some(""), "nodcc"),
            Some("nodcc".to_owned())
        );
        assert_eq!(
            debug_value_for(156, Some("nodma"), "nodcc"),
            Some("nodma,nodcc".to_owned())
        );
        assert_eq!(
            debug_value_for(156, Some("nodma,nogfx"), "nodcc"),
            Some("nodma,nogfx,nodcc".to_owned())
        );
        assert_eq!(debug_value_for(156, Some("nodcc"), "nodcc"), None);
        assert_eq!(debug_value_for(156, Some("nodma,nodcc"), "nodcc"), None);
        assert_eq!(debug_value_for(154, Some("nodma"), "nodcc"), None);
        assert_eq!(
            debug_value_for(156, Some("perf"), "noccs"),
            Some("perf,noccs".to_owned())
        );
        assert_eq!(debug_value_for(156, Some("noccs,perf"), "noccs"), None);
    }

    #[test]
    fn a_flag_is_matched_whole() {
        // `noccsx` is not `noccs`; only an exact list entry counts.
        assert_eq!(
            debug_value_for(156, Some("noccsx"), "noccs"),
            Some("noccsx,noccs".to_owned())
        );
    }
}
