use std::process::{Command, Stdio};
use std::thread;

pub fn open(url: &str) {
    let child = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    match child {
        Ok(mut child) => {
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => {
            tracing::error!("spawn(xdg-open) failed: {}", e);
        }
    }
}
