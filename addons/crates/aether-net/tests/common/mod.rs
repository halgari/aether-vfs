//! Test support: a local HTTP server that imitates the Nexus API, Nexus's
//! repacked-file host, the Wabbajack CDN and plain HTTP servers, and a
//! builder for Nexus-shaped repacked zips.
#![allow(dead_code)]

pub mod repack;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use aether_net::{Events, Http, HttpConfig, RetryPolicy};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::Response;

pub const API_KEY: &str = "test-key";

/// One request the server saw.
#[derive(Debug, Clone)]
pub struct Logged {
    pub method: String,
    pub path: String,
    pub range: Option<String>,
}

/// A canned response served instead of the real one.
#[derive(Debug, Clone)]
pub struct Canned {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

pub struct ServerState {
    pub base: String,
    files: Mutex<HashMap<String, Arc<Vec<u8>>>>,
    no_range: Mutex<Vec<String>>,
    strict_suffix: Mutex<Vec<String>>,
    delays: Mutex<HashMap<String, u64>>,
    redirects: Mutex<HashMap<String, String>>,
    scripted: Mutex<HashMap<String, VecDeque<Canned>>>,
    endless: Mutex<HashMap<String, u16>>,
    log: Mutex<Vec<Logged>>,
    /// The only `sig` the repacked-file host accepts.
    pub valid_sig: AtomicU64,
    /// Lifetime of issued signed URLs.
    pub url_ttl_secs: AtomicU64,
    /// The account's API allowances, if the API reports them: see
    /// [`TestServer::set_quota`].
    quota: Mutex<Option<Metered>>,
}

/// (limit, remaining) of the hourly and of the daily allowance.
#[derive(Debug, Clone, Copy)]
struct Metered {
    hourly: (u64, u64),
    daily: (u64, u64),
    /// `x-rl-hourly-reset`, if the API says when the hour ends.
    hourly_reset: Option<SystemTime>,
}

#[derive(Clone)]
pub struct TestServer {
    pub base: String,
    pub state: Arc<ServerState>,
}

pub async fn start() -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(ServerState {
        base: base.clone(),
        files: Mutex::default(),
        no_range: Mutex::default(),
        strict_suffix: Mutex::default(),
        delays: Mutex::default(),
        redirects: Mutex::default(),
        scripted: Mutex::default(),
        endless: Mutex::default(),
        log: Mutex::default(),
        valid_sig: AtomicU64::new(1),
        url_ttl_secs: AtomicU64::new(4 * 3600),
        quota: Mutex::default(),
    });
    let app = Router::new().fallback(handle).with_state(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    TestServer { base, state }
}

impl TestServer {
    pub fn put(&self, path: &str, bytes: Vec<u8>) {
        self.state
            .files
            .lock()
            .unwrap()
            .insert(path.to_string(), Arc::new(bytes));
    }
    /// Serve a Nexus repacked zip for `uid`.
    pub fn put_repacked(&self, uid: u64, zip: Vec<u8>) {
        self.put(&format!("/repacked/{uid}"), zip);
    }
    pub fn no_range(&self, path: &str) {
        self.state.no_range.lock().unwrap().push(path.to_string());
    }
    /// Answer a suffix range (`bytes=-N`) longer than the file at `path`
    /// with the 500 the Nexus host gives a range past the end, instead of
    /// the whole file.
    pub fn strict_suffix(&self, path: &str) {
        self.state
            .strict_suffix
            .lock()
            .unwrap()
            .push(path.to_string());
    }
    /// Answer every request to `path` only after `ms` milliseconds (it is
    /// in the log as soon as it arrives).
    pub fn delay(&self, path: &str, ms: u64) {
        self.state
            .delays
            .lock()
            .unwrap()
            .insert(path.to_string(), ms);
    }
    pub fn redirect(&self, from: &str, to: &str) {
        self.state
            .redirects
            .lock()
            .unwrap()
            .insert(from.to_string(), to.to_string());
    }
    /// Serve `c` for the next request to `path`, before any real response.
    pub fn script(&self, path: &str, c: Canned) {
        self.state
            .scripted
            .lock()
            .unwrap()
            .entry(path.to_string())
            .or_default()
            .push_back(c);
    }
    /// Answer every request to `path` with `status` and a body that never
    /// ends (64 KiB chunks forever).
    pub fn endless_error(&self, path: &str, status: u16) {
        self.state
            .endless
            .lock()
            .unwrap()
            .insert(path.to_string(), status);
    }
    /// From now on the API counts its requests as the real one does: each
    /// request (answered or refused) takes one from both allowances, given
    /// as (limit, remaining), and its response says what is left in
    /// `x-rl-hourly-*` and `x-rl-daily-*` headers.
    pub fn set_quota(&self, hourly: (u64, u64), daily: (u64, u64)) {
        *self.state.quota.lock().unwrap() = Some(Metered {
            hourly,
            daily,
            hourly_reset: None,
        });
    }
    /// [`set_quota`](Self::set_quota), with responses that also say the
    /// hourly allowance is full again at `reset` (`x-rl-hourly-reset`, in
    /// the real API's form: `2026-10-02 18:00:00 +0000`).
    pub fn set_quota_until(&self, hourly: (u64, u64), daily: (u64, u64), reset: SystemTime) {
        *self.state.quota.lock().unwrap() = Some(Metered {
            hourly,
            daily,
            hourly_reset: Some(reset),
        });
    }
    /// Back to an API that reports no allowance.
    pub fn no_quota(&self) {
        *self.state.quota.lock().unwrap() = None;
    }
    /// What is left of the (hourly, daily) allowance set by
    /// [`set_quota`](Self::set_quota).
    pub fn quota_left(&self) -> (u64, u64) {
        let q = self.state.quota.lock().unwrap().expect("set_quota");
        (q.hourly.1, q.daily.1)
    }
    pub fn requests(&self, path_prefix: &str) -> Vec<Logged> {
        self.state
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.path.starts_with(path_prefix))
            .cloned()
            .collect()
    }
    pub fn clear_log(&self) {
        self.state.log.lock().unwrap().clear();
    }
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

/// An `Http` with fast retries and small chunks, so tests run quickly.
pub fn http(events: Events) -> Http {
    Http::new(
        HttpConfig {
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(5),
                max_retry_after: Duration::from_secs(2),
            },
            chunk_size: 64 << 10,
            parallel_parts: 4,
            ..HttpConfig::default()
        },
        events,
    )
    .unwrap()
}

fn respond(status: u16, headers: &[(&str, String)], body: Vec<u8>) -> Response {
    let mut r = Response::builder().status(StatusCode::from_u16(status).unwrap());
    for (k, v) in headers {
        r = r.header(*k, v);
    }
    r.body(Body::from(body)).unwrap()
}

fn problem(status: u16, detail: &str) -> Response {
    let body = serde_json::json!({ "status": status, "detail": detail }).to_string();
    respond(
        status,
        &[("content-type", "application/problem+json".into())],
        body.into_bytes(),
    )
}

async fn handle(State(s): State<Arc<ServerState>>, req: Request) -> Response {
    let api = ["/v1/", "/v3/"]
        .iter()
        .any(|p| req.uri().path().starts_with(p));
    let mut resp = answer(&s, req).await;
    if api && let Some(q) = s.quota.lock().unwrap().as_mut() {
        q.hourly.1 = q.hourly.1.saturating_sub(1);
        q.daily.1 = q.daily.1.saturating_sub(1);
        for (name, n) in [
            ("x-rl-hourly-limit", q.hourly.0),
            ("x-rl-hourly-remaining", q.hourly.1),
            ("x-rl-daily-limit", q.daily.0),
            ("x-rl-daily-remaining", q.daily.1),
        ] {
            resp.headers_mut().insert(name, n.into());
        }
        if let Some(reset) = q.hourly_reset {
            let text = humantime::format_rfc3339_seconds(reset).to_string();
            let text = text.replace('T', " ").replace('Z', " +0000");
            resp.headers_mut()
                .insert("x-rl-hourly-reset", text.parse().unwrap());
        }
    }
    resp
}

async fn answer(s: &Arc<ServerState>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let key_ok = req.headers().get("apikey").and_then(|v| v.to_str().ok()) == Some(API_KEY);
    s.log.lock().unwrap().push(Logged {
        method: req.method().to_string(),
        path: path.clone(),
        range: range.clone(),
    });
    let delay = s.delays.lock().unwrap().get(&path).copied();
    if let Some(ms) = delay {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    if let Some(status) = s.endless.lock().unwrap().get(&path).copied() {
        static CHUNK: [u8; 64 << 10] = [b'x'; 64 << 10];
        let body = futures_util::stream::repeat_with(|| {
            Ok::<_, std::io::Error>(axum::body::Bytes::from_static(&CHUNK))
        });
        return Response::builder()
            .status(StatusCode::from_u16(status).unwrap())
            .body(Body::from_stream(body))
            .unwrap();
    }
    if let Some(c) = s
        .scripted
        .lock()
        .unwrap()
        .get_mut(&path)
        .and_then(VecDeque::pop_front)
    {
        return respond(c.status, &c.headers, c.body);
    }
    if let Some(to) = s.redirects.lock().unwrap().get(&path) {
        return respond(302, &[("location", to.clone())], Vec::new());
    }
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (req.method().clone(), parts.as_slice()) {
        (Method::POST, ["v3", "mod-file-versions", uid, "download-repacked"]) => {
            if !key_ok {
                return problem(401, "Please provide a valid API Key");
            }
            if !s
                .files
                .lock()
                .unwrap()
                .contains_key(&format!("/repacked/{uid}"))
            {
                return problem(404, &format!("Mod file version not found: {uid}"));
            }
            let ttl = s.url_ttl_secs.load(Ordering::SeqCst);
            let exp = SystemTime::now() + Duration::from_secs(ttl);
            let body = serde_json::json!({
                "download_url": format!("{}/repacked/{uid}?exp=0&kid=1&sig={}", s.base, s.valid_sig.load(Ordering::SeqCst)),
                "expires_at": humantime::format_rfc3339_seconds(exp).to_string(),
            });
            respond(200, &[], body.to_string().into_bytes())
        }
        (Method::GET, ["v3", "games", _domain, "mod-file-versions", file_id]) => {
            if !key_ok {
                return problem(401, "Please provide a valid API Key");
            }
            let id: u64 = file_id.parse().unwrap();
            let body = serde_json::json!({ "data": {
                "id": ((1704u64 << 32) + id).to_string(),
                "name": "Test File", "version": "1.0", "category": "main",
            }});
            respond(200, &[], body.to_string().into_bytes())
        }
        (Method::GET, ["v1", "users", "validate.json"]) => {
            if !key_ok {
                return problem(401, "Please provide a valid API Key");
            }
            // The real endpoint echoes the key back.
            let body = serde_json::json!({
                "user_id": 1, "key": API_KEY, "name": "Tester", "email": "t@example.com",
                "is_premium": true, "is_premium?": true, "is_supporter": true,
            });
            respond(200, &[], body.to_string().into_bytes())
        }
        (Method::GET, ["repacked", _]) => {
            let sig = format!("sig={}", s.valid_sig.load(Ordering::SeqCst));
            if !query.split('&').any(|kv| kv == sig) {
                return respond(403, &[], b"Invalid signature".to_vec());
            }
            serve_file(s, &path, range, true)
        }
        (Method::GET, _) => serve_file(s, &path, range, false),
        _ => respond(405, &[], Vec::new()),
    }
}

fn serve_file(s: &ServerState, path: &str, range: Option<String>, nexus: bool) -> Response {
    let Some(data) = s.files.lock().unwrap().get(path).cloned() else {
        return respond(404, &[], b"not found".to_vec());
    };
    let honour = !s.no_range.lock().unwrap().iter().any(|p| p == path);
    let len = data.len() as u64;
    match range.filter(|_| honour) {
        None => respond(200, &[("accept-ranges", "bytes".into())], data.to_vec()),
        Some(r) => {
            let spec = r.strip_prefix("bytes=").unwrap();
            let (a, b) = spec.split_once('-').unwrap();
            if a.is_empty() {
                // A suffix range: the last `n` bytes, or the whole file
                // when it is shorter (RFC 9110 §14.1.2).
                let n: u64 = b.parse().unwrap();
                let strict = s.strict_suffix.lock().unwrap().iter().any(|p| p == path);
                if n == 0 || len == 0 || (strict && n > len) {
                    return if nexus {
                        respond(500, &[], b"error code: 1101".to_vec())
                    } else {
                        respond(
                            416,
                            &[("content-range", format!("bytes */{len}"))],
                            Vec::new(),
                        )
                    };
                }
                let a = len.saturating_sub(n);
                return respond(
                    206,
                    &[("content-range", format!("bytes {a}-{}/{len}", len - 1))],
                    data[a as usize..].to_vec(),
                );
            }
            let a: u64 = a.parse().unwrap();
            let b: u64 = if b.is_empty() {
                len - 1
            } else {
                b.parse().unwrap()
            };
            if a >= len {
                // Nexus's host crashes instead of answering 416.
                return if nexus {
                    respond(500, &[], b"error code: 1101".to_vec())
                } else {
                    respond(
                        416,
                        &[("content-range", format!("bytes */{len}"))],
                        Vec::new(),
                    )
                };
            }
            let b = b.min(len - 1);
            respond(
                206,
                &[("content-range", format!("bytes {a}-{b}/{len}"))],
                data[a as usize..=b as usize].to_vec(),
            )
        }
    }
}
