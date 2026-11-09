//! Wayland subsystem: clipboard, input, KDE decoration palette, output-scale probe.
//!
//! Subsystem state is owned by [`runtime::WlRuntime`] and passed to the code
//! that needs it; nothing reaches its state through a module-level `static`.

#![cfg(target_os = "linux")]

pub(crate) mod app_conn;
pub(crate) mod decoration_probe;
pub(crate) mod input;
pub(crate) mod input_lifecycle;
#[cfg(feature = "kde-palette")]
pub(crate) mod kde_palette;
pub(crate) mod layer;
pub(crate) mod layer_actor;
pub(crate) mod lifecycle;
pub mod make_platform;
pub(crate) mod mpv_host;
pub(crate) mod mpv_proxy;
pub mod paint_override;
pub(crate) mod popup;
pub(crate) mod popup_protocol;
pub(crate) mod protocol;
pub(crate) mod root_window;
pub(crate) mod runtime;
pub(crate) mod scale;
pub(crate) mod scale_probe;
pub(crate) mod scene;
pub(crate) mod selection;
pub(crate) mod window_source;
pub(crate) mod window_state;
pub(crate) mod wl_ops;
pub(crate) mod wl_state;

pub use paint_override::WlPaintOverride;

#[cfg(test)]
mod source_guard {
    use std::path::Path;

    fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rs_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// State belongs to `WlRuntime`, which is passed to whoever needs it. A new
    /// `static` here would reintroduce exactly the ambient reachability that
    /// ownership removed, so it has to be argued for rather than typed.
    #[test]
    fn no_module_statics() {
        let mut files = Vec::new();
        rs_files(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path(),
            &mut files,
        );
        assert!(!files.is_empty(), "found no sources to scan");

        let mut found = Vec::new();
        for path in &files {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("static ") || trimmed.starts_with("pub static ") {
                    found.push(format!("{}:{}: {}", path.display(), i + 1, trimmed));
                }
            }
        }
        assert!(
            found.is_empty(),
            "module-level statics are not allowed in this crate; \
             put the state on WlRuntime and pass it:\n{}",
            found.join("\n")
        );
    }
}
