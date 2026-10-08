//! Depot keys, manifests and CDN servers, from the disk cache when present
//! and from Steam otherwise. A CM session is opened only on a cache miss.
use crate::cache::SteamCache;
use crate::cdn::{
    CdnConfig, CdnObserver, CdnPool, CdnServer, CdnTokenSource, Fetcher, HttpPreconnect,
    Preconnect, Probe, TcpProbe, http_client,
};
use crate::cm::SessionConfig;
use crate::credentials::SteamCredentials;
use crate::error::SteamError;
use crate::ids::{AppId, DepotId, DepotKey, ManifestId};
use crate::manifest::DepotManifest;
use crate::reader::DepotReader;
use crate::session::SteamSession;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::OnceCell;

/// How long a cached CDN server list is trusted.
pub const CDN_SERVER_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

/// A `OnceCell` initialized the same way as `cell`, so a builder method that
/// rebuilds `Inner` to replace one field does not silently drop another
/// already-set session.
fn clone_once_cell<T: Clone>(cell: &OnceCell<T>) -> OnceCell<T> {
    match cell.get() {
        Some(v) => OnceCell::new_with(Some(v.clone())),
        None => OnceCell::new(),
    }
}

/// Steam content with a disk cache in front. Cheap to clone.
///
/// Every method here is `async` and does its own networking, so calling
/// this type at all requires being inside a Tokio runtime. If a reader
/// obtained through it is then converted to a [`BlockingDepotFile`](crate::BlockingDepotFile)
/// for use from a synchronous thread (e.g. a Wabbajack `VFS` callback),
/// that conversion's `Handle` must belong to a **multi-thread** runtime —
/// see [`SteamDepotFile::into_blocking`](crate::SteamDepotFile::into_blocking)
/// for why a `current_thread` runtime does not work there.
#[derive(Clone)]
pub struct SteamContent {
    inner: Arc<Inner>,
}

struct Inner {
    cache: SteamCache,
    credentials: Option<SteamCredentials>,
    session_cfg: SessionConfig,
    cdn_cfg: CdnConfig,
    session: OnceCell<SteamSession>,
    anonymous: OnceCell<SteamSession>,
    probe: Arc<dyn Probe>,
    /// `None`: a request on the shared client (see [`HttpPreconnect`]).
    preconnect: Option<Arc<dyn Preconnect>>,
    observer: Option<Arc<dyn CdnObserver>>,
    net: Arc<Net>,
}

/// What every depot reader of a [`SteamContent`] shares: one HTTP client,
/// so the depots of a game reuse the same connection, and one server pool
/// per app, so they agree on the fastest host.
#[derive(Default)]
struct Net {
    http: Mutex<Option<reqwest::Client>>,
    pools: Mutex<HashMap<AppId, Arc<CdnPool>>>,
    /// The probe-and-connect tasks started so far, for tests to wait on.
    #[cfg(test)]
    probes: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl SteamContent {
    /// `credentials` is the saved login, if any. Without it, anything not
    /// already cached fails with [`SteamError::NotLoggedIn`], except the CDN
    /// server list, which an anonymous session can fetch.
    pub fn new(
        cache: SteamCache,
        credentials: Option<SteamCredentials>,
        session_cfg: SessionConfig,
        cdn_cfg: CdnConfig,
    ) -> Self {
        SteamContent {
            inner: Arc::new(Inner {
                cache,
                credentials,
                session_cfg,
                cdn_cfg,
                session: OnceCell::new(),
                anonymous: OnceCell::new(),
                probe: Arc::new(TcpProbe),
                preconnect: None,
                observer: None,
                net: Arc::default(),
            }),
        }
    }

    /// A copy of `Inner` for a builder method to change one field of.
    /// Sessions already set, and the shared client and pools, carry over.
    fn rebuild(&self) -> Inner {
        let i = &self.inner;
        Inner {
            cache: i.cache.clone(),
            credentials: i.credentials.clone(),
            session_cfg: i.session_cfg.clone(),
            cdn_cfg: i.cdn_cfg.clone(),
            session: clone_once_cell(&i.session),
            anonymous: clone_once_cell(&i.anonymous),
            probe: i.probe.clone(),
            preconnect: i.preconnect.clone(),
            observer: i.observer.clone(),
            net: i.net.clone(),
        }
    }

    /// Use `session` instead of logging on when Steam is needed.
    pub fn with_session(self, session: SteamSession) -> Self {
        let mut inner = self.rebuild();
        inner.session = OnceCell::new_with(Some(session));
        SteamContent {
            inner: Arc::new(inner),
        }
    }

    /// Tell `observer` about every CDN download of the readers made from
    /// here on (see [`CdnObserver`]).
    pub fn with_observer(self, observer: Arc<dyn CdnObserver>) -> Self {
        let mut inner = self.rebuild();
        inner.observer = Some(observer);
        SteamContent {
            inner: Arc::new(inner),
        }
    }

    /// Rank CDN hosts with `probe` instead of connecting to them.
    /// Test-only: production code always probes with a TCP connect.
    #[cfg(test)]
    pub(crate) fn with_probe(self, probe: Arc<dyn Probe>) -> Self {
        let mut inner = self.rebuild();
        inner.probe = probe;
        SteamContent {
            inner: Arc::new(inner),
        }
    }

    /// Record the host to connect to ahead of the first request with
    /// `preconnect` instead of connecting to it. Test-only.
    #[cfg(test)]
    pub(crate) fn with_preconnect(self, preconnect: Arc<dyn Preconnect>) -> Self {
        let mut inner = self.rebuild();
        inner.preconnect = Some(preconnect);
        SteamContent {
            inner: Arc::new(inner),
        }
    }

    /// Wait for every probe, and the connection that follows it, started
    /// so far.
    #[cfg(test)]
    pub(crate) async fn probes_done(&self) {
        let probes = std::mem::take(&mut *self.inner.net.probes.lock().unwrap());
        for p in probes {
            p.await.unwrap();
        }
    }

    /// Use `session` instead of logging on anonymously when Steam is needed
    /// with no credentials (the CDN server list refresh path). Test-only:
    /// production code always dials a real anonymous session.
    #[cfg(test)]
    pub(crate) fn with_anonymous_session(self, session: SteamSession) -> Self {
        let mut inner = self.rebuild();
        inner.anonymous = OnceCell::new_with(Some(session));
        SteamContent {
            inner: Arc::new(inner),
        }
    }

    pub fn cache(&self) -> &SteamCache {
        &self.inner.cache
    }

    /// The logged-on session, connecting on first use.
    pub async fn session(&self) -> Result<&SteamSession, SteamError> {
        let i = &self.inner;
        i.session
            .get_or_try_init(|| async {
                let creds = i.credentials.as_ref().ok_or(SteamError::NotLoggedIn)?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                if creds.is_expired_at(now) {
                    return Err(SteamError::LoginExpired {
                        account: creds.account_name.clone(),
                    });
                }
                SteamSession::login(creds, i.session_cfg.clone()).await
            })
            .await
    }

    pub async fn depot_key(&self, app: AppId, depot: DepotId) -> Result<DepotKey, SteamError> {
        if let Some(k) = self.inner.cache.depot_key(depot)? {
            return Ok(k);
        }
        let k = self.session().await?.depot_key(app, depot).await?;
        // The key is already in hand; a failure to cache it costs a
        // re-fetch next time, not this call's result.
        if let Err(e) = self.inner.cache.put_depot_key(depot, &k) {
            tracing::warn!(error = %e, %depot, "failed to cache the depot key");
        }
        Ok(k)
    }

    /// CDN servers for `app`: the fresh cached list when there is one,
    /// otherwise a live fetch — logged-in when credentials are usable,
    /// anonymous when they are not (absent, or the session rejects them as
    /// [`SteamError::NotLoggedIn`]/[`SteamError::LoginExpired`]). If that
    /// fetch fails for any reason (no network, expired login, Steam down)
    /// and a stale cached list is on disk, that stale list is returned
    /// instead of failing outright — offline play should keep working past
    /// the 24h freshness window, just without a guarantee the list is
    /// current.
    pub async fn cdn_servers(&self, app: AppId) -> Result<Vec<CdnServer>, SteamError> {
        let i = &self.inner;
        if let Some(s) = i.cache.cdn_servers(app, CDN_SERVER_MAX_AGE)? {
            return Ok(s);
        }
        match self.fetch_cdn_servers(app).await {
            Ok(servers) => {
                // Same here: the list is already in hand, so a cache-write
                // failure is a warning, not a reason to fail this call.
                if let Err(e) = i.cache.put_cdn_servers(app, &servers) {
                    tracing::warn!(error = %e, %app, "failed to cache the CDN server list");
                }
                Ok(servers)
            }
            Err(e) => match i.cache.cdn_servers_any_age(app)? {
                Some(stale) => {
                    tracing::warn!(error = %e, %app, "CDN server list refresh failed; using stale cached list");
                    Ok(stale)
                }
                None => Err(e),
            },
        }
    }

    /// One live attempt at the CDN server list, with no cache fallback.
    async fn fetch_cdn_servers(&self, app: AppId) -> Result<Vec<CdnServer>, SteamError> {
        let i = &self.inner;
        let session = if i.credentials.is_some() || i.session.initialized() {
            match self.session().await {
                Ok(s) => s,
                Err(SteamError::NotLoggedIn | SteamError::LoginExpired { .. }) => {
                    i.anonymous
                        .get_or_try_init(|| SteamSession::anonymous(i.session_cfg.clone()))
                        .await?
                }
                Err(e) => return Err(e),
            }
        } else {
            i.anonymous
                .get_or_try_init(|| SteamSession::anonymous(i.session_cfg.clone()))
                .await?
        };
        let servers = session.cdn_servers(app).await?;
        if servers.is_empty() {
            return Err(SteamError::Cdn(format!(
                "Steam listed no CDN servers for app {app}"
            )));
        }
        Ok(servers)
    }

    /// The HTTP client every reader shares, built on first use.
    fn http(&self) -> Result<reqwest::Client, SteamError> {
        let mut http = self.inner.net.http.lock().unwrap();
        if let Some(c) = &*http {
            return Ok(c.clone());
        }
        let c = http_client(&self.inner.cdn_cfg)?;
        *http = Some(c.clone());
        Ok(c)
    }

    /// The server pool every reader of `app` shares. Building it starts the
    /// connect probe that ranks its hosts, on a task of its own: nothing
    /// waits for the probe, and until it lands the pool keeps to the first
    /// server of the list. Once the hosts are ranked the same task opens a
    /// connection to the chosen one, so the first chunk does not wait for
    /// TCP and TLS. A probe that reaches no host connects to none.
    async fn pool(&self, app: AppId) -> Result<Arc<CdnPool>, SteamError> {
        let i = &self.inner;
        if let Some(p) = i.net.pools.lock().unwrap().get(&app) {
            return Ok(p.clone());
        }
        let servers = self.cdn_servers(app).await?;
        let pool = Arc::new(CdnPool::new(servers, &i.cdn_cfg)?);
        let preconnect: Arc<dyn Preconnect> = match &i.preconnect {
            Some(p) => p.clone(),
            None => Arc::new(HttpPreconnect {
                http: self.http()?,
                timeout: i.cdn_cfg.connect_timeout,
            }),
        };
        {
            let mut pools = i.net.pools.lock().unwrap();
            if let Some(p) = pools.get(&app) {
                return Ok(p.clone()); // another reader built it meanwhile
            }
            pools.insert(app, pool.clone());
        }
        let (probed, probe, timeout) = (pool.clone(), i.probe.clone(), i.cdn_cfg.probe_timeout);
        let task = tokio::spawn(async move {
            if probed.probe(&*probe, timeout).await {
                probed.preconnect(&*preconnect).await;
            }
        });
        #[cfg(test)]
        i.net.probes.lock().unwrap().push(task);
        #[cfg(not(test))]
        drop(task);
        Ok(pool)
    }

    /// A reader for `depot`. CDN 403s are answered with auth tokens from the
    /// logged-on session, opened only if a server asks for one. Readers
    /// share one HTTP client, and those of one app one server pool.
    pub async fn reader(&self, app: AppId, depot: DepotId) -> Result<DepotReader, SteamError> {
        let key = self.depot_key(app, depot).await?;
        let pool = self.pool(app).await?;
        let tokens: Arc<dyn CdnTokenSource> = Arc::new(self.clone());
        let fetcher = Fetcher::shared(
            app,
            depot,
            self.http()?,
            pool,
            self.inner.cdn_cfg.clone(),
            Some(tokens),
            self.inner.observer.clone(),
        );
        Ok(DepotReader::from_fetcher(fetcher, key))
    }

    /// Manifest `id` of `depot`, fetched and cached on first use.
    pub async fn manifest(
        &self,
        app: AppId,
        depot: DepotId,
        id: ManifestId,
    ) -> Result<Arc<DepotManifest>, SteamError> {
        if let Some(m) = self.inner.cache.manifest(depot, id)? {
            return Ok(Arc::new(m));
        }
        let code = self
            .session()
            .await?
            .manifest_request_code(app, depot, id)
            .await?;
        let m = match self
            .reader(app, depot)
            .await?
            .fetch_manifest(id, code)
            .await
        {
            Ok(m) => m,
            Err(e) if is_filename_decrypt_failure(&e) => {
                // The key decrypted nothing sensible — almost certainly a
                // corrupted or stale cache entry, not a CDN fluke (retrying
                // across servers, which `fetch_manifest` already did, would
                // fail identically every time with the same wrong key).
                // Drop it so the next call asks Steam for a fresh one, and
                // report the real integrity failure rather than the CDN
                // retry-loop error it arrived wrapped in.
                if let Err(fe) = self.inner.cache.forget_depot_key(depot) {
                    tracing::warn!(error = %fe, %depot, "failed to drop the stale cached depot key");
                }
                return Err(SteamError::Integrity(e.to_string()));
            }
            Err(e) => return Err(e),
        };
        if let Err(e) = self.inner.cache.put_manifest(&m) {
            tracing::warn!(error = %e, depot = %m.depot(), id = %m.id(), "failed to cache the manifest");
        }
        Ok(Arc::new(m))
    }
}

/// Whether `e` is (possibly wrapped by the CDN fetcher's retry loop) a
/// [`SteamError::Integrity`] specifically about failing to decrypt a
/// manifest's file names — the one integrity failure that means "this
/// depot key is wrong," as opposed to e.g. a corrupt chunk or a manifest
/// that doesn't match the depot/id asked for.
fn is_filename_decrypt_failure(e: &SteamError) -> bool {
    e.to_string().contains("file names:")
}

impl CdnTokenSource for SteamContent {
    fn cdn_auth_token<'a>(
        &'a self,
        app: AppId,
        depot: DepotId,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, SteamError>> + Send + 'a>> {
        Box::pin(async move { self.session().await?.cdn_auth_token(app, depot, host).await })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cm::tests::{Behave, fake, fake_with_cdn};
    use crate::testutil::{
        FakeCdn, FakePreconnect, FakeProbe, FixtureFile, Probed, manifest_body, split,
    };

    pub(crate) fn fast() -> (SessionConfig, CdnConfig) {
        let cdn = CdnConfig {
            request_timeout: Duration::from_millis(500),
            cooldown_base: Duration::from_millis(10),
            max_attempts: 3,
            ..CdnConfig::default()
        };
        (SessionConfig::default(), cdn)
    }

    /// The key the fake CM hands out for depot `d`.
    pub(crate) fn fake_key(d: u32) -> DepotKey {
        DepotKey([d as u8; 32])
    }

    /// A fake CDN serving `files` as manifest `manifest` of `depot`
    /// (request code 77), encrypted with [`fake_key`].
    pub(crate) async fn serve_depot(
        depot: u32,
        manifest: u64,
        files: &[FixtureFile<'_>],
    ) -> FakeCdn {
        let cdn = FakeCdn::start().await;
        for f in files {
            cdn.put_chunks(DepotId(depot), &split(f.data, f.chunk), &fake_key(depot));
        }
        cdn.put(
            &format!("/depot/{depot}/manifest/{manifest}/5/77"),
            manifest_body(DepotId(depot), ManifestId(manifest), files, &[], None),
        );
        cdn
    }

    #[tokio::test]
    async fn cold_start_fetches_once_then_needs_no_session() {
        let files = [FixtureFile {
            path: "steam_api64.dll",
            data: b"MZ fake dll bytes",
            chunk: 8,
        }];
        let cdn = serve_depot(1, 2, &files).await;
        let dir = tempfile::tempdir().unwrap();
        let cache = SteamCache::new(dir.path());
        let (cm, calls) = fake_with_cdn(vec![], vec![cdn.server()]).await;
        let (s, c) = fast();
        let content = SteamContent::new(cache.clone(), None, s.clone(), c.clone())
            .with_session(SteamSession::from_cm(cm, Some("alice".into())));
        let m = content
            .manifest(AppId(10), DepotId(1), ManifestId(2))
            .await
            .unwrap();
        let f = content
            .reader(AppId(10), DepotId(1))
            .await
            .unwrap()
            .open(m, "STEAM_API64.DLL")
            .unwrap();
        assert_eq!(f.read_range(0, f.len()).await.unwrap(), files[0].data);
        let rpcs = calls.calls.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(rpcs, 3, "key, manifest code, CDN servers");
        // A new instance on the same cache, with no login at all, still reads.
        let offline = SteamContent::new(cache, None, s, c);
        let m = offline
            .manifest(AppId(10), DepotId(1), ManifestId(2))
            .await
            .unwrap();
        let f = offline
            .reader(AppId(10), DepotId(1))
            .await
            .unwrap()
            .open(m, "steam_api64.dll")
            .unwrap();
        assert_eq!(f.read_range(3, 4).await.unwrap(), &files[0].data[3..7]);
    }

    /// One fake CDN serving a one-chunk file from each of depots 1 and 3
    /// (manifests 10 and 30), and that file's bytes.
    async fn two_depots(cdn: &FakeCdn) -> &'static [u8] {
        let data: &[u8] = b"the same bytes in both depots";
        for (depot, manifest) in [(1, 10), (3, 30)] {
            let files = [FixtureFile {
                path: "a.bin",
                data,
                chunk: 1024,
            }];
            cdn.put_chunks(DepotId(depot), &split(data, 1024), &fake_key(depot));
            cdn.put(
                &format!("/depot/{depot}/manifest/{manifest}/5/77"),
                manifest_body(DepotId(depot), ManifestId(manifest), &files, &[], None),
            );
        }
        data
    }

    #[tokio::test]
    async fn depot_readers_share_one_client_and_keep_its_connection() {
        let cdn = FakeCdn::start_keep_alive().await;
        let data = two_depots(&cdn).await;
        let dir = tempfile::tempdir().unwrap();
        let (cm, _) = fake_with_cdn(vec![], vec![cdn.server()]).await;
        let (s, c) = fast();
        let content = SteamContent::new(SteamCache::new(dir.path()), None, s, c)
            .with_session(SteamSession::from_cm(cm, Some("alice".into())))
            // The real probe would connect to the fake CDN and be counted.
            .with_probe(Arc::new(FakeProbe(|_: &CdnServer| Probed::Fails)));
        for (depot, manifest) in [(1, 10), (3, 30)] {
            let m = content
                .manifest(AppId(10), DepotId(depot), ManifestId(manifest))
                .await
                .unwrap();
            let reader = content.reader(AppId(10), DepotId(depot)).await.unwrap();
            let f = reader.open(m, "a.bin").unwrap();
            assert_eq!(f.read_range(0, f.len()).await.unwrap(), data);
        }
        assert_eq!(cdn.log().len(), 4, "two manifests, two chunks");
        // Four readers were made (`manifest` makes its own); with a client
        // each, every one would have connected.
        assert_eq!(cdn.connections(), 1);
    }

    #[tokio::test]
    async fn the_first_chunk_reuses_the_connection_opened_after_the_probe() {
        let cdn = FakeCdn::start_keep_alive().await;
        let data = two_depots(&cdn).await;
        let dir = tempfile::tempdir().unwrap();
        let (cm, _) = fake_with_cdn(vec![], vec![cdn.server()]).await;
        let (s, c) = fast();
        // The real pre-connect, on the shared client; the probe is faked
        // because the real one would connect to the fake CDN and be counted.
        let content = SteamContent::new(SteamCache::new(dir.path()), None, s, c)
            .with_session(SteamSession::from_cm(cm, Some("alice".into())))
            .with_probe(Arc::new(FakeProbe(|_: &CdnServer| {
                Probed::Takes(Duration::from_millis(5))
            })));
        let reader = content.reader(AppId(10), DepotId(1)).await.unwrap();
        content.probes_done().await;
        assert_eq!(cdn.log(), ["/"], "no path, token or key in the request");
        assert_eq!(cdn.connections(), 1);
        let m = content
            .manifest(AppId(10), DepotId(1), ManifestId(10))
            .await
            .unwrap();
        let f = reader.open(m, "a.bin").unwrap();
        assert_eq!(f.read_range(0, f.len()).await.unwrap(), data);
        assert_eq!(cdn.connections(), 1, "the requests found it open");
    }

    #[tokio::test]
    async fn a_probe_that_reaches_no_host_connects_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let dead = CdnServer {
            host: "127.0.0.1".into(),
            port: 1,
            https: false,
        };
        let (cm, _) = fake_with_cdn(vec![], vec![dead]).await;
        let (s, c) = fast();
        let connected = Arc::new(FakePreconnect::default());
        let content = SteamContent::new(SteamCache::new(dir.path()), None, s, c)
            .with_session(SteamSession::from_cm(cm, Some("alice".into())))
            .with_probe(Arc::new(FakeProbe(|_: &CdnServer| Probed::Fails)))
            .with_preconnect(connected.clone());
        // The reader is still made: the probe never fails anything.
        content.reader(AppId(10), DepotId(1)).await.unwrap();
        content.probes_done().await;
        assert!(connected.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_probe_sends_every_depot_to_the_fastest_host() {
        let (far, near) = (FakeCdn::start().await, FakeCdn::start().await);
        let data = two_depots(&far).await;
        two_depots(&near).await;
        let dir = tempfile::tempdir().unwrap();
        let (cm, _) = fake_with_cdn(vec![], vec![far.server(), near.server()]).await;
        let (s, c) = fast();
        let near_port = near.server().port;
        let connected = Arc::new(FakePreconnect::default());
        let content = SteamContent::new(SteamCache::new(dir.path()), None, s, c)
            .with_session(SteamSession::from_cm(cm, Some("alice".into())))
            .with_probe(Arc::new(FakeProbe(move |s: &CdnServer| {
                Probed::Takes(Duration::from_millis(if s.port == near_port {
                    5
                } else {
                    80
                }))
            })))
            .with_preconnect(connected.clone());
        // The first reader builds the app's pool and starts the probe.
        let first = content.reader(AppId(10), DepotId(1)).await.unwrap();
        content.probes_done().await;
        for (depot, manifest) in [(1, 10), (3, 30)] {
            let m = content
                .manifest(AppId(10), DepotId(depot), ManifestId(manifest))
                .await
                .unwrap();
            let reader = content.reader(AppId(10), DepotId(depot)).await.unwrap();
            let f = reader.open(m, "a.bin").unwrap();
            assert_eq!(f.read_range(0, f.len()).await.unwrap(), data);
        }
        assert_eq!(near.log().len(), 4, "two manifests, two chunks");
        assert!(far.log().is_empty(), "{:?}", far.log());
        // One probe for the app, however many readers, and one connection
        // ahead of the requests: to the host the probe chose.
        assert!(content.inner.net.probes.lock().unwrap().is_empty());
        assert_eq!(*connected.0.lock().unwrap(), [near_port]);
        drop(first);
    }

    #[tokio::test]
    async fn a_bad_cached_depot_key_is_dropped_and_reported_as_an_integrity_error() {
        let good_key = DepotKey([9u8; 32]);
        let wrong_key = DepotKey([1u8; 32]);
        let files = [FixtureFile {
            path: "Data\\Skyrim.esm",
            data: b"TES4",
            chunk: 1024,
        }];
        let cdn = FakeCdn::start().await;
        cdn.put(
            "/depot/1/manifest/2/5/77",
            manifest_body(DepotId(1), ManifestId(2), &files, &[], Some(&good_key)),
        );
        let dir = tempfile::tempdir().unwrap();
        let cache = SteamCache::new(dir.path());
        // A depot key never changes in reality, but a corrupted or stale
        // cache entry can still be wrong; simulate that directly instead
        // of trying to corrupt the file on disk.
        cache.put_depot_key(DepotId(1), &wrong_key).unwrap();
        let (cm, _) = fake_with_cdn(vec![], vec![cdn.server()]).await;
        let (s, c) = fast();
        let content = SteamContent::new(cache.clone(), None, s, c)
            .with_session(SteamSession::from_cm(cm, Some("alice".into())));
        let err = content
            .manifest(AppId(10), DepotId(1), ManifestId(2))
            .await
            .unwrap_err();
        assert!(matches!(err, SteamError::Integrity(_)), "{err}");
        assert_eq!(
            cache.depot_key(DepotId(1)).unwrap(),
            None,
            "the bad cached key must be dropped, not repeated forever"
        );
    }

    #[tokio::test]
    async fn an_expired_token_is_reported_without_touching_the_network() {
        let creds = expired_creds("alice");
        let dir = tempfile::tempdir().unwrap();
        let (s, c) = fast();
        let content = SteamContent::new(SteamCache::new(dir.path()), Some(creds), s, c);
        let err = content.depot_key(AppId(10), DepotId(1)).await.unwrap_err();
        assert!(matches!(&err, SteamError::LoginExpired { account } if account == "alice"));
        assert!(err.to_string().contains("log in to Steam again"));
    }

    fn expired_creds(account: &str) -> SteamCredentials {
        use base64::Engine;
        let e = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        let token = format!("{}.{}.sig", e("{}"), e(r#"{"exp":1000}"#));
        SteamCredentials::new(account.into(), token, None)
    }

    /// Write a CDN server list to `cache`'s root as though it were fetched
    /// at Unix time 0 — always older than [`CDN_SERVER_MAX_AGE`].
    fn write_stale_cdn_servers(cache: &SteamCache, app: AppId, servers: &[CdnServer]) {
        let json = format!(
            r#"{{"fetched_at":0,"servers":{}}}"#,
            serde_json::to_string(servers).unwrap()
        );
        std::fs::write(cache.root().join(format!("cdn-servers-{app}.json")), json).unwrap();
    }

    #[tokio::test]
    async fn stale_cdn_list_with_expired_creds_refreshes_anonymously() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SteamCache::new(dir.path());
        let stale = vec![CdnServer {
            host: "old".into(),
            port: 443,
            https: true,
        }];
        write_stale_cdn_servers(&cache, AppId(10), &stale);
        let fresh = vec![CdnServer {
            host: "fresh".into(),
            port: 443,
            https: true,
        }];
        let (anon_cm, anon_calls) = fake_with_cdn(vec![], fresh.clone()).await;
        let (s, c) = fast();
        let content = SteamContent::new(cache.clone(), Some(expired_creds("alice")), s, c)
            .with_anonymous_session(SteamSession::from_cm(anon_cm, None));
        let servers = content.cdn_servers(AppId(10)).await.unwrap();
        assert_eq!(
            servers, fresh,
            "expired creds fall through to the anonymous session"
        );
        assert_eq!(
            anon_calls.calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        // The refreshed list is now cached fresh, not just returned once.
        assert_eq!(
            cache.cdn_servers(AppId(10), CDN_SERVER_MAX_AGE).unwrap(),
            Some(fresh)
        );
    }

    #[tokio::test]
    async fn stale_cdn_list_survives_a_refresh_failure() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SteamCache::new(dir.path());
        let stale = vec![CdnServer {
            host: "old".into(),
            port: 443,
            https: true,
        }];
        write_stale_cdn_servers(&cache, AppId(10), &stale);
        let (anon_cm, _) = fake(vec![Behave::Refuse]).await;
        let (s, c) = fast();
        let content = SteamContent::new(cache, None, s, c)
            .with_anonymous_session(SteamSession::from_cm(anon_cm, None));
        let servers = content.cdn_servers(AppId(10)).await.unwrap();
        assert_eq!(servers, stale);
    }

    #[tokio::test]
    async fn no_cdn_list_and_a_failed_refresh_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SteamCache::new(dir.path());
        let (anon_cm, _) = fake(vec![Behave::Refuse]).await;
        let (s, c) = fast();
        let content = SteamContent::new(cache, None, s, c)
            .with_anonymous_session(SteamSession::from_cm(anon_cm, None));
        let err = content.cdn_servers(AppId(10)).await.unwrap_err();
        assert!(matches!(err, SteamError::AccessDenied(_)), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_cache_write_failure_does_not_fail_the_call() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cache = SteamCache::new(dir.path());
        // The key is already in hand by the time it's cached. Make the
        // depot-keys directory searchable (a miss still reads as "not
        // found") but not writable, so caching the freshly fetched key
        // fails, and check the key is still returned.
        let keys_dir = dir.path().join("depot-keys");
        std::fs::create_dir(&keys_dir).unwrap();
        std::fs::set_permissions(&keys_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let (cm, _) = fake(vec![Behave::Answer]).await;
        let (s, c) = fast();
        let content = SteamContent::new(cache, None, s, c)
            .with_session(SteamSession::from_cm(cm, Some("alice".into())));
        let result = content.depot_key(AppId(1), DepotId(7)).await;
        std::fs::set_permissions(&keys_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(result.unwrap(), DepotKey([7; 32]));
    }

    #[tokio::test]
    async fn uncached_without_login_says_how_to_fix_it() {
        let dir = tempfile::tempdir().unwrap();
        let (s, c) = fast();
        let content = SteamContent::new(SteamCache::new(dir.path()), None, s, c);
        let err = content.depot_key(AppId(10), DepotId(1)).await.unwrap_err();
        assert!(matches!(err, SteamError::NotLoggedIn));
        assert!(err.to_string().contains("log in to Steam first"));
    }
}
