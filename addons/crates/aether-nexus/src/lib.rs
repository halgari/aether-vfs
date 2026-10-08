//! Nexus Mods: the v3 API (file records, signed repacked-zip URLs) and
//! random access into repacked zips.

mod archive;
mod links;
#[cfg(feature = "provider")]
pub mod provider;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::Deserialize;
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

use aether_net::bulk::{BulkHttp, RangeBody};
use aether_net::error::{Result, SourceError, redact};
use aether_net::events::Job;
use aether_net::http::{ERROR_BODY_MAX, Http, error_body, read_body};

pub use archive::{NexusArchive, SpanLease};
use links::SignedUrl;

pub const NEXUS_API: &str = "https://api.nexusmods.com";
/// Nexus game id of Skyrim Special Edition.
pub const SKYRIM_SE_GAME_ID: u32 = 1704;
/// Nexus game id of the original Skyrim. Special Edition lists take some
/// archives from its pages (TPF: 28 of 851).
pub const SKYRIM_GAME_ID: u32 = 110;
/// A cached signed URL is refreshed this long before it expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(300);
/// While links are being added, they are written to the link file at most
/// this often ([`NexusClient::save_links`] writes them at once).
const LINK_SAVE_INTERVAL: Duration = Duration::from_secs(30);
/// A reported allowance is believed for this long; after that, or once the
/// reset time it came with has passed, it is no reading (what is left may
/// have changed in either direction since).
const QUOTA_MAX_AGE: Duration = Duration::from_secs(300);
/// Bytes of an API error body read to find its problem+json `detail`.
const PROBLEM_MAX: usize = 4096;

/// Nexus game id for a Wabbajack game name (`Archive.source.game_name`).
pub fn game_id(wabbajack_game: &str) -> Option<u32> {
    [
        ("SkyrimSpecialEdition", SKYRIM_SE_GAME_ID),
        ("Skyrim", SKYRIM_GAME_ID),
    ]
    .into_iter()
    .find(|(name, _)| wabbajack_game.eq_ignore_ascii_case(name))
    .map(|(_, id)| id)
}

/// A mod file version's uid: `(game_id << 32) + file_id`.
pub fn uid(game_id: u32, file_id: u64) -> u64 {
    ((game_id as u64) << 32) + file_id
}

/// `GET /v3/games/{domain}/mod-file-versions/{file_id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub uid: u64,
    pub name: String,
    pub version: String,
    pub category: String,
}

/// The account behind an API key (`GET /v1/users/validate.json`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NexusUser {
    pub name: String,
    pub is_premium: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The signed URLs a client holds, and the file they are kept in between
/// runs (if any).
struct Links {
    urls: Mutex<HashMap<u64, SignedUrl>>,
    /// `None`: links live only as long as the client.
    file: Option<PathBuf>,
    /// Links were added or dropped since the file was last written.
    dirty: AtomicBool,
    /// When the file was last written (or the client was built).
    last_save: Mutex<Instant>,
    /// A background save is running.
    saving: AtomicBool,
    /// Held while the file is written: one writer at a time.
    writing: Mutex<()>,
}

impl Links {
    fn new(urls: HashMap<u64, SignedUrl>, file: Option<PathBuf>) -> Arc<Links> {
        Arc::new(Links {
            urls: Mutex::new(urls),
            file,
            dirty: AtomicBool::new(false),
            last_save: Mutex::new(Instant::now()),
            saving: AtomicBool::new(false),
            writing: Mutex::new(()),
        })
    }

    /// Write the links that are still valid to the file, if any were added
    /// or dropped since it was last written. Expired ones are dropped from
    /// memory too. Blocks on the file.
    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.file else {
            return Ok(());
        };
        let _w = lock(&self.writing);
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let now = SystemTime::now();
        let snapshot = {
            let mut urls = lock(&self.urls);
            urls.retain(|_, s| s.valid(now, EXPIRY_MARGIN));
            urls.clone()
        };
        let r = links::save(path, &snapshot, now, EXPIRY_MARGIN);
        if r.is_err() {
            self.dirty.store(true, Ordering::Release);
        }
        *lock(&self.last_save) = Instant::now();
        r
    }
}

/// The longest a call waits for a used-up API allowance to reset: the
/// hourly one, not the daily one.
const MAX_ALLOWANCE_WAIT: Duration = Duration::from_secs(65 * 60);

/// One of the account's API allowances, as a response reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    /// Requests the allowance holds when it is full.
    pub limit: u64,
    /// Requests left of it.
    pub remaining: u64,
    /// When the allowance is full again (`x-rl-<name>-reset`), if the
    /// response said so in a form that is understood.
    pub reset: Option<SystemTime>,
}

impl Bucket {
    /// Requests that can still be made before less than half the
    /// allowance is left.
    pub fn spare(&self) -> u64 {
        self.remaining.saturating_sub(self.limit.div_ceil(2))
    }

    /// `x-rl-<name>-limit`, `x-rl-<name>-remaining` and, if it is there,
    /// `x-rl-<name>-reset`. `None` unless the first two are there and are
    /// numbers, with a limit above zero.
    fn from_headers(headers: &HeaderMap, name: &str) -> Option<Bucket> {
        let text = |what: &str| -> Option<&str> {
            let value = headers.get(format!("x-rl-{name}-{what}"))?;
            Some(value.to_str().ok()?.trim())
        };
        let number = |what: &str| -> Option<u64> { text(what)?.parse().ok() };
        let (limit, remaining) = (number("limit")?, number("remaining")?);
        (limit > 0).then_some(Bucket {
            limit,
            remaining,
            reset: text("reset").and_then(reset_time),
        })
    }
}

/// The time in a reset header, which the API writes as
/// `2026-10-02 18:00:00 +0000`. Only UTC is understood; anything else is
/// `None`, and the reading is then judged by its age alone.
fn reset_time(text: &str) -> Option<SystemTime> {
    let utc = ["+0000", "+00:00", "UTC", "Z"]
        .iter()
        .find_map(|zone| text.strip_suffix(zone))?;
    humantime::parse_rfc3339_weak(utc.trim_end()).ok()
}

/// A bucket and when it was reported.
#[derive(Debug, Clone, Copy)]
struct Reading {
    bucket: Bucket,
    at: SystemTime,
}

impl Reading {
    /// Whether this still says something at `now`: its reset time has not
    /// passed, and it is no older than [`QUOTA_MAX_AGE`] (a clock that went
    /// back makes it old).
    fn current(&self, now: SystemTime) -> bool {
        let reset = self.bucket.reset.is_some_and(|reset| now >= reset);
        let young = now
            .duration_since(self.at)
            .is_ok_and(|age| age <= QUOTA_MAX_AGE);
        !reset && young
    }

    /// `newer`, reported at `now`, in place of `old`. Responses can be
    /// handled in another order than the API sent them: before the same
    /// reset an allowance only shrinks, so the lower count is the later one.
    fn replace(old: Option<Reading>, newer: Bucket, now: SystemTime) -> Reading {
        let remaining = match old {
            Some(old) if newer.reset.is_some() && old.bucket.reset == newer.reset => {
                newer.remaining.min(old.bucket.remaining)
            }
            _ => newer.remaining,
        };
        Reading {
            bucket: Bucket { remaining, ..newer },
            at: now,
        }
    }
}

/// The last reading of each allowance.
#[derive(Debug, Clone, Copy, Default)]
struct Readings {
    hourly: Option<Reading>,
    daily: Option<Reading>,
}

impl Readings {
    /// Take over the buckets `newer` reports; keep the others.
    fn update(&mut self, newer: Quota, now: SystemTime) {
        if let Some(b) = newer.hourly {
            self.hourly = Some(Reading::replace(self.hourly, b, now));
        }
        if let Some(b) = newer.daily {
            self.daily = Some(Reading::replace(self.daily, b, now));
        }
    }

    /// The readings that are still current at `now`; `None` when neither
    /// is.
    fn quota(&self, now: SystemTime) -> Option<Quota> {
        let current = |r: Option<Reading>| r.filter(|r| r.current(now)).map(|r| r.bucket);
        let q = Quota {
            hourly: current(self.hourly),
            daily: current(self.daily),
        };
        q.known().then_some(q)
    }
}

/// What the API last said about the account's request allowances (2,000
/// an hour and 20,000 a day when this was written), which every call draws
/// on: a download link is one request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Quota {
    pub hourly: Option<Bucket>,
    pub daily: Option<Bucket>,
}

impl Quota {
    /// The allowances a response reports in its `x-rl-hourly-*` and
    /// `x-rl-daily-*` headers. `None` when it reports neither (no such
    /// headers, or values that are not numbers).
    pub fn from_headers(headers: &HeaderMap) -> Option<Quota> {
        let q = Quota {
            hourly: Bucket::from_headers(headers, "hourly"),
            daily: Bucket::from_headers(headers, "daily"),
        };
        q.known().then_some(q)
    }

    fn known(&self) -> bool {
        self.hourly.is_some() || self.daily.is_some()
    }

    /// Requests that can still be made before any reported allowance has
    /// less than half its limit left. Work that is not needed now (the
    /// index pass) stops at zero; reads never look at it.
    pub fn spare(&self) -> u64 {
        [self.hourly, self.daily]
            .iter()
            .flatten()
            .map(Bucket::spare)
            .min()
            .unwrap_or(0)
    }
}

type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// Client for the Nexus Mods v3 API. Holds the user's API key (never
/// printed) and caches signed download URLs until shortly before expiry,
/// across runs when given a link file ([`NexusClient::with_link_file`]).
pub struct NexusClient {
    http: Http,
    base: Url,
    api_key: String,
    links: Arc<Links>,
    link_save_interval: Duration,
    /// The allowances the API last reported (see [`NexusClient::quota`]).
    quota: Mutex<Readings>,
    /// The time, for the age of those readings.
    clock: Clock,
    /// Download links asked of the API so far.
    links_requested: AtomicU64,
    /// One lock per uid, so concurrent callers that all need a signed URL
    /// (a cold cache, or one close to expiry) make exactly one
    /// `download-repacked` request instead of a burst of them.
    fetches: Mutex<HashMap<u64, Arc<AsyncMutex<()>>>>,
}

impl fmt::Debug for NexusClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NexusClient")
            .field("base", &self.base.as_str())
            .field("api_key", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize)]
struct RecordEnvelope {
    data: RecordData,
}

#[derive(Deserialize)]
struct RecordData {
    id: String,
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    category: String,
}

#[derive(Deserialize)]
struct RepackedResponse {
    download_url: String,
    expires_at: String,
}

#[derive(Deserialize, Default)]
struct Problem {
    #[serde(default)]
    detail: String,
}

impl NexusClient {
    pub fn new(http: Http, api_key: impl Into<String>) -> NexusClient {
        NexusClient {
            http,
            base: Url::parse(NEXUS_API).expect("constant URL"),
            api_key: api_key.into(),
            links: Links::new(HashMap::new(), None),
            link_save_interval: LINK_SAVE_INTERVAL,
            quota: Mutex::new(Readings::default()),
            clock: Arc::new(SystemTime::now),
            links_requested: AtomicU64::new(0),
            fetches: Mutex::new(HashMap::new()),
        }
    }

    /// Keep signed URLs in `path` between runs: the links saved there that
    /// have more than five minutes left are used from now on (a missing or
    /// unreadable file is no links), and new ones are written back by
    /// [`save_links`](Self::save_links) and, while links are being added,
    /// at most every 30 seconds. The file is a secret like the API key:
    /// mode 0600, and its content is never logged. Blocks on the file.
    pub fn with_link_file(mut self, path: impl Into<PathBuf>) -> NexusClient {
        let path = path.into();
        let urls = links::load(&path, SystemTime::now(), EXPIRY_MARGIN);
        self.links = Links::new(urls, Some(path));
        self
    }

    /// Write new links to the link file at most this often while they are
    /// being added, instead of every 30 seconds (tests).
    pub fn with_link_save_interval(mut self, every: Duration) -> NexusClient {
        self.link_save_interval = every;
        self
    }

    /// Tell the age of the allowance readings by `clock` instead of the
    /// system's (tests).
    #[doc(hidden)]
    pub fn with_clock(mut self, clock: impl Fn() -> SystemTime + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Write the links signed (or dropped) since the last save to the link
    /// file, without the expired ones. Does nothing without a link file or
    /// when nothing changed. Blocks on the file: call it from a plain thread
    /// or `spawn_blocking`.
    pub fn save_links(&self) -> Result<()> {
        Ok(self.links.save()?)
    }

    /// [`save_links`](Self::save_links) on a blocking thread of the runtime
    /// this is called on, one save at a time; a failure is only logged.
    fn save_links_in_background(&self) {
        let l = self.links.clone();
        // Outside a runtime (a caller that is not async) the links stay
        // marked as changed and go out with the next save.
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if l.file.is_none() || l.saving.swap(true, Ordering::AcqRel) {
            return;
        }
        rt.spawn_blocking(move || {
            if let Err(e) = l.save() {
                tracing::warn!(error = %e, "failed to save the Nexus download links");
            }
            l.saving.store(false, Ordering::Release);
        });
    }

    /// A link was added or dropped: save in the background if the last
    /// save was long enough ago.
    fn links_changed(&self) {
        self.links.dirty.store(true, Ordering::Release);
        if lock(&self.links.last_save).elapsed() >= self.link_save_interval {
            self.save_links_in_background();
        }
    }

    /// Point at another API host (tests).
    pub fn with_base_url(mut self, base: &str) -> Result<NexusClient> {
        self.base = Url::parse(base).map_err(|e| SourceError::Protocol {
            url: base.to_string(),
            msg: format!("not a URL: {e}"),
        })?;
        Ok(self)
    }

    pub fn http(&self) -> &Http {
        &self.http
    }

    /// Look a file up by its public file id (confirms it exists; gives the uid).
    pub async fn file_record(&self, game_domain: &str, file_id: u64) -> Result<FileRecord> {
        let url = self.api_url(&format!(
            "v3/games/{game_domain}/mod-file-versions/{file_id}"
        ))?;
        let job = self
            .http
            .start(format!("nexus record {game_domain}/{file_id}"), None);
        let r = async {
            let body = self.api_call(reqwest::Method::GET, &url, &job).await?;
            let env: RecordEnvelope = serde_json::from_slice(&body)
                .map_err(|e| SourceError::protocol(&url, format!("bad JSON: {e}")))?;
            Ok(FileRecord {
                uid: env.data.id.parse().map_err(|_| {
                    SourceError::protocol(&url, format!("uid {:?} is not a number", env.data.id))
                })?,
                name: env.data.name,
                version: env.data.version,
                category: env.data.category,
            })
        }
        .await;
        job.complete(r)
    }

    /// Check the API key and say whose it is. The reply also echoes the
    /// key, so only `name` and `is_premium` are read, and a malformed reply
    /// is reported by position alone: serde would quote the offending value.
    pub async fn validate(&self) -> Result<NexusUser> {
        let url = self.api_url("v1/users/validate.json")?;
        let job = self.http.start("nexus validate key", None);
        let r = async {
            let body = self.api_call(reqwest::Method::GET, &url, &job).await?;
            serde_json::from_slice::<NexusUser>(&body).map_err(|e| {
                SourceError::protocol(
                    &url,
                    format!("bad JSON at line {} column {}", e.line(), e.column()),
                )
            })
        }
        .await;
        job.complete(r)
    }

    /// A signed URL for the repacked zip of `uid`, from the cache while it
    /// has more than five minutes left, else from
    /// `POST /v3/mod-file-versions/{uid}/download-repacked`. Concurrent
    /// callers for the same `uid` that all miss the cache (a cold start, or
    /// one close to expiry) share one request: the rest wait on a per-uid
    /// lock and then reuse whatever the first caller fetched.
    pub async fn download_url(&self, uid: u64) -> Result<Url> {
        if let Some(url) = self.cached_url(uid) {
            return Ok(url);
        }
        let fetch = lock(&self.fetches)
            .entry(uid)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone();
        let _guard = fetch.lock().await;
        // Another caller may have refreshed it while we waited for the lock.
        if let Some(url) = self.cached_url(uid) {
            return Ok(url);
        }
        let url = self.api_url(&format!("v3/mod-file-versions/{uid}/download-repacked"))?;
        self.links_requested.fetch_add(1, Ordering::Relaxed);
        let job = self.http.start(format!("nexus download link {uid}"), None);
        let r = async {
            let body = self.api_call(reqwest::Method::POST, &url, &job).await?;
            let resp: RepackedResponse = serde_json::from_slice(&body)
                .map_err(|e| SourceError::protocol(&url, format!("bad JSON: {e}")))?;
            let signed = Url::parse(&resp.download_url)
                .map_err(|e| SourceError::protocol(&url, format!("bad download_url: {e}")))?;
            let expires_at = humantime::parse_rfc3339(&resp.expires_at).map_err(|e| {
                SourceError::protocol(&url, format!("bad expires_at {:?}: {e}", resp.expires_at))
            })?;
            Ok(SignedUrl {
                url: signed,
                expires_at,
            })
        }
        .await;
        let signed = job.complete(r)?;
        lock(&self.links.urls).insert(uid, signed.clone());
        self.links_changed();
        Ok(signed.url)
    }

    /// The cached signed URL for `uid`, while it has more than
    /// [`EXPIRY_MARGIN`] left.
    fn cached_url(&self, uid: u64) -> Option<Url> {
        lock(&self.links.urls)
            .get(&uid)
            .filter(|s| s.valid(SystemTime::now(), EXPIRY_MARGIN))
            .map(|s| s.url.clone())
    }

    /// Drop the cached URL of `uid`, whatever it is (tests: the client
    /// itself drops only a link the file host refused, with
    /// [`forget_refused`](Self::forget_refused)).
    #[doc(hidden)]
    pub fn forget_url(&self, uid: u64) {
        if lock(&self.links.urls).remove(&uid).is_some() {
            self.links_changed();
        }
    }

    /// Drop the cached URL of `uid` if it is still `refused`, the one the
    /// file host rejected. A reader whose refusal comes after another
    /// reader's has already put a new link in its place drops nothing.
    pub fn forget_refused(&self, uid: u64, refused: &Url) {
        let dropped = {
            let mut urls = lock(&self.links.urls);
            match urls.get(&uid) {
                Some(s) if s.url == *refused => urls.remove(&uid).is_some(),
                _ => false,
            }
        };
        if dropped {
            self.links_changed();
        }
    }

    /// Bytes `range` of the repacked zip of `uid` (a `len`-byte file),
    /// requested over client `conn` of `bulk` and read as they arrive. The
    /// link is signed on demand like any read's (one API request when it
    /// is not held); a link the file host refuses is renewed once.
    pub async fn range_body(
        &self,
        uid: u64,
        bulk: &BulkHttp,
        conn: usize,
        range: std::ops::Range<u64>,
        len: u64,
    ) -> Result<RangeBody> {
        archive::with_url(self, uid, |url: Url| {
            let range = range.clone();
            async move { bulk.get_range(conn, &url, range, len).await }
        })
        .await
    }

    /// Whether a signed URL for `uid` is held that a read would use (more
    /// than five minutes left).
    pub fn has_url(&self, uid: u64) -> bool {
        self.cached_url(uid).is_some()
    }

    /// The account's API allowances as the most recent responses reported
    /// them (any call, successful or not), while those readings are
    /// current: an allowance whose reset time has passed, or that was
    /// reported more than five minutes ago, is left out, since what is
    /// left of it now is not known. `None` when no reading is current (or
    /// none was ever made). Nothing here is checked when a read needs a
    /// link.
    pub fn quota(&self) -> Option<Quota> {
        lock(&self.quota).quota((self.clock)())
    }

    /// How many download links were asked of the API so far (each is one
    /// request of the allowances, answered or not; retries of one are not
    /// counted again).
    pub fn links_requested(&self) -> u64 {
        self.links_requested.load(Ordering::Relaxed)
    }

    fn api_url(&self, path: &str) -> Result<Url> {
        self.base
            .join(path)
            .map_err(|e| SourceError::protocol(&self.base, format!("bad API path {path}: {e}")))
    }

    /// Call the API with retries; map 401/403/404/429 to actionable errors.
    ///
    /// When the API says an allowance is used up (429) and that allowance
    /// is full again within [`MAX_ALLOWANCE_WAIT`], the call waits for it
    /// and goes on: a prepare of a large list asks for more links than an
    /// hour allows, and failing its jobs would only make it start again.
    async fn api_call(&self, method: reqwest::Method, url: &Url, job: &Job) -> Result<Vec<u8>> {
        loop {
            let r = self
                .http
                .config()
                .retry
                .run(job, || self.api_attempt(method.clone(), url, job))
                .await;
            if !matches!(r, Err(SourceError::RateLimited { .. })) {
                return r;
            }
            let Some(wait) = self.allowance_wait() else {
                return r;
            };
            tracing::warn!(
                seconds = wait.as_secs(),
                "the Nexus API allowance is used up; waiting for it to reset"
            );
            // Shown while it lasts, however it ends.
            let _shown = self.http.events().allowance_wait(wait);
            tokio::time::sleep(wait).await;
        }
    }

    /// How long to wait after a 429 before the API answers again: until the
    /// allowances it reported as used up reset. `None` when it reported
    /// none (the retries already waited what the response asked for) or
    /// when that is longer than [`MAX_ALLOWANCE_WAIT`] (the daily
    /// allowance): the call fails instead.
    fn allowance_wait(&self) -> Option<Duration> {
        let now = (self.clock)();
        let resets: Vec<SystemTime> = self
            .quota()
            .into_iter()
            .flat_map(|q| [q.hourly, q.daily])
            .flatten()
            .filter(|b| b.remaining == 0)
            .filter_map(|b| b.reset)
            .collect();
        // A little past the reset: the clocks need not agree.
        let wait =
            resets.iter().max()?.duration_since(now).unwrap_or_default() + Duration::from_secs(5);
        (wait <= MAX_ALLOWANCE_WAIT).then_some(wait)
    }

    /// One attempt at an API call.
    async fn api_attempt(&self, method: reqwest::Method, url: &Url, job: &Job) -> Result<Vec<u8>> {
        let _permit = self.http.permit(url).await;
        // No redirects: the `apikey` header must never be
        // forwarded to whatever host a redirect names.
        let mut req = self
            .http
            .api_client()
            .request(method.clone(), url.clone())
            .header("apikey", &self.api_key)
            .header(reqwest::header::ACCEPT, "application/json");
        if method == reqwest::Method::POST {
            req = req.body(Vec::<u8>::new()); // Content-Length: 0
        }
        let resp = req.send().await.map_err(|e| SourceError::network(url, e))?;
        let status = resp.status().as_u16();
        if tracing::enabled!(tracing::Level::DEBUG)
            && let Some(limits) = rate_limits(resp.headers())
        {
            tracing::debug!(status, %limits, "Nexus API rate limits");
        }
        if let Some(q) = Quota::from_headers(resp.headers()) {
            lock(&self.quota).update(q, (self.clock)());
        }
        if resp.status().is_success() {
            return read_body(resp, url, job, None).await;
        }
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        // Bounded: enough for a problem+json document, never the
        // whole of an arbitrarily large error body.
        let body = error_body(resp, PROBLEM_MAX).await;
        let mut detail = serde_json::from_str::<Problem>(&body)
            .map(|p| p.detail)
            .unwrap_or_default();
        detail.truncate(detail.floor_char_boundary(ERROR_BODY_MAX));
        Err(match status {
            401 => SourceError::NexusUnauthorized,
            403 => SourceError::NexusForbidden { detail },
            404 => SourceError::NotFound {
                what: if detail.is_empty() {
                    redact(url)
                } else {
                    detail
                },
            },
            429 => SourceError::RateLimited {
                host: url.host_str().unwrap_or("").to_string(),
                retry_after,
            },
            s => SourceError::Status {
                url: redact(url),
                status: s,
                body: detail,
            },
        })
    }
}

/// The rate-limit headers of an API response (`x-rl-hourly-remaining` and
/// the like), as `name=value` pairs: they say how much of the account's
/// quota is left, and are not secret. `None` when there are none.
fn rate_limits(headers: &HeaderMap) -> Option<String> {
    let mut out = String::new();
    for (name, value) in headers {
        let name = name.as_str();
        if !(is_rate_limit(name) || name == "retry-after") {
            continue;
        }
        let Ok(value) = value.to_str() else { continue };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(name);
        out.push('=');
        out.push_str(value);
    }
    (!out.is_empty()).then_some(out)
}

fn is_rate_limit(header: &str) -> bool {
    header.starts_with("x-rl-") || header.contains("ratelimit")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn a_response_reports_the_hourly_and_daily_allowance() {
        // The headers of a real v3 response.
        let q = Quota::from_headers(&headers(&[
            ("x-rl-hourly-limit", "2000"),
            ("x-rl-hourly-remaining", "1998"),
            ("x-rl-hourly-reset", "2026-09-30 16:00:00 +0000"),
            ("x-rl-daily-limit", "20000"),
            ("x-rl-daily-remaining", " 19998 "),
            ("x-rl-daily-reset", "2026-10-01 00:00:00 +0000"),
        ]))
        .unwrap();
        let utc = |text: &str| humantime::parse_rfc3339(text).unwrap();
        assert_eq!(
            q.hourly,
            Some(Bucket {
                limit: 2000,
                remaining: 1998,
                reset: Some(utc("2026-09-30T16:00:00Z")),
            })
        );
        assert_eq!(
            q.daily,
            Some(Bucket {
                limit: 20_000,
                remaining: 19_998,
                reset: Some(utc("2026-10-01T00:00:00Z")),
            })
        );
        assert_eq!(q.spare(), 998, "the hourly allowance is the tighter one");
    }

    #[test]
    fn absent_or_garbage_allowance_headers_report_nothing() {
        assert_eq!(Quota::from_headers(&HeaderMap::new()), None);
        assert_eq!(
            Quota::from_headers(&headers(&[("content-length", "12")])),
            None
        );
        for (limit, remaining) in [
            ("2000", "soon"),
            ("many", "12"),
            ("2000", "-1"),
            ("2000", ""),
            ("2000", "1e3"),
            ("2000", "99999999999999999999999999"),
            // An allowance of nothing is not an allowance.
            ("0", "0"),
        ] {
            let h = headers(&[
                ("x-rl-hourly-limit", limit),
                ("x-rl-hourly-remaining", remaining),
            ]);
            assert_eq!(Quota::from_headers(&h), None, "{limit:?} {remaining:?}");
        }
        // Half a reading is none: what is left means nothing without the
        // limit, nor the limit without what is left.
        assert_eq!(
            Quota::from_headers(&headers(&[("x-rl-hourly-remaining", "5")])),
            None
        );
        assert_eq!(
            Quota::from_headers(&headers(&[("x-rl-daily-limit", "20000")])),
            None
        );
        let mut bytes = HeaderMap::new();
        bytes.insert("x-rl-hourly-limit", "2000".parse().unwrap());
        bytes.insert(
            "x-rl-hourly-remaining",
            reqwest::header::HeaderValue::from_bytes(b"\xff\xfe").unwrap(),
        );
        assert_eq!(Quota::from_headers(&bytes), None);

        // One good bucket beside a bad one is still a reading.
        let q = Quota::from_headers(&headers(&[
            ("x-rl-hourly-limit", "2000"),
            ("x-rl-hourly-remaining", "?"),
            ("x-rl-daily-limit", "20000"),
            ("x-rl-daily-remaining", "15000"),
        ]))
        .unwrap();
        assert_eq!(q.hourly, None);
        assert_eq!(q.spare(), 5000);
    }

    #[test]
    fn what_is_spare_is_what_is_left_above_half_of_each_allowance() {
        let quota = |hourly: (u64, u64), daily: (u64, u64)| Quota {
            hourly: Some(Bucket {
                limit: hourly.0,
                remaining: hourly.1,
                reset: None,
            }),
            daily: Some(Bucket {
                limit: daily.0,
                remaining: daily.1,
                reset: None,
            }),
        };
        assert_eq!(quota((2000, 2000), (20_000, 20_000)).spare(), 1000);
        assert_eq!(quota((2000, 1001), (20_000, 20_000)).spare(), 1);
        assert_eq!(quota((2000, 1000), (20_000, 20_000)).spare(), 0);
        assert_eq!(quota((2000, 3), (20_000, 20_000)).spare(), 0);
        // The day's allowance binds when it is the one nearer its half.
        assert_eq!(quota((2000, 2000), (20_000, 10_007)).spare(), 7);
        assert_eq!(quota((2000, 2000), (20_000, 9000)).spare(), 0);
        // An odd limit: half of 5 is kept as 3.
        assert_eq!(quota((5, 4), (20_000, 20_000)).spare(), 1);
        assert_eq!(quota((5, 3), (20_000, 20_000)).spare(), 0);
        // More left than the limit is taken as it is.
        assert_eq!(quota((100, 150), (20_000, 20_000)).spare(), 100);
    }

    /// A fixed moment, and `secs` after it: no test here reads a clock.
    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + secs)
    }

    fn bucket(limit: u64, remaining: u64, reset: Option<SystemTime>) -> Bucket {
        Bucket {
            limit,
            remaining,
            reset,
        }
    }

    fn reading(hourly: Option<Bucket>, daily: Option<Bucket>) -> Quota {
        Quota { hourly, daily }
    }

    #[test]
    fn a_reset_time_is_read_in_the_form_the_api_writes_it() {
        let want = humantime::parse_rfc3339("2026-10-02T18:00:00Z").unwrap();
        for text in [
            "2026-10-02 18:00:00 +0000",
            "2026-10-02 18:00:00+0000",
            "2026-10-02 18:00:00 +00:00",
            "2026-10-02 18:00:00 UTC",
            "2026-10-02T18:00:00Z",
        ] {
            assert_eq!(reset_time(text), Some(want), "{text:?}");
        }
        for text in [
            "",
            "soon",
            "1759428000",
            "2026-10-02 18:00:00",
            // Another zone is not worked out: such a reading goes by its age.
            "2026-10-02 20:00:00 +0200",
            "2026-13-45 99:00:00 +0000",
        ] {
            assert_eq!(reset_time(text), None, "{text:?}");
        }
        // A reset that cannot be read does not spoil the reading.
        let q = Quota::from_headers(&headers(&[
            ("x-rl-hourly-limit", "2000"),
            ("x-rl-hourly-remaining", "1500"),
            ("x-rl-hourly-reset", "in a while"),
        ]))
        .unwrap();
        assert_eq!(q.hourly, Some(bucket(2000, 1500, None)));
    }

    #[test]
    fn a_newer_reading_replaces_only_the_buckets_it_reports() {
        let mut r = Readings::default();
        assert_eq!(r.quota(at(0)), None);
        let (hour, day) = (Some(at(3600)), Some(at(86_400)));
        r.update(
            reading(
                Some(bucket(2000, 1500, hour)),
                Some(bucket(20_000, 12_000, day)),
            ),
            at(0),
        );
        r.update(reading(Some(bucket(2000, 1499, hour)), None), at(10));
        let q = r.quota(at(10)).unwrap();
        assert_eq!(q.hourly, Some(bucket(2000, 1499, hour)));
        assert_eq!(q.daily, Some(bucket(20_000, 12_000, day)));
    }

    #[test]
    fn a_reading_is_none_once_its_reset_time_has_passed() {
        let mut r = Readings::default();
        // Nothing spare in the hour that ends in ten minutes.
        r.update(
            reading(
                Some(bucket(2000, 900, Some(at(600)))),
                Some(bucket(20_000, 15_000, Some(at(40_000)))),
            ),
            at(0),
        );
        assert_eq!(r.quota(at(60)).unwrap().spare(), 0);
        // Asked again just before the reset, and just after it.
        r.update(
            reading(Some(bucket(2000, 899, Some(at(600)))), None),
            at(590),
        );
        r.update(
            reading(None, Some(bucket(20_000, 14_999, Some(at(40_000))))),
            at(590),
        );
        assert_eq!(r.quota(at(599)).unwrap().spare(), 0);
        let q = r.quota(at(600)).unwrap();
        assert_eq!(q.hourly, None, "the hour's allowance is full again");
        assert_eq!(q.spare(), 4999, "the day's reading stands");
        // Later still nothing is known: the day's reading is old.
        assert_eq!(r.quota(at(600 + 300)), None);
    }

    #[test]
    fn a_reading_is_none_after_five_minutes_whatever_its_reset_time() {
        for reset in [None, Some(at(3600))] {
            let mut r = Readings::default();
            r.update(reading(Some(bucket(2000, 1800, reset)), None), at(0));
            assert_eq!(r.quota(at(0)).unwrap().spare(), 800);
            assert_eq!(r.quota(at(300)).unwrap().spare(), 800);
            assert_eq!(r.quota(at(301)), None, "{reset:?}");
            // A clock that went back: its age is unknown.
            assert_eq!(
                r.quota(SystemTime::UNIX_EPOCH + Duration::from_secs(5)),
                None
            );
            // A new reading is believed again.
            r.update(reading(Some(bucket(2000, 1700, reset)), None), at(400));
            assert_eq!(r.quota(at(401)).unwrap().spare(), 700);
        }
    }

    #[test]
    fn responses_handled_out_of_order_do_not_raise_the_reading() {
        let hour = Some(at(3600));
        let mut r = Readings::default();
        // The API answered 1001 and then 1000; the second is handled first.
        r.update(reading(Some(bucket(2000, 1000, hour)), None), at(1));
        r.update(reading(Some(bucket(2000, 1001, hour)), None), at(2));
        assert_eq!(r.quota(at(2)).unwrap().spare(), 0);
        // After the reset the new hour's count is taken as it is.
        let next = Some(at(7200));
        r.update(reading(Some(bucket(2000, 1999, next)), None), at(3601));
        assert_eq!(r.quota(at(3601)).unwrap().spare(), 999);
        // Without reset times nothing says which is later: the newest stands.
        let mut r = Readings::default();
        r.update(reading(Some(bucket(2000, 1000, None)), None), at(1));
        r.update(reading(Some(bucket(2000, 1001, None)), None), at(2));
        assert_eq!(r.quota(at(2)).unwrap().spare(), 1);
    }

    #[test]
    fn uid_matches_the_research_example() {
        assert_eq!(uid(SKYRIM_SE_GAME_ID, 75329), 7_318_624_347_713);
        assert_eq!(game_id("SkyrimSpecialEdition"), Some(1704));
        assert_eq!(game_id("Fallout4"), None);
    }

    #[test]
    fn original_skyrim_mods_are_nexus_game_110() {
        // TPF takes 28 archives from the original Skyrim's Nexus pages.
        assert_eq!(game_id("Skyrim"), Some(SKYRIM_GAME_ID));
        assert_eq!(SKYRIM_GAME_ID, 110);
        assert_eq!(game_id("skyrim"), Some(110));
    }

    #[test]
    fn only_rate_limit_headers_are_logged() {
        let mut h = HeaderMap::new();
        assert_eq!(rate_limits(&h), None);
        h.insert("content-type", "application/json".parse().unwrap());
        h.insert("set-cookie", "session=SECRET".parse().unwrap());
        assert_eq!(rate_limits(&h), None);
        h.insert("x-rl-hourly-remaining", "97".parse().unwrap());
        h.insert("x-rl-daily-limit", "2500".parse().unwrap());
        let text = rate_limits(&h).unwrap();
        assert!(text.contains("x-rl-hourly-remaining=97"), "{text}");
        assert!(text.contains("x-rl-daily-limit=2500"), "{text}");
        assert!(!text.contains("SECRET") && !text.contains("json"), "{text}");
    }

    #[test]
    fn debug_never_shows_the_key() {
        let http = Http::new(Default::default(), Default::default()).unwrap();
        let c = NexusClient::new(http, "SECRET-KEY-123");
        assert!(!format!("{c:?}").contains("SECRET"));
    }

    #[test]
    fn a_used_up_hourly_allowance_is_waited_for_and_a_daily_one_is_not() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let http = Http::new(Default::default(), Default::default()).unwrap();
        let c = NexusClient::new(http, "k").with_clock(move || now);
        let bucket = |limit, remaining, secs| Bucket {
            limit,
            remaining,
            reset: Some(now + Duration::from_secs(secs)),
        };
        let set = |hourly, daily| {
            *lock(&c.quota) = Readings::default();
            lock(&c.quota).update(Quota { hourly, daily }, now);
        };
        // Nothing known: the retries are all there is.
        assert_eq!(c.allowance_wait(), None);
        // The hour is used up: until it resets, and a little longer.
        set(
            Some(bucket(2000, 0, 600)),
            Some(bucket(20_000, 9000, 40_000)),
        );
        assert_eq!(c.allowance_wait(), Some(Duration::from_secs(605)));
        // The day is used up: not waited for.
        set(Some(bucket(2000, 0, 600)), Some(bucket(20_000, 0, 40_000)));
        assert_eq!(c.allowance_wait(), None);
        // Allowances with room say nothing about a 429.
        set(Some(bucket(2000, 5, 600)), None);
        assert_eq!(c.allowance_wait(), None);
    }
}
