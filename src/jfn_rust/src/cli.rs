use clap::{ArgAction, Parser};

const ENV_LOG_LEVEL: &str = "JELLIUM_DESKTOP_LOG_LEVEL";
const ENV_LOG_FILE: &str = "JELLIUM_DESKTOP_LOG_FILE";
const ENV_CONFIG_DIR: &str = "JELLIUM_DESKTOP_CONFIG_DIR";
const ENV_CACHE_DIR: &str = "JELLIUM_DESKTOP_CACHE_DIR";

#[cfg(test)]
const ENV_BACKED: &[&str] = &[ENV_LOG_LEVEL, ENV_LOG_FILE, ENV_CONFIG_DIR, ENV_CACHE_DIR];

#[derive(Parser, Debug)]
#[command(
    name = "jellium-desktop",
    disable_version_flag = true,
    args_override_self = true
)]
pub struct Cli {
    #[arg(short = 'v', long, action = ArgAction::SetTrue)]
    pub version: bool,

    #[arg(long, env = ENV_LOG_LEVEL)]
    pub log_level: Option<String>,

    #[arg(long, env = ENV_LOG_FILE)]
    pub log_file: Option<String>,

    #[arg(long, env = ENV_CONFIG_DIR)]
    pub config_dir: Option<String>,

    #[arg(long, env = ENV_CACHE_DIR)]
    pub cache_dir: Option<String>,

    #[arg(long)]
    pub hwdec: Option<String>,

    #[arg(long)]
    pub audio_passthrough: Option<String>,

    #[arg(long, action = ArgAction::SetTrue)]
    pub audio_exclusive: bool,

    #[arg(long)]
    pub audio_channels: Option<String>,

    #[arg(long)]
    pub remote_debug_port: Option<jfn_cef::DebuggingPort>,

    #[arg(long, action = ArgAction::SetTrue)]
    pub disable_gpu_compositing: bool,

    #[cfg(target_os = "linux")]
    #[command(flatten)]
    pub linux: jfn_linux_util::cli::LinuxArgs,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard(&'static str);
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            unsafe { std::env::set_var(key, val) };
            EnvGuard(key)
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(self.0) };
        }
    }

    struct EnvClear(Vec<(&'static str, Option<String>)>);
    impl EnvClear {
        fn new() -> Self {
            let saved = ENV_BACKED
                .iter()
                .map(|&k| {
                    let prev = std::env::var(k).ok();
                    unsafe { std::env::remove_var(k) };
                    (k, prev)
                })
                .collect();
            EnvClear(saved)
        }
    }
    impl Drop for EnvClear {
        fn drop(&mut self) {
            for (k, prev) in &self.0 {
                match prev {
                    Some(v) => unsafe { std::env::set_var(k, v) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    fn try_parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args.iter().copied())
    }

    fn ok(args: &[&str]) -> Cli {
        try_parse(args).expect("expected successful parse")
    }

    fn err_kind(args: &[&str]) -> ErrorKind {
        try_parse(args).expect_err("expected parse error").kind()
    }

    #[test]
    fn help_short() {
        assert_eq!(err_kind(&["app", "-h"]), ErrorKind::DisplayHelp);
    }

    #[test]
    fn help_long() {
        assert_eq!(err_kind(&["app", "--help"]), ErrorKind::DisplayHelp);
    }

    #[test]
    fn version_long() {
        assert!(ok(&["app", "--version"]).version);
    }

    #[test]
    fn version_short() {
        assert!(ok(&["app", "-v"]).version);
    }

    #[test]
    fn version_absent() {
        assert!(!ok(&["app"]).version);
    }

    #[test]
    fn unknown_flag() {
        assert_eq!(err_kind(&["app", "--nope"]), ErrorKind::UnknownArgument);
    }

    #[test]
    fn unknown_short_flag() {
        assert_eq!(err_kind(&["app", "-x"]), ErrorKind::UnknownArgument);
    }

    #[test]
    fn log_file_explicit_empty() {
        assert_eq!(ok(&["app", "--log-file", ""]).log_file.as_deref(), Some(""));
    }

    #[test]
    fn equals_form() {
        let a = ok(&["app", "--hwdec=vaapi", "--remote-debug-port=9222"]);
        assert_eq!(a.hwdec.as_deref(), Some("vaapi"));
        assert_eq!(
            a.remote_debug_port,
            "9222".parse::<jfn_cef::DebuggingPort>().ok()
        );
    }

    #[test]
    fn bool_flags_default_false() {
        let a = ok(&["app"]);
        assert!(!a.audio_exclusive);
        assert!(!a.disable_gpu_compositing);
    }

    #[test]
    fn bool_flags_set() {
        let a = ok(&["app", "--audio-exclusive", "--disable-gpu-compositing"]);
        assert!(a.audio_exclusive);
        assert!(a.disable_gpu_compositing);
    }

    #[test]
    fn no_args_all_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clear = EnvClear::new();
        let a = ok(&["app"]);
        assert!(a.hwdec.is_none());
        assert!(a.audio_passthrough.is_none());
        assert!(a.audio_channels.is_none());
        assert!(a.log_level.is_none());
        assert!(a.log_file.is_none());
        assert!(a.config_dir.is_none());
        assert!(a.cache_dir.is_none());
        assert!(!a.audio_exclusive);
        assert!(!a.disable_gpu_compositing);
        assert!(a.remote_debug_port.is_none());
        #[cfg(target_os = "linux")]
        {
            assert!(a.linux.platform.is_none());
            assert!(a.linux.platform_paint.is_none());
        }
    }

    #[test]
    fn positional_is_error() {
        assert!(try_parse(&["app", "positional"]).is_err());
    }

    #[test]
    fn missing_trailing_value_is_error() {
        assert!(try_parse(&["app", "--log-level"]).is_err());
    }

    #[test]
    fn remote_debug_port_range_is_validated() {
        for value in ["1", "1023", "65536", "-1"] {
            assert!(try_parse(&["app", &format!("--remote-debug-port={value}")]).is_err());
        }
        assert_eq!(
            ok(&["app", "--remote-debug-port=0"]).remote_debug_port,
            Some(jfn_cef::DebuggingPort::Disabled)
        );
    }

    #[test]
    fn remote_debug_port_non_numeric_error() {
        assert!(try_parse(&["app", "--remote-debug-port=bogus"]).is_err());
    }

    #[test]
    fn space_form_common_flags() {
        let a = ok(&[
            "app",
            "--hwdec",
            "vaapi",
            "--log-level",
            "debug",
            "--log-file",
            "/tmp/x.log",
            "--config-dir",
            "/tmp/config",
            "--cache-dir",
            "/tmp/cache",
            "--audio-passthrough",
            "ac3,dts-hd",
            "--audio-channels",
            "5.1",
            "--remote-debug-port",
            "9222",
        ]);
        assert_eq!(a.hwdec.as_deref(), Some("vaapi"));
        assert_eq!(a.log_level.as_deref(), Some("debug"));
        assert_eq!(a.log_file.as_deref(), Some("/tmp/x.log"));
        assert_eq!(a.config_dir.as_deref(), Some("/tmp/config"));
        assert_eq!(a.cache_dir.as_deref(), Some("/tmp/cache"));
        assert_eq!(a.audio_passthrough.as_deref(), Some("ac3,dts-hd"));
        assert_eq!(a.audio_channels.as_deref(), Some("5.1"));
        assert_eq!(
            a.remote_debug_port,
            "9222".parse::<jfn_cef::DebuggingPort>().ok()
        );
    }

    #[test]
    fn log_file_unset_vs_explicit_empty() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _clear = EnvClear::new();
        assert!(ok(&["app"]).log_file.is_none());
        assert_eq!(ok(&["app", "--log-file="]).log_file.as_deref(), Some(""));
    }

    #[test]
    fn prefix_collision_log_level_vs_log_file() {
        let a = ok(&["app", "--log-file=path", "--log-level=trace"]);
        assert_eq!(a.log_file.as_deref(), Some("path"));
        assert_eq!(a.log_level.as_deref(), Some("trace"));
    }

    #[test]
    fn duplicate_flag_last_wins() {
        let a = ok(&["app", "--log-level=info", "--log-level", "debug"]);
        assert_eq!(a.log_level.as_deref(), Some("debug"));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn platform_flags_rejected_off_linux() {
        assert_eq!(
            err_kind(&["app", "--platform", "x11"]),
            ErrorKind::UnknownArgument
        );
        assert_eq!(
            err_kind(&["app", "--platform-paint", "shm"]),
            ErrorKind::UnknownArgument
        );
        assert_eq!(
            err_kind(&["app", "--wid", "1234"]),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn env_log_level_fallback() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _v = EnvGuard::set(ENV_LOG_LEVEL, "debug");
        assert_eq!(ok(&["app"]).log_level.as_deref(), Some("debug"));
    }

    #[test]
    fn env_cli_flag_overrides_env() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _v = EnvGuard::set(ENV_LOG_LEVEL, "debug");
        assert_eq!(
            ok(&["app", "--log-level", "info"]).log_level.as_deref(),
            Some("info")
        );
    }

    #[test]
    fn env_config_dir_fallback() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _v = EnvGuard::set(ENV_CONFIG_DIR, "/tmp/jfd-cfg");
        assert_eq!(ok(&["app"]).config_dir.as_deref(), Some("/tmp/jfd-cfg"));
    }

    #[test]
    fn const_defaults_match_help_text() {
        assert_eq!(jfn_config::HWDEC_DEFAULT, "no");
        assert_eq!(crate::app::DEFAULT_LOG_FILTER, "info");
    }
}
