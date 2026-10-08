//! A local imitation of GOG's auth server, content-system API and CDN, in
//! the shape of `aether-net`'s test servers. Shared by the test crates with
//! `mod fake_gog;`.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_gog::{Chunk, DepotItem, DepotManifest, GogConfig};
use aether_net::{Events, Http, HttpConfig, RetryPolicy};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use md5::{Digest, Md5};
use serde_json::json;
use url::Url;

/// The only authorization code the fake accepts.
pub const CODE: &str = "the-login-code";
pub const CLIENT_ID: &str = "test-client";
pub const CLIENT_SECRET: &str = "test-secret";
pub const REDIRECT: &str = "https://embed.gog.com/on_login_success?origin=client";
pub const USER_ID: &str = "4242";

/// The game and a DLC whose depot the game's build lists.
pub const GAME: u64 = 1207658691;
pub const DLC: u64 = 1207658692;
pub const BUILD_ID: &str = "56452082907692588";
pub const BUILD_META: &str = "92ab0000000000000000000000000001";
pub const DEPOT_A: &str = "aa110000000000000000000000000002";
pub const DEPOT_B: &str = "bb220000000000000000000000000003";

/// Raw chunk size of `Data\Big.bin`.
pub const CHUNK: usize = 4096;

/// `Data\Big.bin`: three chunks of 4 KiB.
pub fn big() -> Vec<u8> {
    (0..3 * CHUNK as u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect()
}

/// `Readme.TXT`: one chunk.
pub fn readme() -> Vec<u8> {
    b"Readme for the test game.\n".repeat(4)
}

/// The depot's small-files container: one chunk.
pub fn sfc() -> Vec<u8> {
    (0..64u8).collect()
}

/// `Data\Small.ini` lives at bytes 10..30 of the small-files container.
pub fn small_ini() -> Vec<u8> {
    sfc()[10..30].to_vec()
}

/// `Data\DLC.esp` in the DLC's depot.
pub fn dlc_esp() -> Vec<u8> {
    b"TES4 dlc plugin".to_vec()
}

pub fn md5_hex(b: &[u8]) -> String {
    hex(&Md5::digest(b))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn zlib(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub struct ServerState {
    pub base: String,
    /// Path and query of every request, in order.
    log: Mutex<Vec<String>>,
    /// Zlib-compressed meta documents by path.
    meta: Mutex<HashMap<String, Vec<u8>>>,
    /// Compressed chunk bodies by compressed MD5 (hex).
    chunks: Mutex<HashMap<String, Vec<u8>>>,
    /// Chunks served with a byte flipped.
    corrupt: Mutex<HashSet<String>>,
    issued: AtomicU32,
    access: Mutex<HashSet<String>>,
    refresh: Mutex<Option<String>>,
    code_calls: AtomicU32,
    refresh_calls: AtomicU32,
    fail_refresh: AtomicBool,
    last_token_query: Mutex<Vec<(String, String)>>,
    /// Milliseconds every CDN response waits before it is sent.
    cdn_delay_ms: AtomicU64,
    /// CDN requests being answered now, and the most seen at once.
    cdn_in_flight: AtomicU32,
    cdn_max_in_flight: AtomicU32,
}

#[derive(Clone)]
pub struct FakeGog {
    pub base: String,
    pub state: Arc<ServerState>,
}

/// One chunk record as the manifest lists it; the compressed body is
/// registered with the server.
fn chunk(s: &ServerState, raw: &[u8]) -> serde_json::Value {
    let packed = zlib(raw);
    let cmd5 = md5_hex(&packed);
    let rec = json!({
        "md5": md5_hex(raw),
        "size": raw.len(),
        "compressedMd5": cmd5,
        "compressedSize": packed.len(),
    });
    s.chunks.lock().unwrap().insert(cmd5, packed);
    rec
}

fn meta_path(id: &str) -> String {
    format!("/content-system/v2/meta/{}/{}/{id}", &id[..2], &id[2..4])
}

pub async fn start() -> FakeGog {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(ServerState {
        base: base.clone(),
        log: Mutex::default(),
        meta: Mutex::default(),
        chunks: Mutex::default(),
        corrupt: Mutex::default(),
        issued: AtomicU32::new(0),
        access: Mutex::default(),
        refresh: Mutex::default(),
        code_calls: AtomicU32::new(0),
        refresh_calls: AtomicU32::new(0),
        fail_refresh: AtomicBool::new(false),
        last_token_query: Mutex::default(),
        cdn_delay_ms: AtomicU64::new(0),
        cdn_in_flight: AtomicU32::new(0),
        cdn_max_in_flight: AtomicU32::new(0),
    });
    install_fixtures(&state);
    let app = Router::new().fallback(handle).with_state(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakeGog { base, state }
}

fn install_fixtures(s: &ServerState) {
    let big = big();
    let big_chunks: Vec<_> = big.chunks(CHUNK).map(|c| chunk(s, c)).collect();
    let depot_a = json!({
        "version": 2,
        "depot": {
            "items": [
                { "type": "DepotDirectory", "path": "Data" },
                { "type": "DepotFile", "path": "Data\\Big.bin", "chunks": big_chunks },
                { "type": "DepotFile", "path": "Readme.TXT", "md5": md5_hex(&readme()),
                  "chunks": [chunk(s, &readme())] },
                { "type": "DepotLink", "path": "Link.txt", "target": "Readme.TXT" },
                { "type": "DepotFile", "path": "Data\\Small.ini", "flags": ["executable"],
                  "sfcRef": { "offset": 10, "size": 20 },
                  "chunks": [chunk(s, &small_ini())] },
            ],
            "smallFilesContainer": { "chunks": [chunk(s, &sfc())] },
        },
    });
    let depot_b = json!({
        "version": 2,
        "depot": { "items": [
            { "type": "DepotFile", "path": "Data\\DLC.esp", "chunks": [chunk(s, &dlc_esp())] },
        ]},
    });
    let details = json!({
        "baseProductId": GAME.to_string(),
        "buildId": BUILD_ID,
        "installDirectory": "Test Game",
        "version": 2,
        "depots": [
            { "productId": GAME.to_string(), "manifest": DEPOT_A, "size": 12_000,
              "compressedSize": 9_000, "languages": ["*"], "osBitness": ["64"] },
            { "productId": DLC.to_string(), "manifest": DEPOT_B, "size": 15,
              "compressedSize": 20, "languages": ["en-US", "de-DE"] },
        ],
        "offlineDepot": { "productId": GAME.to_string(), "manifest": "ff00", "size": 0,
                          "compressedSize": 0, "languages": ["*"] },
    });
    let mut meta = s.meta.lock().unwrap();
    for (id, doc) in [
        (DEPOT_A, depot_a),
        (DEPOT_B, depot_b),
        (BUILD_META, details),
    ] {
        meta.insert(meta_path(id), zlib(doc.to_string().as_bytes()));
    }
}

impl FakeGog {
    /// A config whose every endpoint is this server, with the fake's client.
    pub fn config(&self, dir: &Path) -> GogConfig {
        let mut cfg = GogConfig::new(dir.join("cache"), dir.join("login/gog.json"));
        let u = |p: &str| Url::parse(&format!("{}{p}", self.base)).unwrap();
        cfg.auth_base = u("/auth");
        cfg.api_base = u("/api");
        cfg.content_base = u("/cs");
        cfg.cdn_meta_base = u("");
        cfg.client_id = CLIENT_ID.into();
        cfg.client_secret = CLIENT_SECRET.into();
        cfg.redirect_uri = REDIRECT.into();
        cfg
    }

    /// Every request whose path starts with `prefix`.
    pub fn requests(&self, prefix: &str) -> Vec<String> {
        self.state
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// Requests for the chunk whose compressed MD5 is `cmd5`.
    pub fn chunk_requests(&self, cmd5: &str) -> usize {
        self.requests("/cdn/")
            .iter()
            .filter(|p| p.ends_with(cmd5))
            .count()
    }

    pub fn clear_log(&self) {
        self.state.log.lock().unwrap().clear();
    }

    /// The compressed MD5 of `raw` as a chunk (what the CDN path names).
    pub fn chunk_id(raw: &[u8]) -> String {
        md5_hex(&zlib(raw))
    }

    /// Serve the chunk `cmd5` with one byte flipped until [`heal`](Self::heal).
    pub fn corrupt(&self, cmd5: &str) {
        self.state.corrupt.lock().unwrap().insert(cmd5.to_string());
    }

    pub fn heal(&self) {
        self.state.corrupt.lock().unwrap().clear();
    }

    /// Answer 401 to every access token issued so far, as GOG does once
    /// one expires.
    pub fn expire_access_tokens(&self) {
        self.state.access.lock().unwrap().clear();
    }

    /// Refuse refresh requests with 400 `invalid_grant`.
    pub fn fail_refresh(&self) {
        self.state.fail_refresh.store(true, Ordering::SeqCst);
    }

    pub fn refresh_calls(&self) -> u32 {
        self.state.refresh_calls.load(Ordering::SeqCst)
    }

    pub fn code_calls(&self) -> u32 {
        self.state.code_calls.load(Ordering::SeqCst)
    }

    /// Hold every CDN response for `ms` milliseconds, so concurrent
    /// requests overlap.
    pub fn delay_cdn(&self, ms: u64) {
        self.state.cdn_delay_ms.store(ms, Ordering::SeqCst);
    }

    /// The most CDN requests seen in flight at once.
    pub fn max_cdn_in_flight(&self) -> u32 {
        self.state.cdn_max_in_flight.load(Ordering::SeqCst)
    }

    /// Serve `raw` as a chunk; its manifest record.
    pub fn add_chunk(&self, raw: &[u8]) -> Chunk {
        let packed = zlib(raw);
        let c = Chunk {
            compressed_md5: Md5::digest(&packed).into(),
            md5: Md5::digest(raw).into(),
            size: raw.len() as u64,
            compressed_size: packed.len() as u64,
        };
        self.state
            .chunks
            .lock()
            .unwrap()
            .insert(md5_hex(&packed), packed);
        c
    }

    /// A depot manifest (built in memory, its chunks served by this CDN)
    /// whose files are `path` -> the file's chunks, in order.
    pub fn manifest(&self, files: &[(&str, &[&[u8]])]) -> DepotManifest {
        DepotManifest {
            items: files
                .iter()
                .map(|(path, chunks)| DepotItem {
                    path: path.to_string(),
                    chunks: chunks.iter().map(|c| self.add_chunk(c)).collect(),
                    size: chunks.iter().map(|c| c.len() as u64).sum(),
                    md5: None,
                    sfc_ref: None,
                    flags: Vec::new(),
                })
                .collect(),
            small_files_container: None,
        }
    }

    /// The query of the last token request, sorted by key.
    pub fn last_token_query(&self) -> Vec<(String, String)> {
        self.state.last_token_query.lock().unwrap().clone()
    }
}

/// An `Http` with fast retries (three attempts), so tests run quickly.
pub fn http() -> Http {
    http_with_parts(HttpConfig::default().parallel_parts)
}

/// [`http`] fetching at most `parallel_parts` parts of one file at once.
pub fn http_with_parts(parallel_parts: usize) -> Http {
    Http::new(
        HttpConfig {
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(5),
                max_retry_after: Duration::from_secs(2),
            },
            parallel_parts,
            ..HttpConfig::default()
        },
        Events::default(),
    )
    .unwrap()
}

fn respond(status: u16, body: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap())
        .body(Body::from(body))
        .unwrap()
}

fn json_resp(status: u16, v: serde_json::Value) -> Response {
    let mut r = respond(status, v.to_string().into_bytes());
    r.headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    r
}

async fn handle(State(s): State<Arc<ServerState>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    s.log.lock().unwrap().push(if query.is_empty() {
        path.clone()
    } else {
        format!("{path}?{query}")
    });
    let q: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);

    if path == "/auth/token" {
        return token(&s, &q);
    }
    if let Some(rest) = path.strip_prefix("/cs/") {
        let authed = bearer.is_some_and(|b| s.access.lock().unwrap().contains(&b));
        if !authed {
            return json_resp(401, json!({ "error": "invalid_token" }));
        }
        return content_system(&s, rest, &q);
    }
    if let Some(rest) = path.strip_prefix("/cdn/") {
        let now = s.cdn_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        s.cdn_max_in_flight.fetch_max(now, Ordering::SeqCst);
        let delay = s.cdn_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        let r = cdn(&s, rest);
        s.cdn_in_flight.fetch_sub(1, Ordering::SeqCst);
        return r;
    }
    match s.meta.lock().unwrap().get(&path) {
        Some(b) => respond(200, b.clone()),
        None => respond(404, b"not found".to_vec()),
    }
}

fn token(s: &ServerState, q: &HashMap<String, String>) -> Response {
    let mut sorted: Vec<_> = q.clone().into_iter().collect();
    sorted.sort();
    *s.last_token_query.lock().unwrap() = sorted;
    if q.get("client_id").map(String::as_str) != Some(CLIENT_ID)
        || q.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET)
    {
        return json_resp(401, json!({ "error": "invalid_client" }));
    }
    let grant = |ok: bool| {
        if ok {
            None
        } else {
            Some(json_resp(
                400,
                json!({ "error": "invalid_grant", "error_description": "bad grant" }),
            ))
        }
    };
    match q.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            s.code_calls.fetch_add(1, Ordering::SeqCst);
            let ok = q.get("code").map(String::as_str) == Some(CODE)
                && q.get("redirect_uri").map(String::as_str) == Some(REDIRECT);
            if let Some(r) = grant(ok) {
                return r;
            }
        }
        Some("refresh_token") => {
            s.refresh_calls.fetch_add(1, Ordering::SeqCst);
            let ok = !s.fail_refresh.load(Ordering::SeqCst)
                && q.get("refresh_token") == s.refresh.lock().unwrap().as_ref();
            if let Some(r) = grant(ok) {
                return r;
            }
        }
        _ => return json_resp(400, json!({ "error": "unsupported_grant_type" })),
    }
    let n = s.issued.fetch_add(1, Ordering::SeqCst) + 1;
    let (access, refresh) = (format!("access-{n}"), format!("refresh-{n}"));
    s.access.lock().unwrap().insert(access.clone());
    *s.refresh.lock().unwrap() = Some(refresh.clone());
    json_resp(
        200,
        json!({
            "expires_in": 3600, "scope": "", "token_type": "bearer",
            "access_token": access, "refresh_token": refresh,
            "user_id": USER_ID, "session_id": "session",
        }),
    )
}

fn content_system(s: &ServerState, rest: &str, q: &HashMap<String, String>) -> Response {
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        ["products", id, "os", os, "builds"]
            if q.get("generation").map(String::as_str) == Some("2") =>
        {
            if *id != GAME.to_string() {
                return json_resp(404, json!({ "error": "not_found" }));
            }
            json_resp(
                200,
                json!({
                    "total_count": 1, "count": 1,
                    "items": [{
                        "build_id": BUILD_ID, "product_id": GAME.to_string(), "os": os,
                        "branch": null, "version_name": "1.6.1170", "tags": ["csb_10_6_1_w_268"],
                        "public": true, "date_published": "2024-01-02T03:04:05+0000",
                        "generation": 2, "legacy_build_id": null,
                        "link": format!("{}{}", s.base, meta_path(BUILD_META)),
                    }],
                }),
            )
        }
        ["products", id, "secure_link"] => {
            let want = [("generation", "2"), ("_version", "2"), ("path", "/")];
            if want
                .iter()
                .any(|(k, v)| q.get(*k).map(String::as_str) != Some(*v))
            {
                return json_resp(400, json!({ "error": "bad query" }));
            }
            json_resp(
                200,
                json!({
                    "product_id": id.parse::<u64>().unwrap(), "type": "depot",
                    "urls": [{
                        "endpoint_name": "fake",
                        "url_format": "{base_url}/token=nva={expires_at}~dirs={dirs}~token={token}{path}",
                        "parameters": {
                            "base_url": format!("{}/cdn", s.base),
                            "path": format!("/content-system/v2/store/{id}"),
                            "token": "tok",
                            "expires_at": now() + 3600,
                            "dirs": 2,
                        },
                        "priority": 1, "max_fails": 1, "supports_generation": [2],
                        "fallback_only": false,
                    }],
                }),
            )
        }
        _ => json_resp(404, json!({ "error": "not_found" })),
    }
}

fn cdn(s: &ServerState, rest: &str) -> Response {
    // token=nva=…~dirs=2~token=tok/content-system/v2/store/{id}/ab/cd/{md5}
    let parts: Vec<&str> = rest.split('/').collect();
    let [auth, "content-system", "v2", "store", _id, a, b, md5] = parts.as_slice() else {
        return respond(404, b"bad cdn path".to_vec());
    };
    if !auth.ends_with("~token=tok") || md5.get(..2) != Some(*a) || md5.get(2..4) != Some(*b) {
        return respond(403, b"forbidden".to_vec());
    }
    let Some(mut body) = s.chunks.lock().unwrap().get(*md5).cloned() else {
        return respond(404, b"no such chunk".to_vec());
    };
    if s.corrupt.lock().unwrap().contains(*md5) {
        let mid = body.len() / 2;
        body[mid] ^= 0xff;
    }
    respond(200, body)
}
