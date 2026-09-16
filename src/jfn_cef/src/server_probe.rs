use cef::*;
use parking_lot::Mutex;
use std::sync::Arc;

use cef::rc::Rc;
use cef::{ThreadId, post_task, wrap_task};

use jfn_jellyfin::{extract_base_url, is_valid_public_info, normalize_input};

pub type ProbeCallback = Box<dyn FnOnce(Option<String>) + Send + Sync>;

#[derive(Copy, Clone, PartialEq)]
enum Phase {
    Head,
    Get,
}

struct ProbeState {
    _owner: crossbeam_channel::Sender<std::convert::Infallible>,
    url: String,
    phase: Phase,
    base: String,
    body: Vec<u8>,
    callback: Option<ProbeCallback>,
    active: Option<Urlrequest>,
    cancelled: bool,
    session: Arc<crate::runtime::Session>,
}

pub(crate) struct Probe {
    state: Arc<Mutex<ProbeState>>,
}

impl Probe {
    pub(crate) fn start(
        session: Arc<crate::runtime::Session>,
        url: &str,
        on_done: ProbeCallback,
    ) -> Probe {
        let (owner, disconnected) = crossbeam_channel::unbounded();
        let state = Arc::new(Mutex::new(ProbeState {
            _owner: owner,
            url: normalize_input(url),
            phase: Phase::Head,
            base: String::new(),
            body: Vec::new(),
            callback: Some(on_done),
            active: None,
            cancelled: false,
            session: Arc::clone(&session),
        }));
        let accepted = session
            .dispatch(|| {
                session.track_producer(disconnected);
                let mut task = StartTask::new(Arc::clone(&state));
                post_task(ThreadId::UI, Some(&mut task)) == 1
            })
            .unwrap_or(false);
        if !accepted {
            state.lock().cancelled = true;
            let callback = state.lock().callback.take();
            if let Some(callback) = callback {
                session.dispatch(|| callback(None));
            }
        }
        Probe { state }
    }

    pub(crate) fn cancel_on_ui(self) {
        {
            let mut state = self.state.lock();
            state.cancelled = true;
            state.callback = None;
        }
        cancel_on_ui(&self.state);
    }
}

fn start_on_ui(state: &Arc<Mutex<ProbeState>>) {
    let head_url = {
        let st = state.lock();
        if st.cancelled || !st.session.is_active() {
            return;
        }
        st.url.clone()
    };
    let client = JfnServerProbeClient::new(Arc::clone(state));
    let request = make_request("HEAD", &head_url, client);
    let mut st = state.lock();
    if st.cancelled || !st.session.is_active() {
        if let Some(r) = request {
            r.cancel();
        }
        return;
    }
    match request {
        Some(r) => st.active = Some(r),
        None => {
            jfn_logging::log(
                jfn_logging::Category::Cef,
                jfn_logging::Level::Warn,
                "server probe: CefURLRequest::Create returned null",
            );
            if let Some(f) = st.callback.take() {
                drop(st);
                f(None);
            }
        }
    }
}

fn cancel_on_ui(state: &Arc<Mutex<ProbeState>>) {
    let request = state.lock().active.take();
    if let Some(r) = request {
        r.cancel();
    }
}

wrap_task! {
    struct StartTask {
        state: Arc<Mutex<ProbeState>>,
    }
    impl Task {
        fn execute(&self) {
            let session = Arc::clone(&self.state.lock().session);
            session.dispatch(|| start_on_ui(&self.state));
        }
    }
}

fn on_complete(state: &Arc<Mutex<ProbeState>>, request: &Urlrequest) {
    if !state.lock().session.is_active() {
        cancel_on_ui(state);
        return;
    }
    let next_request = {
        let mut st = state.lock();
        if st.callback.is_none() {
            return;
        }
        if st.phase == Phase::Head {
            let mut resolved = st.url.clone();
            if let Some(resp) = request.response() {
                let url_uf = resp.url();
                let cs: CefString = (&url_uf).into();
                let s = cs.to_string();
                if !s.is_empty() {
                    resolved = s;
                }
            }
            st.base = extract_base_url(&resolved).to_string();
            st.phase = Phase::Get;
            let next_url = format!("{}/System/Info/Public", st.base);
            let client = JfnServerProbeClient::new(Arc::clone(state));
            let next = make_request("GET", &next_url, client);
            st.active = next.clone();
            next
        } else {
            None
        }
    };
    if next_request.is_some() {
        return;
    }

    let (success, base, cb) = {
        let mut st = state.lock();
        let mut ok = false;
        let status = request.request_status();
        if status.as_ref() == &sys::cef_urlrequest_status_t::UR_SUCCESS
            && let Some(resp) = request.response()
            && resp.status() == 200
        {
            ok = is_valid_public_info(&st.body);
        }
        let base = st.base.clone();
        st.active = None;
        let cb = st.callback.take();
        (ok, base, cb)
    };
    if let Some(f) = cb {
        f(success.then_some(base));
    }
}

fn make_request(method: &str, url: &str, client: UrlrequestClient) -> Option<Urlrequest> {
    let req: Request = request_create()?;
    req.set_url(Some(&CefString::from(url)));
    req.set_method(Some(&CefString::from(method)));
    let mut req_arg = req;
    let mut client_arg = client;
    urlrequest_create(Some(&mut req_arg), Some(&mut client_arg), None)
}

cef::wrap_urlrequest_client! {
    struct JfnServerProbeClient {
        state: Arc<Mutex<ProbeState>>,
    }

    impl UrlrequestClient {
        fn on_request_complete(&self, request: Option<&mut Urlrequest>) {
            let Some(req) = request else { return };
            let session = Arc::clone(&self.state.lock().session);
            session.dispatch(|| on_complete(&self.state, req));
        }
        fn on_download_data(
            &self,
            _request: Option<&mut Urlrequest>,
            data: *const u8,
            data_length: usize,
        ) {
            let mut st = self.state.lock();
            if st.phase == Phase::Get && !data.is_null() && data_length > 0 {
                let slice = unsafe { std::slice::from_raw_parts(data, data_length) };
                st.body.extend_from_slice(slice);
            }
        }
    }
}
