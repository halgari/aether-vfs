//! Steam CDN: a pool over a given server list that keeps to the fastest
//! healthy host, and a fetcher that retries across those servers with
//! timeouts and requests a CDN auth token on HTTP 403. Picking *which*
//! servers go into that list —
//! DepotDownloader-style filtering by type, region and `allowed_app_ids` —
//! is not this module's job; it happens wherever the `Vec<CdnServer>` this
//! module is given gets built (the CM/content-server-directory layer).
use crate::error::SteamError;
use crate::ids::{AppId, DepotId};
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A CDN host that serves depot chunks and manifests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdnServer {
    pub host: String,
    pub port: u16,
    pub https: bool,
}

impl CdnServer {
    /// Absolute URL for `path` (which starts with `/`), with the CDN auth
    /// token, if any, as the query string.
    pub(crate) fn url(&self, path: &str, token: Option<&str>) -> String {
        let scheme = if self.https { "https" } else { "http" };
        let mut url = format!("{scheme}://{}:{}{path}", self.host, self.port);
        if let Some(t) = token
            .map(|t| t.trim_start_matches('?'))
            .filter(|t| !t.is_empty())
        {
            url.push('?');
            url.push_str(t);
        }
        url
    }
}

/// Timeouts, retry and concurrency limits for CDN traffic.
#[derive(Clone, Debug)]
pub struct CdnConfig {
    pub connect_timeout: Duration,
    /// Deadline for one whole HTTP request (headers and body).
    pub request_timeout: Duration,
    /// Attempts per chunk or manifest, across all servers.
    pub max_attempts: u32,
    /// First cooldown for a failing server; doubles per failure.
    pub cooldown_base: Duration,
    pub cooldown_max: Duration,
    /// Chunk downloads in flight per depot reader.
    pub max_concurrent_chunks: usize,
    /// Decoded chunks kept in memory per depot reader.
    pub chunk_cache_bytes: usize,
    /// How long the connect probe of one host may take before that host
    /// ranks last.
    pub probe_timeout: Duration,
}

impl Default for CdnConfig {
    fn default() -> Self {
        CdnConfig {
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
            max_attempts: 8,
            cooldown_base: Duration::from_secs(1),
            cooldown_max: Duration::from_secs(30),
            max_concurrent_chunks: 16,
            chunk_cache_bytes: 64 << 20,
            probe_timeout: Duration::from_secs(2),
        }
    }
}

/// Where CDN auth tokens come from when a server answers 403.
/// [`SteamSession`](crate::SteamSession) implements it.
pub trait CdnTokenSource: Send + Sync {
    /// The token for `host`, or `None` when Steam says none is needed.
    fn cdn_auth_token<'a>(
        &'a self,
        app: AppId,
        depot: DepotId,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, SteamError>> + Send + 'a>>;
}

/// How a CDN download ended, as its [`CdnObserver`] hears it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CdnEnd {
    Ok,
    /// Every attempt failed.
    Failed,
    /// The download was dropped before it ended.
    Cancelled,
}

/// One finished CDN download (a chunk or a manifest), all attempts together.
#[derive(Clone, Debug)]
pub struct CdnRequest<'a> {
    /// `steam <depot> <chunk id>`, or `steam <depot> manifest <manifest id>`.
    /// Never a URL: no host, no CDN auth token, no manifest request code.
    pub label: &'a str,
    /// Body bytes received, failed attempts included.
    pub bytes: u64,
    /// From the first attempt's start to the end: every attempt, the waits
    /// between them and the decode.
    pub elapsed: Duration,
    /// Attempts after the first.
    pub retries: u32,
    pub end: CdnEnd,
}

/// Hears about every CDN download as it ends: how a caller (the streaming
/// trace) sees this crate's HTTP traffic. Called on the task that made the
/// request, so it must not block.
pub trait CdnObserver: Send + Sync {
    fn request(&self, r: &CdnRequest<'_>);
}

/// A boxed future, as the object-safe traits here return them.
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Measures how long connecting to a CDN host takes, to seed a pool's
/// ranking. A trait so tests rank hosts without a network.
pub(crate) trait Probe: Send + Sync {
    fn connect<'a>(&'a self, server: &'a CdnServer) -> BoxFuture<'a, std::io::Result<Duration>>;
}

/// The real probe: the time of one TCP connect, which is about one round
/// trip. The name is resolved first, so a slow DNS answer does not count
/// against the host. Every address of the name is tried at once and the
/// first to connect counts, so an address family this machine cannot route
/// does not make the host look dead. The connection is dropped at once;
/// nothing is sent.
pub(crate) struct TcpProbe;

impl Probe for TcpProbe {
    fn connect<'a>(&'a self, server: &'a CdnServer) -> BoxFuture<'a, std::io::Result<Duration>> {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((server.host.as_str(), server.port)).await?;
            connect_any(addrs.collect()).await
        })
    }
}

/// The time of the first TCP connect to succeed among `addrs`, all tried
/// at once; the last error when none does.
async fn connect_any(addrs: Vec<std::net::SocketAddr>) -> std::io::Result<Duration> {
    if addrs.is_empty() {
        return Err(std::io::Error::other("the host name has no address"));
    }
    let started = Instant::now();
    let connects = addrs.into_iter().map(|a| {
        Box::pin(async move {
            tokio::net::TcpStream::connect(a).await?;
            Ok::<_, std::io::Error>(started.elapsed())
        })
    });
    let (took, _rest) = futures_util::future::select_ok(connects).await?;
    Ok(took)
}

/// Opens a connection to a CDN host ahead of the first request to it, so
/// that request does not pay for TCP and TLS. A trait so tests see which
/// host was chosen without a network.
pub(crate) trait Preconnect: Send + Sync {
    /// Whether a connection was made. Failing is not an error to anyone:
    /// the first request connects by itself.
    fn connect<'a>(&'a self, server: &'a CdnServer) -> BoxFuture<'a, bool>;
}

/// The real pre-connect: one `HEAD /` on the client the readers share,
/// which leaves the connection in that client's pool. The answer's status
/// does not matter, and the request carries no path, token or key.
pub(crate) struct HttpPreconnect {
    pub(crate) http: reqwest::Client,
    pub(crate) timeout: Duration,
}

impl Preconnect for HttpPreconnect {
    fn connect<'a>(&'a self, server: &'a CdnServer) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let request = self.http.head(server.url("/", None)).send();
            match tokio::time::timeout(self.timeout, request).await {
                Ok(Ok(_)) => true,
                Ok(Err(e)) => {
                    let error = e.without_url();
                    tracing::debug!(host = %server.host, %error, "CDN pre-connect failed");
                    false
                }
                Err(_) => {
                    tracing::debug!(host = %server.host, "CDN pre-connect timed out");
                    false
                }
            }
        })
    }
}

/// What is known of how fast a host answers. Ordered best first: any
/// measured host, then one never measured, then one whose probe failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Latency {
    /// The probe's connect time, then a moving average of the time to first
    /// byte of the requests sent to the host.
    Measured(Duration),
    Unknown,
    Unreachable,
}

/// The weight of a new time-to-first-byte sample in a host's average is
/// one in this many.
const LATENCY_SMOOTHING: u32 = 4;

/// The host in use is kept until another is faster by more than this
/// factor. A busy host answers a little slower, and a reconnect costs
/// several round trips; neither should send the pool to a neighbour with
/// a cold connection.
const SWITCH_FACTOR: u32 = 2;

/// Whether `challenger` is enough faster than `incumbent` to replace it.
fn clearly_faster(challenger: Latency, incumbent: Latency) -> bool {
    match (challenger, incumbent) {
        (Latency::Measured(c), Latency::Measured(i)) => c.saturating_mul(SWITCH_FACTOR) < i,
        (c, i) => c < i,
    }
}

struct Host {
    /// Failures since the last success. Above zero the host is on trial:
    /// see [`CdnPool::pick`].
    failures: u32,
    until: Option<Instant>,
    latency: Latency,
    /// The probe's connect time: what `latency` goes back to when its
    /// request samples are old.
    baseline: Option<Duration>,
    /// When `latency` last took a request's sample.
    sampled: Option<Instant>,
    /// When the host first answered since it last failed. A request sent
    /// before then paid for the connection, so its time to first byte says
    /// nothing of the host's latency.
    warm: Option<Instant>,
    /// When the trial request of a host with `failures` was sent.
    trial: Option<Instant>,
}

struct PoolState {
    /// The host in use: the last one `pick` returned other than for a trial.
    current: Option<usize>,
    hosts: Vec<Host>,
}

/// The servers of one app, ranked by latency. Every request goes to the
/// fastest healthy host; a failing host cools down and the next fastest
/// takes over until it recovers. Shared by the depot readers of an app, so
/// they agree on the host and reuse its connection.
pub(crate) struct CdnPool {
    servers: Vec<CdnServer>,
    state: Mutex<PoolState>,
    base: Duration,
    max: Duration,
    /// How long a trial request keeps other requests off its host: a
    /// request cannot take longer, and one dropped midway never reports.
    trial_timeout: Duration,
}

impl CdnPool {
    /// A pool that knows nothing of its hosts' latency yet: it keeps to the
    /// first server of the list until [`probe`](Self::probe) or real
    /// requests say otherwise.
    pub(crate) fn new(servers: Vec<CdnServer>, cfg: &CdnConfig) -> Result<Self, SteamError> {
        if servers.is_empty() {
            return Err(SteamError::Cdn("no usable CDN servers".into()));
        }
        let hosts = servers
            .iter()
            .map(|_| Host {
                failures: 0,
                until: None,
                latency: Latency::Unknown,
                baseline: None,
                sampled: None,
                warm: None,
                trial: None,
            })
            .collect();
        Ok(CdnPool {
            servers,
            state: Mutex::new(PoolState {
                current: None,
                hosts,
            }),
            base: cfg.cooldown_base,
            max: cfg.cooldown_max,
            trial_timeout: cfg.request_timeout,
        })
    }

    pub(crate) fn server(&self, i: usize) -> &CdnServer {
        &self.servers[i]
    }

    /// Probe every host at once and rank each as its answer lands, so one
    /// silent host does not hold up the others' ranking. A host whose probe
    /// fails, or takes longer than `timeout`, ranks last; it stays in the
    /// pool. Returns whether any host answered. Run it off the read path:
    /// it takes as long as the slowest host, up to `timeout`.
    pub(crate) async fn probe(&self, probe: &dyn Probe, timeout: Duration) -> bool {
        let mut pending: FuturesUnordered<_> = self
            .servers
            .iter()
            .enumerate()
            .map(|(i, s)| async move {
                match tokio::time::timeout(timeout, probe.connect(s)).await {
                    Ok(Ok(d)) => (i, Some(d)),
                    Ok(Err(e)) => {
                        tracing::debug!(host = %s.host, error = %e, "CDN probe failed");
                        (i, None)
                    }
                    Err(_) => {
                        tracing::debug!(host = %s.host, "CDN probe timed out");
                        (i, None)
                    }
                }
            })
            .collect();
        let mut fastest: Option<(usize, Duration)> = None;
        while let Some((i, r)) = pending.next().await {
            let mut st = self.state.lock().unwrap();
            let h = &mut st.hosts[i];
            match r {
                // Over whatever a request measured meanwhile: connect times
                // compare like with like across the hosts.
                Some(d) => {
                    h.baseline = Some(d);
                    h.latency = Latency::Measured(d);
                    h.sampled = None;
                    if fastest.is_none_or(|(_, f)| d < f) {
                        fastest = Some((i, d));
                    }
                }
                // A host that has answered a request is not unreachable.
                None if h.latency == Latency::Unknown => h.latency = Latency::Unreachable,
                None => {}
            }
        }
        if let Some((i, d)) = fastest {
            let host = &self.servers[i].host;
            tracing::debug!(%host, ms = d.as_millis(), "fastest CDN host");
        }
        fastest.is_some()
    }

    /// Open a connection to the host the next request will go to, and count
    /// that host as connected (see [`Host::warm`]). For after the probe,
    /// off the read path; nothing fails if it does.
    pub(crate) async fn preconnect(&self, pre: &dyn Preconnect) {
        let chosen = {
            let now = Instant::now();
            let mut st = self.state.lock().unwrap();
            self.refresh(&mut st, now);
            self.choose(&st, now)
        };
        let Some(i) = chosen else { return };
        if pre.connect(&self.servers[i]).await {
            let mut st = self.state.lock().unwrap();
            st.hosts[i].warm.get_or_insert_with(Instant::now);
        }
    }

    /// Forget request samples older than the longest cooldown: the host
    /// ranks by its probe time again. Without this a host the pool left
    /// because it was slow for a moment would keep that figure, and never
    /// be returned to, for as long as it is not used.
    fn refresh(&self, st: &mut PoolState, now: Instant) {
        for h in &mut st.hosts {
            if h.sampled
                .is_some_and(|t| now.saturating_duration_since(t) >= self.max)
            {
                h.latency = h.baseline.map_or(Latency::Unknown, Latency::Measured);
                h.sampled = None;
            }
        }
    }

    /// The host a request sent at `now` should go to, if any may take one:
    /// the fastest that is not cooling down, except that the host in use is
    /// kept while it is healthy and no other is clearly faster (see
    /// [`SWITCH_FACTOR`]). A host that has failed and whose trial request is
    /// still out is passed over.
    fn choose(&self, st: &PoolState, now: Instant) -> Option<usize> {
        let open = |h: &Host| {
            let trying = h
                .trial
                .is_some_and(|t| now.saturating_duration_since(t) < self.trial_timeout);
            h.until.is_none_or(|t| t <= now) && (h.failures == 0 || !trying)
        };
        let best = st
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, h)| open(h))
            .min_by_key(|&(i, h)| (h.latency, i))
            .map(|(i, _)| i)?;
        let keep = st.current.filter(|&c| {
            st.hosts[c].failures == 0
                && !clearly_faster(st.hosts[best].latency, st.hosts[c].latency)
        });
        Some(keep.unwrap_or(best))
    }

    /// The host to send the next request to (see [`choose`](Self::choose)).
    ///
    /// A host that failed does not get the traffic back just because its
    /// cooldown ended: it gets one trial request, and the others stay on
    /// the next fastest host until that trial succeeds. So a host that
    /// connects fast and then stalls costs one request per cooldown, not
    /// every request.
    ///
    /// When no server may take a request (all cooling down), the one that
    /// recovers first and how long until it does.
    pub(crate) fn pick(&self) -> (usize, Duration) {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        self.refresh(&mut st, now);
        if let Some(i) = self.choose(&st, now) {
            if st.hosts[i].failures > 0 {
                st.hosts[i].trial = Some(now);
            } else {
                st.current = Some(i);
            }
            return (i, Duration::ZERO);
        }
        let (i, t) = st
            .hosts
            .iter()
            .enumerate()
            .filter_map(|(i, h)| h.until.map(|t| (i, t)))
            .min_by_key(|&(_, t)| t)
            .expect("a server that may not take a request has failed");
        (i, t.saturating_duration_since(now))
    }

    pub(crate) fn report_success(&self, i: usize) {
        let mut st = self.state.lock().unwrap();
        let h = &mut st.hosts[i];
        h.failures = 0;
        h.until = None;
        h.trial = None;
    }

    pub(crate) fn report_failure(&self, i: usize, retry_after: Option<Duration>) {
        let mut st = self.state.lock().unwrap();
        let h = &mut st.hosts[i];
        h.failures = h.failures.saturating_add(1);
        let backoff = self.base.saturating_mul(1 << (h.failures - 1).min(16));
        let cool = retry_after.unwrap_or(backoff).min(self.max);
        h.until = Some(Instant::now() + cool);
        h.trial = None;
        // Its connection may be gone with it.
        h.warm = None;
    }

    /// The request to server `i` neither succeeded nor failed and will be
    /// sent again (a 403 answered with a fresh token): if it was the host's
    /// trial, the next request may be.
    pub(crate) fn report_retry(&self, i: usize) {
        self.state.lock().unwrap().hosts[i].trial = None;
    }

    /// Server `i` took `ttfb` to start answering a request sent at `sent`.
    /// The first answer, and any other to a request sent before it, only
    /// marks the host connected: those requests waited for the connection.
    pub(crate) fn report_latency(&self, i: usize, sent: Instant, ttfb: Duration) {
        let now = Instant::now();
        let mut st = self.state.lock().unwrap();
        let h = &mut st.hosts[i];
        let warm = *h.warm.get_or_insert(now);
        if sent < warm {
            return;
        }
        h.latency = Latency::Measured(match h.latency {
            Latency::Measured(old) => (old * (LATENCY_SMOOTHING - 1) + ttfb) / LATENCY_SMOOTHING,
            Latency::Unknown | Latency::Unreachable => ttfb,
        });
        h.sampled = Some(now);
    }

    /// Server `i` did not start answering a request within `waited` (it
    /// timed out, or the connection broke). A measured host ranks no better
    /// than that wait: a host that connects fast and then says nothing must
    /// not keep ranking by its connect time.
    pub(crate) fn report_stall(&self, i: usize, waited: Duration) {
        let mut st = self.state.lock().unwrap();
        let h = &mut st.hosts[i];
        if let Latency::Measured(old) = h.latency
            && waited > old
        {
            h.latency = Latency::Measured(waited);
            h.sampled = Some(Instant::now());
        }
    }

    #[cfg(test)]
    fn latency(&self, i: usize) -> Latency {
        self.state.lock().unwrap().hosts[i].latency
    }
}

enum HttpFail {
    Status(u16, Option<Duration>),
    Transport(String),
}

/// How often an idle HTTP/2 connection is pinged, so that it is still open
/// (at the CDN and at every NAT on the way) for the next sparse read.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(20);
/// A ping unanswered for this long closes the connection.
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// The HTTP client for CDN traffic. One is shared by every depot reader of
/// a [`SteamContent`](crate::SteamContent), and its connections are never
/// closed for being idle: a read after a quiet minute finds the connection
/// to its host still open instead of paying for a new one.
pub(crate) fn http_client(cfg: &CdnConfig) -> Result<reqwest::Client, SteamError> {
    steamroom::tls::ensure_crypto_provider();
    reqwest::Client::builder()
        .connect_timeout(cfg.connect_timeout)
        .timeout(cfg.request_timeout)
        .pool_idle_timeout(None)
        .http2_keep_alive_interval(KEEP_ALIVE_INTERVAL)
        .http2_keep_alive_timeout(KEEP_ALIVE_TIMEOUT)
        .http2_keep_alive_while_idle(true)
        // Chunk and manifest URLs are plain GETs to a fixed path; a CDN
        // redirecting one is not a case this crate needs to follow, and not
        // following keeps the max_body accounting below honest.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| SteamError::Cdn(format!("HTTP client: {e}")))
}

/// What one GET measured, whether or not it succeeded.
#[derive(Default)]
struct GetStats {
    /// Time from sending the request to the response's status line.
    ttfb: Option<Duration>,
    /// Body bytes received.
    bytes: u64,
}

/// One download on its way to the observer. Dropped before
/// [`end`](Self::end) (a cancelled future), it reports
/// [`CdnEnd::Cancelled`].
struct Report<'a> {
    observer: Option<&'a dyn CdnObserver>,
    label: &'a str,
    started: Instant,
    bytes: u64,
    attempts: u32,
    done: bool,
}

impl Report<'_> {
    fn end(&mut self, end: CdnEnd) {
        self.done = true;
        if let Some(o) = self.observer {
            o.request(&CdnRequest {
                label: self.label,
                bytes: self.bytes,
                elapsed: self.started.elapsed(),
                retries: self.attempts.saturating_sub(1),
                end,
            });
        }
    }
}

impl Drop for Report<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.end(CdnEnd::Cancelled);
        }
    }
}

/// Downloads one depot's content from a CDN pool with retries across
/// servers.
pub(crate) struct Fetcher {
    pub(crate) app: AppId,
    pub(crate) depot: DepotId,
    http: reqwest::Client,
    pool: Arc<CdnPool>,
    cfg: CdnConfig,
    tokens: Option<Arc<dyn CdnTokenSource>>,
    observer: Option<Arc<dyn CdnObserver>>,
    token_cache: Mutex<HashMap<String, String>>,
    token_lock: tokio::sync::Mutex<()>,
}

impl Fetcher {
    /// A fetcher with a client and an unprobed pool of its own.
    pub(crate) fn new(
        app: AppId,
        depot: DepotId,
        servers: Vec<CdnServer>,
        cfg: CdnConfig,
        tokens: Option<Arc<dyn CdnTokenSource>>,
    ) -> Result<Self, SteamError> {
        let http = http_client(&cfg)?;
        let pool = Arc::new(CdnPool::new(servers, &cfg)?);
        Ok(Fetcher::shared(app, depot, http, pool, cfg, tokens, None))
    }

    /// A fetcher on a client and a pool shared with other depots.
    pub(crate) fn shared(
        app: AppId,
        depot: DepotId,
        http: reqwest::Client,
        pool: Arc<CdnPool>,
        cfg: CdnConfig,
        tokens: Option<Arc<dyn CdnTokenSource>>,
        observer: Option<Arc<dyn CdnObserver>>,
    ) -> Self {
        Fetcher {
            app,
            depot,
            http,
            pool,
            cfg,
            tokens,
            observer,
            token_cache: Mutex::new(HashMap::new()),
            token_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub(crate) fn config(&self) -> &CdnConfig {
        &self.cfg
    }

    /// GET `path` and run `decode` on the body on the blocking pool. A body
    /// over `max_body` bytes fails the attempt like any other transport
    /// error — checked against `Content-Length` up front where the server
    /// sends one, and against the bytes actually received either way, so a
    /// hostile or lying CDN can never make this allocate past that cap. A
    /// failed request, or a body `decode` rejects, moves on to the next
    /// server, up to `max_attempts` tries in total. On 403 a CDN auth token
    /// is requested once per host and cached for that host's later attempts.
    ///
    /// `label` names the download to the observer (see
    /// [`CdnRequest::label`]); `path` never reaches it.
    pub(crate) async fn fetch<T, F>(
        &self,
        path: &str,
        what: &str,
        label: &str,
        max_body: u64,
        decode: F,
    ) -> Result<T, SteamError>
    where
        T: Send + 'static,
        F: Fn(Bytes) -> Result<T, SteamError> + Clone + Send + 'static,
    {
        let attempts = self.cfg.max_attempts.max(1);
        let mut last = String::from("no attempt made");
        // Servers that answered 403 during this call.
        let mut refused: Vec<usize> = Vec::new();
        let mut report = Report {
            observer: self.observer.as_deref(),
            label,
            started: Instant::now(),
            bytes: 0,
            attempts: 0,
            done: false,
        };
        for _ in 0..attempts {
            let (i, wait) = self.pool.pick();
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            let host = self.pool.server(i).host.clone();
            let token = self.token_cache.lock().unwrap().get(&host).cloned();
            let url = self.pool.server(i).url(path, token.as_deref());
            report.attempts += 1;
            let mut stats = GetStats::default();
            let sent = Instant::now();
            let got = tokio::time::timeout(
                self.cfg.request_timeout,
                self.get(&url, max_body, &mut stats),
            )
            .await;
            report.bytes += stats.bytes;
            match stats.ttfb {
                Some(ttfb) => self.pool.report_latency(i, sent, ttfb),
                None => self.pool.report_stall(i, sent.elapsed()),
            }
            match got {
                Ok(Ok(body)) => {
                    let d = decode.clone();
                    match tokio::task::spawn_blocking(move || d(body)).await {
                        Ok(Ok(v)) => {
                            self.pool.report_success(i);
                            report.end(CdnEnd::Ok);
                            return Ok(v);
                        }
                        Ok(Err(e)) => last = format!("{host}: {e}"),
                        Err(e) => last = format!("{host}: decoder panicked: {e}"),
                    }
                    self.pool.report_failure(i, None);
                }
                Ok(Err(HttpFail::Status(403, _))) => {
                    last = format!("{host}: HTTP 403");
                    // The pool would send the next attempt to this server
                    // again. That is right once, with a fresh token; a
                    // server that still refuses is failing for this depot,
                    // whatever token it is sent, and the others get a turn.
                    let again = refused.contains(&i);
                    refused.push(i);
                    if !again && self.refresh_token(&host, token.as_deref(), &mut last).await {
                        self.pool.report_retry(i);
                    } else {
                        self.pool.report_failure(i, None);
                    }
                }
                Ok(Err(HttpFail::Status(code, retry_after))) => {
                    last = format!("{host}: HTTP {code}");
                    self.pool.report_failure(i, retry_after);
                }
                Ok(Err(HttpFail::Transport(e))) => {
                    last = format!("{host}: {e}");
                    self.pool.report_failure(i, None);
                }
                Err(_) => {
                    last = format!(
                        "{host}: no response within {:.1}s",
                        self.cfg.request_timeout.as_secs_f32()
                    );
                    self.pool.report_failure(i, None);
                }
            }
            tracing::debug!(what, error = %last, "CDN attempt failed");
        }
        report.end(CdnEnd::Failed);
        Err(SteamError::Cdn(format!(
            "{what}: gave up after {attempts} attempts; last error: {last}"
        )))
    }

    /// After a 403 on `host` with `used` (the token sent, if any), make sure a
    /// fresh token is cached. One task at a time: concurrent 403s on the same
    /// host wait here and reuse the token the first one fetched. Returns
    /// whether a token is now cached for `host`. When one is, the host is not
    /// reported as failing, so the pool's next `pick()` stays on it and the
    /// next attempt carries the fresh token.
    async fn refresh_token(&self, host: &str, used: Option<&str>, last: &mut String) -> bool {
        let Some(src) = &self.tokens else {
            return false;
        };
        let _one_at_a_time = self.token_lock.lock().await;
        let current = self.token_cache.lock().unwrap().get(host).cloned();
        if current.is_some() && current.as_deref() != used {
            return true; // another request already refreshed it
        }
        self.token_cache.lock().unwrap().remove(host);
        match src.cdn_auth_token(self.app, self.depot, host).await {
            Ok(Some(t)) => {
                self.token_cache.lock().unwrap().insert(host.to_string(), t);
                true
            }
            Ok(None) => false,
            Err(e) => {
                *last = format!("{host}: HTTP 403, and no CDN auth token: {e}");
                false
            }
        }
    }

    /// GET `url`, refusing a body over `max_body` bytes. Checked against
    /// `Content-Length` before reading anything, and again against the
    /// running total while streaming, so a CDN that lies about
    /// `Content-Length` (or sends none) cannot make this buffer more than
    /// `max_body` bytes before giving up. `stats` is filled in as the
    /// response arrives, so it holds what was measured even when the caller
    /// gives up on the request.
    async fn get(&self, url: &str, max_body: u64, stats: &mut GetStats) -> Result<Bytes, HttpFail> {
        let sent = Instant::now();
        let mut resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| HttpFail::Transport(e.without_url().to_string()))?;
        stats.ttfb = Some(sent.elapsed());
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            return Err(HttpFail::Status(status.as_u16(), retry_after));
        }
        if let Some(len) = resp.content_length()
            && len > max_body
        {
            return Err(HttpFail::Transport(format!(
                "body of {len} bytes exceeds the {max_body}-byte cap"
            )));
        }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(part) = resp
            .chunk()
            .await
            .map_err(|e| HttpFail::Transport(e.without_url().to_string()))?
        {
            stats.bytes += part.len() as u64;
            if buf.len() as u64 + part.len() as u64 > max_body {
                return Err(HttpFail::Transport(format!(
                    "body exceeds the {max_body}-byte cap"
                )));
            }
            buf.extend_from_slice(&part);
        }
        Ok(Bytes::from(buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeProbe, Probed};

    #[test]
    fn url_appends_token_once() {
        let s = CdnServer {
            host: "h".into(),
            port: 443,
            https: true,
        };
        assert_eq!(
            s.url("/depot/1/chunk/ab", None),
            "https://h:443/depot/1/chunk/ab"
        );
        assert_eq!(s.url("/p", Some("?token=x")), "https://h:443/p?token=x");
        assert_eq!(s.url("/p", Some("token=x")), "https://h:443/p?token=x");
        assert_eq!(s.url("/p", Some("")), "https://h:443/p");
    }

    fn servers(n: usize) -> Vec<CdnServer> {
        (0..n)
            .map(|i| CdnServer {
                host: format!("s{i}"),
                port: 80 + i as u16,
                https: false,
            })
            .collect()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A pool over `answers.len()` servers, probed with `answers` in server
    /// order.
    async fn probed(answers: &[Probed], cfg: &CdnConfig) -> CdnPool {
        let pool = CdnPool::new(servers(answers.len()), cfg).unwrap();
        let answers = answers.to_vec();
        let probe = FakeProbe(move |s: &CdnServer| {
            answers[s.host[1..].parse::<usize>().expect("servers() names")]
        });
        pool.probe(&probe, cfg.probe_timeout).await;
        pool
    }

    /// Server `i` answered a request sent just now after `ttfb`.
    fn sample(pool: &CdnPool, i: usize, ttfb: Duration) {
        pool.report_latency(i, Instant::now(), ttfb);
    }

    /// Server `i` answers its first request: the connection is made, and
    /// later samples count.
    fn connect(pool: &CdnPool, i: usize) {
        sample(pool, i, ms(1000));
    }

    #[test]
    fn an_unprobed_pool_keeps_to_the_first_server() {
        let cfg = CdnConfig {
            cooldown_base: Duration::from_secs(60),
            ..CdnConfig::default()
        };
        let pool = CdnPool::new(servers(3), &cfg).unwrap();
        for _ in 0..3 {
            assert_eq!(pool.pick(), (0, Duration::ZERO)); // no rotation
        }
        pool.report_failure(0, None);
        assert_eq!(pool.pick(), (1, Duration::ZERO));
        assert_eq!(pool.pick(), (1, Duration::ZERO));
        assert!(CdnPool::new(vec![], &cfg).is_err());
    }

    #[tokio::test]
    async fn pool_prefers_the_fastest_host_and_stays_on_it() {
        let answers = [ms(30), ms(5), ms(20)].map(Probed::Takes);
        let pool = probed(&answers, &CdnConfig::default()).await;
        for _ in 0..4 {
            assert_eq!(pool.pick(), (1, Duration::ZERO));
            pool.report_success(1);
        }
    }

    #[tokio::test]
    async fn pool_fails_over_to_the_next_fastest_and_returns_after_cooldown() {
        let cfg = CdnConfig {
            cooldown_base: ms(40),
            ..CdnConfig::default()
        };
        let answers = [ms(30), ms(5), ms(20)].map(Probed::Takes);
        let pool = probed(&answers, &cfg).await;
        assert_eq!(pool.pick().0, 1);
        pool.report_failure(1, None);
        // The next fastest, not the next in the list; and it stays there.
        assert_eq!(pool.pick(), (2, Duration::ZERO));
        assert_eq!(pool.pick(), (2, Duration::ZERO));
        std::thread::sleep(ms(80));
        assert_eq!(pool.pick(), (1, Duration::ZERO), "tried once it has cooled");
        pool.report_success(1);
        assert_eq!(pool.pick(), (1, Duration::ZERO));
        assert_eq!(pool.pick(), (1, Duration::ZERO), "and back for good");
    }

    #[tokio::test]
    async fn a_recovering_host_gets_one_trial_request_not_the_traffic() {
        let cfg = CdnConfig {
            cooldown_base: ms(40),
            ..CdnConfig::default()
        };
        let answers = [ms(30), ms(5), ms(20)].map(Probed::Takes);
        let pool = probed(&answers, &cfg).await;
        assert_eq!(pool.pick().0, 1);
        pool.report_failure(1, None);
        assert_eq!(pool.pick().0, 2);
        std::thread::sleep(ms(80));
        // Its cooldown is over, and it still has the best figure. Of the
        // requests made now, one tries it; the rest stay where they were.
        assert_eq!(pool.pick(), (1, Duration::ZERO));
        assert_eq!(pool.pick(), (2, Duration::ZERO));
        assert_eq!(pool.pick(), (2, Duration::ZERO));
        // The trial fails (or hangs until its timeout): nobody else waited.
        pool.report_failure(1, None);
        assert_eq!(pool.pick(), (2, Duration::ZERO));
        std::thread::sleep(ms(160)); // the second cooldown is 80 ms
        assert_eq!(pool.pick(), (1, Duration::ZERO));
        assert_eq!(pool.pick(), (2, Duration::ZERO));
        // A 403 answered with a fresh token ends the trial undecided: the
        // retry is the next trial.
        pool.report_retry(1);
        assert_eq!(pool.pick(), (1, Duration::ZERO));
        assert_eq!(pool.pick(), (2, Duration::ZERO));
        // This one succeeds, and the host has the traffic back.
        pool.report_success(1);
        assert_eq!(pool.pick(), (1, Duration::ZERO));
        assert_eq!(pool.pick(), (1, Duration::ZERO));
    }

    #[tokio::test]
    async fn a_trial_that_never_reports_is_offered_again_after_the_request_timeout() {
        let cfg = CdnConfig {
            cooldown_base: ms(10),
            request_timeout: ms(60),
            ..CdnConfig::default()
        };
        let pool = probed(&[ms(5), ms(50)].map(Probed::Takes), &cfg).await;
        pool.report_failure(0, None);
        std::thread::sleep(ms(30));
        assert_eq!(pool.pick().0, 0); // the trial, whose future is then dropped
        assert_eq!(pool.pick().0, 1);
        std::thread::sleep(ms(90));
        assert_eq!(pool.pick().0, 0);
        assert_eq!(pool.pick().0, 1);
    }

    #[tokio::test]
    async fn a_host_that_does_not_answer_stops_ranking_by_its_connect_time() {
        let pool = probed(&[ms(5), ms(50)].map(Probed::Takes), &CdnConfig::default()).await;
        pool.report_stall(0, Duration::from_secs(60));
        assert_eq!(pool.latency(0), Latency::Measured(Duration::from_secs(60)));
        // Never the other way: a connection refused at once is not fast.
        pool.report_stall(1, ms(1));
        assert_eq!(pool.latency(1), Latency::Measured(ms(50)));
        let unprobed = CdnPool::new(servers(1), &CdnConfig::default()).unwrap();
        unprobed.report_stall(0, ms(1));
        assert_eq!(unprobed.latency(0), Latency::Unknown);
    }

    #[tokio::test]
    async fn a_timed_out_request_counts_against_the_hosts_latency() {
        let cdn = crate::testutil::FakeCdn::start().await;
        cdn.set_mode(crate::testutil::Mode::Hang);
        let cfg = CdnConfig {
            max_attempts: 1,
            request_timeout: ms(100),
            ..CdnConfig::default()
        };
        let fetcher = Fetcher::new(AppId(1), DepotId(1), vec![cdn.server()], cfg, None).unwrap();
        let fast = FakeProbe(|_: &CdnServer| Probed::Takes(ms(5)));
        fetcher.pool.probe(&fast, ms(50)).await;
        fetcher
            .fetch::<Bytes, _>("/x", "test", "steam 1 x", 100, Ok)
            .await
            .unwrap_err();
        let Latency::Measured(d) = fetcher.pool.latency(0) else {
            panic!("the probe measured it");
        };
        assert!(d >= ms(100), "{d:?}");
    }

    #[tokio::test]
    async fn a_host_left_for_being_slow_is_returned_to_when_its_samples_are_old() {
        let cfg = CdnConfig {
            cooldown_max: ms(60),
            ..CdnConfig::default()
        };
        let pool = probed(&[ms(10), ms(50)].map(Probed::Takes), &cfg).await;
        assert_eq!(pool.pick().0, 0);
        connect(&pool, 0);
        // A local stall: every answer of the best host takes two seconds.
        for _ in 0..4 {
            sample(&pool, 0, Duration::from_secs(2));
        }
        assert_eq!(pool.pick().0, 1, "the other host is clearly faster now");
        connect(&pool, 1);
        sample(&pool, 1, ms(55));
        assert_eq!(pool.pick().0, 1);
        // Nothing has measured host 0 since. Its bad figure is forgotten,
        // it ranks by its probe time again, and the pool goes back.
        std::thread::sleep(ms(120));
        assert!(pool.latency(0) > Latency::Measured(Duration::from_secs(1)));
        assert_eq!(pool.pick().0, 0);
        assert_eq!(pool.latency(0), Latency::Measured(ms(10)));
        assert_eq!(pool.pick().0, 0);
    }

    #[tokio::test]
    async fn the_first_burst_on_a_cold_connection_is_not_sampled() {
        let pool = probed(&[ms(10), ms(12)].map(Probed::Takes), &CdnConfig::default()).await;
        assert_eq!(pool.pick().0, 0);
        // Four chunks go out at once on no connection. Each waits for TCP
        // and TLS: 40 ms to first byte against a 10 ms probe.
        let sent = Instant::now();
        for _ in 0..4 {
            pool.report_latency(0, sent, ms(40));
            assert_eq!(pool.latency(0), Latency::Measured(ms(10)));
            assert_eq!(pool.pick().0, 0, "no walk to the neighbour");
        }
        // Requests sent on the open connection are samples.
        sample(&pool, 0, ms(14));
        assert_eq!(pool.latency(0), Latency::Measured(ms(11)));
        // A failure may have cost the connection: the next answer is cold.
        pool.report_failure(0, None);
        pool.report_success(0);
        sample(&pool, 0, ms(40));
        assert_eq!(pool.latency(0), Latency::Measured(ms(11)));
    }

    #[tokio::test]
    async fn a_silent_host_does_not_hold_up_the_ranking_of_the_others() {
        let pool = Arc::new(CdnPool::new(servers(3), &CdnConfig::default()).unwrap());
        let probing = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let probe = FakeProbe(|s: &CdnServer| match s.host.as_str() {
                    "s0" => Probed::Takes(ms(30)),
                    "s1" => Probed::Hangs,
                    _ => Probed::Takes(ms(5)),
                });
                pool.probe(&probe, Duration::from_secs(30)).await
            })
        };
        tokio::time::sleep(ms(50)).await;
        assert!(!probing.is_finished(), "still waiting for s1");
        assert_eq!(pool.latency(2), Latency::Measured(ms(5)));
        assert_eq!(pool.pick().0, 2);
        probing.abort();
    }

    #[tokio::test]
    async fn preconnect_goes_to_the_chosen_host_and_counts_as_its_connection() {
        let pool = probed(&[ms(30), ms(5)].map(Probed::Takes), &CdnConfig::default()).await;
        let pre = crate::testutil::FakePreconnect::default();
        pool.preconnect(&pre).await;
        assert_eq!(*pre.0.lock().unwrap(), [81]);
        // So the first request's time to first byte is a sample already.
        sample(&pool, 1, ms(9));
        assert_eq!(pool.latency(1), Latency::Measured(ms(6)));
    }

    #[tokio::test]
    async fn the_http_preconnect_never_fails_and_says_whether_it_connected() {
        let cfg = CdnConfig {
            connect_timeout: ms(200),
            ..CdnConfig::default()
        };
        let pre = HttpPreconnect {
            http: http_client(&cfg).unwrap(),
            timeout: ms(200),
        };
        let cdn = crate::testutil::FakeCdn::start().await;
        assert!(pre.connect(&cdn.server()).await, "a 404 is a connection");
        assert_eq!(cdn.log(), ["/"]);
        let dead = CdnServer {
            host: "127.0.0.1".into(),
            port: 1,
            https: false,
        };
        assert!(!pre.connect(&dead).await);
        cdn.set_mode(crate::testutil::Mode::Hang);
        assert!(!pre.connect(&cdn.server()).await);
    }

    #[tokio::test]
    async fn when_every_server_is_cooling_the_earliest_recovery_wins() {
        let cfg = CdnConfig {
            cooldown_base: Duration::from_secs(60),
            cooldown_max: Duration::from_secs(60),
            ..CdnConfig::default()
        };
        let answers = [ms(30), ms(5), ms(20)].map(Probed::Takes);
        let pool = probed(&answers, &cfg).await;
        pool.report_failure(1, None);
        pool.report_failure(2, None);
        pool.report_failure(0, Some(Duration::from_secs(5)));
        let (i, wait) = pool.pick();
        assert_eq!(i, 0);
        assert!(wait > Duration::from_secs(4) && wait <= Duration::from_secs(5));
        pool.report_success(2);
        assert_eq!(pool.pick(), (2, Duration::ZERO));
    }

    #[tokio::test]
    async fn a_dead_or_silent_host_in_the_probe_ranks_last_and_stays_usable() {
        let cfg = CdnConfig {
            cooldown_base: Duration::from_secs(60),
            probe_timeout: ms(50),
            ..CdnConfig::default()
        };
        let answers = [Probed::Fails, Probed::Hangs, Probed::Takes(ms(400))];
        let started = Instant::now();
        let pool = probed(&answers, &cfg).await;
        assert!(started.elapsed() < Duration::from_secs(2), "the timeout");
        assert_eq!(pool.latency(0), Latency::Unreachable);
        assert_eq!(pool.latency(1), Latency::Unreachable);
        assert_eq!(pool.pick(), (2, Duration::ZERO));
        // Last, not gone: they take over when the measured host fails.
        pool.report_failure(2, None);
        assert_eq!(pool.pick(), (0, Duration::ZERO));
        // One that answers a request after all is measured like any other.
        connect(&pool, 0);
        assert_eq!(pool.latency(0), Latency::Unreachable);
        sample(&pool, 0, ms(7));
        assert_eq!(pool.latency(0), Latency::Measured(ms(7)));

        // Every probe failing leaves a working pool in list order.
        let pool = probed(&[Probed::Fails, Probed::Fails], &cfg).await;
        assert_eq!(pool.pick(), (0, Duration::ZERO));
        pool.report_failure(0, None);
        assert_eq!(pool.pick(), (1, Duration::ZERO));
    }

    #[tokio::test]
    async fn a_late_probe_moves_the_pool_off_the_first_server() {
        let pool = CdnPool::new(servers(2), &CdnConfig::default()).unwrap();
        assert_eq!(pool.pick().0, 0);
        // Server 0 is far: its requests take 400 ms to answer.
        connect(&pool, 0);
        sample(&pool, 0, ms(400));
        assert_eq!(pool.latency(0), Latency::Measured(ms(400)));
        let probe =
            FakeProbe(|s: &CdnServer| Probed::Takes(if s.host == "s0" { ms(40) } else { ms(5) }));
        pool.probe(&probe, ms(50)).await;
        assert_eq!(pool.latency(0), Latency::Measured(ms(40)), "the probe wins");
        assert_eq!(pool.pick().0, 1);
    }

    #[tokio::test]
    async fn latency_is_a_moving_average_and_only_a_clear_lead_moves_the_pool() {
        let answers = [ms(10), ms(12)].map(Probed::Takes);
        let pool = probed(&answers, &CdnConfig::default()).await;
        assert_eq!(pool.pick().0, 0);
        connect(&pool, 0);
        // One slow answer: 10 -> 15 ms. Server 1 now has the lower figure,
        // but not by enough to pay for a new connection.
        sample(&pool, 0, ms(30));
        assert_eq!(pool.latency(0), Latency::Measured(ms(15)));
        assert_eq!(pool.pick().0, 0);
        // It really has become slow: over twice server 1's 12 ms.
        for _ in 0..8 {
            sample(&pool, 0, ms(100));
        }
        assert_eq!(pool.pick().0, 1);
        // And the pool does not flap back on a small difference.
        connect(&pool, 1);
        sample(&pool, 1, ms(60));
        assert_eq!(pool.pick().0, 1);
    }

    #[tokio::test]
    async fn the_tcp_probe_times_a_connect_and_reports_a_closed_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = CdnServer {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            https: true, // no TLS is spoken: the probe only connects
        };
        let took = TcpProbe.connect(&server).await.unwrap();
        assert!(took < Duration::from_secs(1), "{took:?}");
        drop(listener);
        assert!(TcpProbe.connect(&server).await.is_err());
    }

    #[tokio::test]
    async fn the_tcp_probe_takes_whichever_address_connects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        // Nothing listens on port 1: as an address this machine cannot
        // reach, listed first.
        let closed: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
        connect_any(vec![closed, open]).await.unwrap();
        assert!(connect_any(vec![closed]).await.is_err());
        assert!(connect_any(vec![]).await.is_err());
    }

    #[tokio::test]
    async fn fetch_feeds_time_to_first_byte_into_the_pool() {
        let cdn = crate::testutil::FakeCdn::start().await;
        cdn.put("/x", vec![7u8; 10]);
        let fetcher = Fetcher::new(
            AppId(1),
            DepotId(1),
            vec![cdn.server()],
            CdnConfig::default(),
            None,
        )
        .unwrap();
        let fetch = || fetcher.fetch::<Bytes, _>("/x", "test", "steam 1 x", 100, Ok);
        // The first answer only shows the connection is made.
        fetch().await.unwrap();
        assert_eq!(fetcher.pool.latency(0), Latency::Unknown);
        fetch().await.unwrap();
        assert!(matches!(fetcher.pool.latency(0), Latency::Measured(_)));
    }

    #[test]
    fn cooldown_doubles_and_is_capped() {
        let cfg = CdnConfig {
            cooldown_base: Duration::from_secs(1),
            cooldown_max: Duration::from_secs(3),
            ..CdnConfig::default()
        };
        let pool = CdnPool::new(servers(1), &cfg).unwrap();
        for secs in [1u64, 2, 3, 3] {
            pool.report_failure(0, None);
            let (_, wait) = pool.pick();
            assert!(
                wait <= Duration::from_secs(secs)
                    && wait > Duration::from_millis(secs * 1000 - 200),
                "{wait:?}"
            );
        }
    }

    #[tokio::test]
    async fn transport_error_never_leaks_the_cdn_token_from_the_url() {
        let cfg = CdnConfig {
            max_attempts: 1,
            connect_timeout: Duration::from_millis(200),
            ..CdnConfig::default()
        };
        let fetcher = Fetcher::new(AppId(1), DepotId(1), servers(1), cfg, None).unwrap();
        // Nothing listens on this port, so the GET fails at the transport
        // level with a reqwest error that (unless stripped) prints the
        // request URL, token and all, in its Display/Debug.
        let err = fetcher
            .get(
                "http://127.0.0.1:1/x?token=SECRETVALUE",
                100,
                &mut GetStats::default(),
            )
            .await
            .unwrap_err();
        let msg = match err {
            HttpFail::Transport(m) => m,
            HttpFail::Status(..) => panic!("expected a transport error"),
        };
        assert!(!msg.contains("SECRETVALUE"), "{msg}");
        assert!(!msg.contains("127.0.0.1:1"), "{msg}");
    }

    #[tokio::test]
    async fn fetch_rejects_a_body_over_the_cap_via_content_length() {
        let cdn = crate::testutil::FakeCdn::start().await;
        cdn.put("/big", vec![7u8; 1000]);
        let cfg = CdnConfig {
            max_attempts: 1,
            ..CdnConfig::default()
        };
        let fetcher = Fetcher::new(AppId(1), DepotId(1), vec![cdn.server()], cfg, None).unwrap();
        let err = fetcher
            .fetch::<Bytes, _>("/big", "test", "steam 1 big", 100, Ok)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("cap"), "{err}");
        assert_eq!(cdn.log().len(), 1); // one request, no retries needed to see it
    }
}
