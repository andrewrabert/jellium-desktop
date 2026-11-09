use super::{APP_DIR_NAME, home};
use std::path::PathBuf;

pub(super) const DEFAULT_LOG_TO_FILE: bool = false;

/// `dirs` resolves `$HOME` through `getpwuid_r` before this fallback is
/// reached.
fn home_subdir(subdir: &str) -> PathBuf {
    PathBuf::from(home()).join(subdir)
}

pub(super) fn config_base() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| home_subdir(".config"))
}

pub(super) fn cache_base() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(|| home_subdir(".cache"))
}

pub(super) fn log_dir_path() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| home_subdir(".local/state"))
        .join(APP_DIR_NAME)
}
