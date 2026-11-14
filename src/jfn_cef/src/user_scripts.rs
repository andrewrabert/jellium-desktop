// src/jfn_cef/src/user_scripts.rs
//! Loads user-editable JS from disk and injects it per navigation, but only
//! on pages that are NOT the configured Jellyfin server. Files live in
//! <config_dir>/inject/ — edit them and reload the page; no rebuild needed.

use cef::{CefString, Frame, ImplFrame};

fn inject_dir() -> std::path::PathBuf {
    jfn_paths::config_dir().join("inject")
}

/// Best-effort host extraction (scheme://host[:port]/...) without pulling
/// in a URL-parsing crate.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host_and_maybe_more = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let host = host_and_maybe_more
        .split(':')
        .next()
        .unwrap_or(host_and_maybe_more);
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

fn is_jellyfin_host(host: &str) -> bool {
    match host_of(&jfn_config::server_url()) {
        Some(server_host) => server_host == host,
        None => false,
    }
}

/// Runs after `run_user_scripts`. Skips injection entirely on the Jellyfin
/// server's own host; otherwise loads `_global.js` fresh off disk and
/// executes it in the page.
pub(crate) fn inject_for_frame(frame: &Frame) {
    let url = crate::cef_string::userfree_to_string(&frame.url());
    let Some(host) = host_of(&url) else { return };

    if is_jellyfin_host(&host) {
        return;
    }

    let Some(mut code) = read_script("_global.js") else {
        return;
    };
    if code.trim().is_empty() {
        return;
    }

    code = code.replace("__SERVER_URL__", &jfn_config::server_url());
    let code_cef = CefString::from(code.as_str());
    let url_cef = CefString::from(url.as_str());
    frame.execute_java_script(Some(&code_cef), Some(&url_cef), 0);
}

fn read_script(name: &str) -> Option<String> {
    std::fs::read_to_string(inject_dir().join(name)).ok()
}
