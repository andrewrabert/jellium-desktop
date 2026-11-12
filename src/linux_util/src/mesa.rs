//! Mesa driver environment workarounds, applied per CEF runtime version.

/// The CEF major whose Linux shared-texture OSR capturer requests
/// `SCANOUT` buffers with the compositor's full modifier list, letting
/// radeonsi pick an AMD DCC modifier that then never paints. See
/// <https://github.com/andrewrabert/jellium-desktop/commit/ec4cba2004989cd9e06a96dc6c21ba121bf396f6>.
const DCC_BROKEN_CEF_MAJOR: u32 = 156;

const AMD_DEBUG: &str = "AMD_DEBUG";
const NODCC: &str = "nodcc";

/// Value `AMD_DEBUG` must carry for `cef_major`, given its current value.
/// `None` means leave the variable untouched. `AMD_DEBUG` is a
/// comma-separated flag list (<https://docs.mesa3d.org/envvars.html>), so
/// an existing value is kept verbatim and `nodcc` appended to it.
fn amd_debug_for(cef_major: u32, current: Option<&str>) -> Option<String> {
    if cef_major != DCC_BROKEN_CEF_MAJOR {
        return None;
    }
    match current.filter(|s| !s.is_empty()) {
        None => Some(NODCC.to_owned()),
        Some(cur) if cur.split(',').any(|flag| flag == NODCC) => None,
        Some(cur) => Some(format!("{cur},{NODCC}")),
    }
}

/// Export `AMD_DEBUG=nodcc` to this process and everything it spawns (CEF's
/// GPU process, mpv) when the loaded libcef is the affected major. Any
/// other major leaves the environment alone. Must run on the main thread
/// before CEF initialization and mpv boot, so both inherit it.
pub fn apply_amd_dcc_workaround(cef_major: u32) -> Option<String> {
    let current = std::env::var(AMD_DEBUG).ok();
    let value = amd_debug_for(cef_major, current.as_deref())?;
    // SAFETY: startup path on the main thread; nothing reads AMD_DEBUG yet.
    unsafe { std::env::set_var(AMD_DEBUG, &value) };
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_broken_major_is_patched() {
        assert_eq!(amd_debug_for(154, None), None);
        assert_eq!(amd_debug_for(157, None), None);
        assert_eq!(amd_debug_for(156, None), Some("nodcc".to_owned()));
    }

    #[test]
    fn existing_flags_are_kept() {
        assert_eq!(amd_debug_for(156, Some("")), Some("nodcc".to_owned()));
        assert_eq!(
            amd_debug_for(156, Some("nodma")),
            Some("nodma,nodcc".to_owned())
        );
        assert_eq!(
            amd_debug_for(156, Some("nodma,nogfx")),
            Some("nodma,nogfx,nodcc".to_owned())
        );
        assert_eq!(amd_debug_for(156, Some("nodcc")), None);
        assert_eq!(amd_debug_for(156, Some("nodma,nodcc")), None);
        assert_eq!(amd_debug_for(154, Some("nodma")), None);
    }
}
