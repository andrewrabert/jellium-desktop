use std::fmt::Debug;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::tokio::{Listener as SocketListener, Stream as SocketStream};
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name as SocketName, ToFsName};
use jfn_platform_abi::Instance;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, WIN32_ERROR,
};

pub mod jfn;

pub struct Name {
    path: PathBuf,
}

impl Name {
    pub fn for_instance(instance: &Instance) -> io::Result<Self> {
        Ok(Self {
            path: jfn_paths::instance_listener_path(instance.id())?,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn to_socket(&self) -> io::Result<SocketName<'static>> {
        self.path.clone().to_fs_name::<GenericFilePath>()
    }
}

pub struct Stream {
    inner: BufReader<SocketStream>,
}

impl Stream {
    pub async fn connect(instance: &Instance) -> io::Result<Self> {
        let name = Name::for_instance(instance)?;
        let stream = SocketStream::connect(name.to_socket()?).await?;
        Ok(Self::wrap(stream))
    }

    fn wrap(stream: SocketStream) -> Self {
        Self {
            inner: BufReader::new(stream),
        }
    }

    pub async fn send<M: Serialize>(&mut self, msg: &M) -> io::Result<()> {
        let mut bytes =
            serde_json::to_vec(msg).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
        bytes.push(b'\n');
        let stream = self.inner.get_mut();
        stream.write_all(&bytes).await?;
        stream.flush().await
    }

    pub async fn recv<M: DeserializeOwned>(&mut self) -> io::Result<Option<M>> {
        let mut line = String::new();
        if self.inner.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        serde_json::from_str(line.trim_end())
            .map(Some)
            .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))
    }
}

#[must_use]
pub enum Start {
    Started(Listener),
    AlreadyRunning,
    Failed(io::Error),
}

pub struct Listener {
    shutdown: Arc<Notify>,
    accept: Option<JoinHandle<()>>,
}

impl Listener {
    pub async fn try_start<Req, Resp>(instance: &Instance, handle: fn(&Req) -> Resp) -> Start
    where
        Req: DeserializeOwned + Debug + Send + 'static,
        Resp: Serialize + Send + Sync + 'static,
    {
        match Name::for_instance(instance) {
            Ok(name) => Self::create(&name, handle).await,
            Err(e) => Start::Failed(e),
        }
    }

    async fn create<Req, Resp>(name: &Name, handle: fn(&Req) -> Resp) -> Start
    where
        Req: DeserializeOwned + Debug + Send + 'static,
        Resp: Serialize + Send + Sync + 'static,
    {
        match Self::make(name, false) {
            Ok(listener) => Self::spawn(listener, handle),
            Err(held) if name_held(&held) => match Self::probe(name).await {
                Probe::AlreadyRunning => Start::AlreadyRunning,
                // Windows ignores `try_overwrite`, so this re-bind is the
                // second opinion that turns a genuine denial back into the
                // denial itself.
                Probe::Vacant => match Self::make(name, true) {
                    Ok(listener) => Self::spawn(listener, handle),
                    Err(e) => Start::Failed(e),
                },
                Probe::Failed(e) => Start::Failed(e),
            },
            Err(e) => Start::Failed(e),
        }
    }

    fn make(name: &Name, overwrite: bool) -> io::Result<SocketListener> {
        ListenerOptions::new()
            .name(name.to_socket()?)
            .try_overwrite(overwrite)
            .create_tokio()
    }

    async fn probe(name: &Name) -> Probe {
        let sock = match name.to_socket() {
            Ok(sock) => sock,
            Err(e) => return Probe::Failed(e),
        };
        match SocketStream::connect(sock).await {
            Ok(_) => Probe::AlreadyRunning,
            Err(e) if name_vacant(&e) => Probe::Vacant,
            // Unreachable ≠ dead — never classify (and later reclaim) as vacant.
            Err(e) => Probe::Failed(e),
        }
    }

    fn spawn<Req, Resp>(listener: SocketListener, handle: fn(&Req) -> Resp) -> Start
    where
        Req: DeserializeOwned + Debug + Send + 'static,
        Resp: Serialize + Send + Sync + 'static,
    {
        let shutdown = Arc::new(Notify::new());
        let accept = tokio::spawn(accept_loop(listener, handle, shutdown.clone()));
        Start::Started(Listener {
            shutdown,
            accept: Some(accept),
        })
    }

    pub async fn shutdown(mut self) {
        self.shutdown.notify_waiters();
        if let Some(accept) = self.accept.take() {
            accept.abort();
            let _ = accept.await;
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.shutdown.notify_waiters();
        if let Some(accept) = self.accept.take() {
            accept.abort();
        }
    }
}

enum Probe {
    AlreadyRunning,
    Vacant,
    Failed(io::Error),
}

/// `bind` reports `EADDRINUSE` for a socket file that already exists, whether
/// or not anything still listens on it. Linux and macOS agree here.
#[cfg(unix)]
fn name_held(e: &io::Error) -> bool {
    e.kind() == ErrorKind::AddrInUse
}

/// The listener instance carries `FILE_FLAG_FIRST_PIPE_INSTANCE`, so
/// `CreateNamedPipeW` answers every instance after the first with
/// `ERROR_ACCESS_DENIED` — the same code a real privilege failure reports, so
/// the probe, not this function, decides which one happened.
#[cfg(windows)]
fn name_held(e: &io::Error) -> bool {
    is_win32_error(e, ERROR_ACCESS_DENIED)
}

/// A socket file whose listener is gone refuses connections.
#[cfg(unix)]
fn name_vacant(e: &io::Error) -> bool {
    e.kind() == ErrorKind::ConnectionRefused
}

/// A named pipe exists only while some instance of it is open, so no pipe of
/// that name means nothing holds the name and the bind's `ERROR_ACCESS_DENIED`
/// was a privilege failure. A pipe held under a denying ACL answers
/// `ERROR_ACCESS_DENIED` here instead and stays `Probe::Failed`.
#[cfg(windows)]
fn name_vacant(e: &io::Error) -> bool {
    is_win32_error(e, ERROR_FILE_NOT_FOUND) || is_win32_error(e, ERROR_PATH_NOT_FOUND)
}

#[cfg(windows)]
fn is_win32_error(e: &io::Error, code: WIN32_ERROR) -> bool {
    e.raw_os_error()
        .is_some_and(|raw| raw.cast_unsigned() == code)
}

async fn accept_loop<Req, Resp>(
    listener: SocketListener,
    handle: fn(&Req) -> Resp,
    shutdown: Arc<Notify>,
) where
    Req: DeserializeOwned + Debug + Send + 'static,
    Resp: Serialize + Send + Sync + 'static,
{
    loop {
        let conn = tokio::select! {
            () = shutdown.notified() => break,
            accepted = listener.accept() => match accepted {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::warn!("accept: {e}");
                    continue;
                }
            },
        };
        tokio::spawn(serve(Stream::wrap(conn), handle, shutdown.clone()));
    }
}

async fn serve<Req, Resp>(mut stream: Stream, handle: fn(&Req) -> Resp, shutdown: Arc<Notify>)
where
    Req: DeserializeOwned + Debug + Send + 'static,
    Resp: Serialize + Send + Sync + 'static,
{
    loop {
        let received = tokio::select! {
            () = shutdown.notified() => break,
            received = stream.recv::<Req>() => received,
        };
        match received {
            Ok(Some(req)) => {
                tracing::debug!("received {req:?}");
                let resp = handle(&req);
                if let Err(e) = stream.send(&resp).await {
                    tracing::warn!("send response: {e}");
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("recv: {e}");
                break;
            }
        }
    }
}
