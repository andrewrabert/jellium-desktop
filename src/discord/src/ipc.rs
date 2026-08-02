use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use serde_json::{Value, json};

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;

const MAX_FRAME_LEN: u32 = 64 * 1024;

trait Stream: Read + Write + Send {
    fn try_clone_boxed(&self) -> io::Result<Box<dyn Stream>>;
    fn shutdown(&self);
}

#[cfg(unix)]
mod platform {
    use super::Stream;
    use std::io;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};

    impl Stream for UnixStream {
        fn try_clone_boxed(&self) -> io::Result<Box<dyn Stream>> {
            Ok(Box::new(self.try_clone()?))
        }
        fn shutdown(&self) {
            let _ = UnixStream::shutdown(self, std::net::Shutdown::Both);
        }
    }

    const NESTED: [&str; 4] = [
        "app/com.discordapp.Discord",
        "app/com.discordapp.DiscordCanary",
        "app/com.discordapp.DiscordPTB",
        "snap.discord",
    ];

    pub fn candidates() -> Vec<PathBuf> {
        let mut bases: Vec<PathBuf> = Vec::new();
        for key in ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"] {
            if let Ok(v) = std::env::var(key)
                && !v.is_empty()
            {
                bases.push(PathBuf::from(v));
            }
        }
        bases.push(PathBuf::from("/tmp"));
        bases.dedup();

        let mut out = Vec::new();
        for base in &bases {
            for i in 0..10 {
                out.push(base.join(format!("discord-ipc-{i}")));
            }
            for nested in NESTED {
                for i in 0..10 {
                    out.push(base.join(nested).join(format!("discord-ipc-{i}")));
                }
            }
        }
        out
    }

    pub fn connect(path: &Path) -> io::Result<Box<dyn Stream>> {
        Ok(Box::new(UnixStream::connect(path)?))
    }
}

#[cfg(windows)]
mod platform {
    use super::Stream;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::{Path, PathBuf};

    impl Stream for File {
        fn try_clone_boxed(&self) -> io::Result<Box<dyn Stream>> {
            Ok(Box::new(self.try_clone()?))
        }
        fn shutdown(&self) {}
    }

    pub fn candidates() -> Vec<PathBuf> {
        (0..10)
            .map(|i| PathBuf::from(format!(r"\\.\pipe\discord-ipc-{i}")))
            .collect()
    }

    pub fn connect(path: &Path) -> io::Result<Box<dyn Stream>> {
        Ok(Box::new(
            OpenOptions::new().read(true).write(true).open(path)?,
        ))
    }
}

fn write_frame<W: Write + ?Sized>(w: &mut W, op: u32, payload: &[u8]) -> io::Result<()> {
    let len =
        u32::try_from(payload.len()).map_err(|_| io::Error::other("discord: frame too large"))?;
    let mut buf = Vec::with_capacity(8 + payload.len());
    buf.extend_from_slice(&op.to_le_bytes());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)
}

fn read_frame<R: Read + ?Sized>(r: &mut R) -> io::Result<(u32, Vec<u8>)> {
    let mut hdr = [0u8; 8];
    r.read_exact(&mut hdr)?;
    let op = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    if len > MAX_FRAME_LEN {
        return Err(io::Error::other(format!("discord: frame too large: {len}")));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok((op, payload))
}

pub struct Connection {
    stream: Arc<Mutex<Box<dyn Stream>>>,
    alive: Arc<AtomicBool>,
}

impl Connection {
    pub fn connect(application_id: &str) -> Option<Self> {
        for path in platform::candidates() {
            if let Some(conn) = Self::try_path(&path, application_id) {
                tracing::info!(target: "Media", "discord: connected via {}", path.display());
                return Some(conn);
            }
        }
        None
    }

    fn try_path(path: &Path, application_id: &str) -> Option<Self> {
        let stream = platform::connect(path).ok()?;
        let reader = stream.try_clone_boxed().ok()?;
        let shared = Arc::new(Mutex::new(stream));

        let hello = json!({ "v": 1, "client_id": application_id }).to_string();
        if write_frame(&mut **shared.lock(), OP_HANDSHAKE, hello.as_bytes()).is_err() {
            return None;
        }

        let alive = Arc::new(AtomicBool::new(true));
        spawn_reader(reader, Arc::clone(&shared), Arc::clone(&alive));
        Some(Self {
            stream: shared,
            alive,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub fn send_activity(&self, activity: Option<Value>, nonce: u64) -> bool {
        let payload = json!({
            "cmd": "SET_ACTIVITY",
            "nonce": nonce.to_string(),
            "args": { "pid": std::process::id(), "activity": activity },
        })
        .to_string();

        let failed = {
            let mut guard = self.stream.lock();
            write_frame(&mut **guard, OP_FRAME, payload.as_bytes()).is_err()
        };
        if failed {
            self.close();
            return false;
        }
        true
    }

    pub fn close(&self) {
        self.alive.store(false, Ordering::Release);
        self.stream.lock().shutdown();
    }
}

fn spawn_reader(
    mut reader: Box<dyn Stream>,
    writer: Arc<Mutex<Box<dyn Stream>>>,
    alive: Arc<AtomicBool>,
) {
    let on_exit = Arc::clone(&alive);
    let spawned = std::thread::Builder::new()
        .name("discord-rpc-read".into())
        .spawn(move || {
            loop {
                match read_frame(&mut *reader) {
                    Ok((OP_PING, payload)) => {
                        if write_frame(&mut **writer.lock(), OP_PONG, &payload).is_err() {
                            break;
                        }
                    }
                    Ok((OP_CLOSE, payload)) => {
                        tracing::info!(
                            target: "Media",
                            "discord: connection closed by client: {}",
                            String::from_utf8_lossy(&payload)
                        );
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            on_exit.store(false, Ordering::Release);
        });

    if spawned.is_err() {
        alive.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, OP_FRAME, b"{\"a\":1}").ok();
        let (op, payload) = match read_frame(&mut buf.as_slice()) {
            Ok(v) => v,
            Err(_) => unreachable!("round trip"),
        };
        assert_eq!(op, OP_FRAME);
        assert_eq!(payload, b"{\"a\":1}");
    }

    #[test]
    fn header_is_little_endian() {
        let mut buf = Vec::new();
        write_frame(&mut buf, OP_HANDSHAKE, b"hi").ok();
        assert_eq!(&buf[0..4], &[0, 0, 0, 0]);
        assert_eq!(&buf[4..8], &[2, 0, 0, 0]);
        assert_eq!(&buf[8..], b"hi");
    }

    #[test]
    fn oversized_length_is_rejected_before_allocating() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&OP_FRAME.to_le_bytes());
        frame.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_frame(&mut frame.as_slice()).is_err());
    }

    #[test]
    fn truncated_frame_is_an_error_not_a_panic() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&OP_FRAME.to_le_bytes());
        frame.extend_from_slice(&64u32.to_le_bytes());
        frame.extend_from_slice(b"short");
        assert!(read_frame(&mut frame.as_slice()).is_err());
    }

    #[test]
    fn candidates_are_probed_in_index_order() {
        let c = platform::candidates();
        assert!(!c.is_empty());
        let first = c.first().map(|p| p.to_string_lossy().into_owned());
        assert_eq!(
            first.as_deref().map(|s| s.ends_with("discord-ipc-0")),
            Some(true)
        );
    }
}
