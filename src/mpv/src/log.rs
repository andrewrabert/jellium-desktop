use crate::event::LogMessage;
use crate::sys;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
#[allow(clippy::unnecessary_cast)]
pub enum LogLevel {
    Off = sys::mpv_log_level::MPV_LOG_LEVEL_NONE.0 as u32,
    Fatal = sys::mpv_log_level::MPV_LOG_LEVEL_FATAL.0 as u32,
    Error = sys::mpv_log_level::MPV_LOG_LEVEL_ERROR.0 as u32,
    Warn = sys::mpv_log_level::MPV_LOG_LEVEL_WARN.0 as u32,
    Info = sys::mpv_log_level::MPV_LOG_LEVEL_INFO.0 as u32,
    Verbose = sys::mpv_log_level::MPV_LOG_LEVEL_V.0 as u32,
    Debug = sys::mpv_log_level::MPV_LOG_LEVEL_DEBUG.0 as u32,
    Trace = sys::mpv_log_level::MPV_LOG_LEVEL_TRACE.0 as u32,
}

impl LogLevel {
    pub fn as_token(self) -> &'static std::ffi::CStr {
        match self {
            LogLevel::Off => c"no",
            LogLevel::Fatal => c"fatal",
            LogLevel::Error => c"error",
            LogLevel::Warn => c"warn",
            LogLevel::Info => c"info",
            LogLevel::Verbose => c"v",
            LogLevel::Debug => c"debug",
            LogLevel::Trace => c"trace",
        }
    }

    pub fn from_raw(raw: sys::mpv_log_level) -> Self {
        match raw {
            sys::mpv_log_level::MPV_LOG_LEVEL_FATAL => LogLevel::Fatal,
            sys::mpv_log_level::MPV_LOG_LEVEL_ERROR => LogLevel::Error,
            sys::mpv_log_level::MPV_LOG_LEVEL_WARN => LogLevel::Warn,
            sys::mpv_log_level::MPV_LOG_LEVEL_INFO => LogLevel::Info,
            sys::mpv_log_level::MPV_LOG_LEVEL_V => LogLevel::Verbose,
            sys::mpv_log_level::MPV_LOG_LEVEL_DEBUG => LogLevel::Debug,
            sys::mpv_log_level::MPV_LOG_LEVEL_TRACE => LogLevel::Trace,
            _ => LogLevel::Off,
        }
    }
}

pub fn forward_to_tracing(msg: &LogMessage) {
    let text = msg.text.trim_end_matches(['\r', '\n']);
    let prefix = msg.prefix.as_str();
    match msg.level {
        LogLevel::Fatal | LogLevel::Error => {
            tracing::event!(target: "mpv", tracing::Level::ERROR, "{}: {}", prefix, text)
        }
        LogLevel::Warn => {
            tracing::event!(target: "mpv", tracing::Level::WARN, "{}: {}", prefix, text)
        }
        LogLevel::Info => {
            tracing::event!(target: "mpv", tracing::Level::INFO, "{}: {}", prefix, text)
        }
        LogLevel::Verbose => {
            tracing::event!(target: "mpv", tracing::Level::DEBUG, "{}: {}", prefix, text)
        }
        LogLevel::Debug => {
            tracing::event!(target: "mpv", tracing::Level::TRACE, "{}: {}", prefix, text)
        }
        LogLevel::Trace | LogLevel::Off => tracing::event!(
            target: "mpv",
            tracing::Level::WARN,
            "[unhandled mpv level {:?}] {}: {}",
            msg.level,
            prefix,
            text
        ),
    }
}
