#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fmt::Debug;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use jfn_instance_ipc::{Listener, Start, Stream};
use jfn_platform_abi::Instance;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::runtime::Runtime;

#[derive(Debug, Serialize, Deserialize)]
struct Req {
    payload: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Resp {
    len: usize,
}

fn echo_len(r: &Req) -> Resp {
    Resp {
        len: r.payload.len(),
    }
}

fn tag_a(_r: &Req) -> Resp {
    Resp { len: 1 }
}

fn tag_b(_r: &Req) -> Resp {
    Resp { len: 2 }
}

fn rt() -> Runtime {
    Runtime::new().unwrap()
}

fn scratch_instance() -> (TempDir, Instance) {
    let dir = tempfile::tempdir().unwrap();
    let instance = Instance::for_config_dir(dir.path()).unwrap();
    (dir, instance)
}

fn serving<Req, Resp>(rt: &Runtime, instance: &Instance, handle: fn(&Req) -> Resp) -> Listener
where
    Req: DeserializeOwned + Debug + Send + 'static,
    Resp: Serialize + Send + Sync + 'static,
{
    match rt.block_on(Listener::try_start(instance, handle)) {
        Start::Started(listener) => listener,
        Start::AlreadyRunning => panic!("unexpected AlreadyRunning"),
        Start::Failed(e) => panic!("start failed: {e}"),
    }
}

#[test]
fn round_trip_no_truncation() {
    let rt = rt();
    let (_dir, instance) = scratch_instance();
    let _listener = serving(&rt, &instance, echo_len);
    let len = rt.block_on(async {
        let mut stream = Stream::connect(&instance).await.unwrap();
        stream
            .send(&Req {
                payload: "x".repeat(4096),
            })
            .await
            .unwrap();
        stream.recv::<Resp>().await.unwrap().unwrap().len
    });
    assert_eq!(len, 4096);
}

#[test]
fn many_frames_one_connection() {
    let rt = rt();
    let (_dir, instance) = scratch_instance();
    let _listener = serving(&rt, &instance, echo_len);
    rt.block_on(async {
        let mut stream = Stream::connect(&instance).await.unwrap();
        stream
            .send(&Req {
                payload: "abc".into(),
            })
            .await
            .unwrap();
        assert_eq!(stream.recv::<Resp>().await.unwrap().unwrap().len, 3);
        stream
            .send(&Req {
                payload: "hello".into(),
            })
            .await
            .unwrap();
        assert_eq!(stream.recv::<Resp>().await.unwrap().unwrap().len, 5);
    });
}

#[test]
fn second_bind_reports_already_running() {
    let rt = rt();
    let (_dir, instance) = scratch_instance();
    let _first = serving(&rt, &instance, echo_len);
    match rt.block_on(Listener::try_start(&instance, echo_len)) {
        Start::AlreadyRunning => {}
        Start::Started(_) => panic!("expected AlreadyRunning, got Started"),
        Start::Failed(e) => panic!("expected AlreadyRunning, got Failed: {e}"),
    }
}

#[cfg(unix)]
#[test]
fn stale_socket_is_reclaimed() {
    let rt = rt();
    let (_dir, instance) = scratch_instance();
    let path = jfn_instance_ipc::Name::for_instance(&instance)
        .unwrap()
        .path()
        .to_path_buf();
    // Only a bound-then-dropped socket yields ECONNREFUSED on connect; a plain
    // file gives ENOTSOCK and never reaches the stale path.
    let dead = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(dead);
    assert!(path.exists());

    let _listener = serving(&rt, &instance, echo_len);
    let len = rt.block_on(async {
        let mut stream = Stream::connect(&instance).await.unwrap();
        stream
            .send(&Req {
                payload: "zz".into(),
            })
            .await
            .unwrap();
        stream.recv::<Resp>().await.unwrap().unwrap().len
    });
    assert_eq!(len, 2);
}

#[cfg(unix)]
#[test]
fn clean_drop_frees_name() {
    let rt = rt();
    let (_dir, instance) = scratch_instance();
    let path = jfn_instance_ipc::Name::for_instance(&instance)
        .unwrap()
        .path()
        .to_path_buf();

    let listener = serving(&rt, &instance, echo_len);
    assert!(path.exists());
    rt.block_on(listener.shutdown());
    assert!(!path.exists());

    let _again = serving(&rt, &instance, echo_len);
}

#[test]
fn drop_with_open_connection_does_not_hang() {
    let rt = rt();
    let (_dir, instance) = scratch_instance();
    let listener = serving(&rt, &instance, echo_len);
    let _client = rt.block_on(Stream::connect(&instance)).unwrap();

    let (tx, rx) = mpsc::channel();
    let joiner = thread::spawn(move || {
        drop(listener);
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("dropping Listener with an open connection hung");
    joiner.join().unwrap();
}

#[test]
fn distinct_instances_are_isolated() {
    let rt = rt();
    let (_dir_a, a) = scratch_instance();
    let (_dir_b, b) = scratch_instance();
    let _la = serving(&rt, &a, tag_a);
    let _lb = serving(&rt, &b, tag_b);

    let (ra, rb) = rt.block_on(async {
        let mut sa = Stream::connect(&a).await.unwrap();
        sa.send(&Req {
            payload: String::new(),
        })
        .await
        .unwrap();
        let ra = sa.recv::<Resp>().await.unwrap().unwrap().len;

        let mut sb = Stream::connect(&b).await.unwrap();
        sb.send(&Req {
            payload: String::new(),
        })
        .await
        .unwrap();
        let rb = sb.recv::<Resp>().await.unwrap().unwrap().len;
        (ra, rb)
    });
    assert_eq!(ra, 1);
    assert_eq!(rb, 2);
}
