use super::APP_DIR_NAME;
use std::path::PathBuf;

pub(super) const DEFAULT_LOG_TO_FILE: bool = true;

fn local_appdata() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("C:"))
}

pub(super) fn config_base() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from("C:"))
}

pub(super) fn cache_base() -> PathBuf {
    local_appdata()
}

pub(super) fn log_dir_path() -> PathBuf {
    local_appdata().join(APP_DIR_NAME).join("Logs")
}
