use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct ApplicationMetadata {
    pub app_version: String,
    pub cef_version: String,
    pub config_dir: PathBuf,
    pub log_file: Option<PathBuf>,
}

#[cfg(test)]
impl ApplicationMetadata {
    #[allow(clippy::expect_used)]
    pub(crate) fn testing() -> Self {
        Self {
            app_version: "app".to_owned(),
            cef_version: "151.3.16+gabcdef0+chromium-151.0.0.0".to_owned(),
            config_dir: PathBuf::from("/config"),
            log_file: None,
        }
    }
}
