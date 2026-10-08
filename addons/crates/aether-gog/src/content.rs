//! Builds, build details, depot manifests (cached on disk) and the secure
//! CDN links chunks are fetched from.
//!
//! Ported from NexusMods.App `src/NexusMods.Networking.GOG/Client.cs`
//! (GPL-3.0).
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aether_net::{Http, SourceError};
use serde::Deserialize;
use url::Url;

use crate::api::Api;
use crate::config::{GogConfig, endpoint};
use crate::credentials::{GogCredentials, now_unix};
use crate::error::GogError;
use crate::fsutil::{read_optional, write_atomic};
use crate::ids::{Os, ProductId};
use crate::manifest::{
    Build, BuildDetails, DepotManifest, DepotRef, parse_builds, valid_manifest_id,
};
use crate::reader::{ChunkFetch, ChunkLru, GogDepotFile};

const MAX_JSON_BODY: u64 = 16 << 20;
/// Compressed build details / depot manifest bodies.
const MAX_META_BODY: u64 = 256 << 20;
/// A secure link is used until this long before it expires.
const LINK_MARGIN_SECS: u64 = 60;
/// How long a secure link without an `expires_at` is used.
const LINK_DEFAULT_TTL_SECS: u64 = 3600;
/// Secure-link parameter values at least this long are scrubbed from error
/// text (shorter ones, like `dirs=2`, cannot be secrets and would mangle it).
const MIN_SECRET_LEN: usize = 6;

/// GOG content for one login. Cheap to clone.
///
/// Every method is `async` and needs a Tokio runtime; a file read from a
/// synchronous thread goes through [`GogDepotFile::into_blocking`], whose
/// handle must belong to a multi-thread runtime.
#[derive(Clone)]
pub struct GogContent {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) http: Http,
    cfg: GogConfig,
    api: Api,
    manifests: Mutex<HashMap<String, Arc<DepotManifest>>>,
    links: tokio::sync::Mutex<HashMap<ProductId, SecureLink>>,
    pub(crate) chunks: Mutex<ChunkLru>,
    /// Chunk downloads under way, by compressed MD5: a read needing one
    /// awaits it instead of fetching it again.
    pub(crate) inflight: Mutex<HashMap<[u8; 16], ChunkFetch>>,
}

impl GogContent {
    /// Content for the login saved at `cfg.credentials`;
    /// [`GogError::NotLoggedIn`] when there is none.
    pub async fn open(http: Http, cfg: GogConfig) -> Result<Self, GogError> {
        let creds = GogCredentials::load(&cfg.credentials)?.ok_or(GogError::NotLoggedIn)?;
        Ok(GogContent::with_credentials(http, cfg, creds))
    }

    /// Content for `creds` (refreshed tokens are still saved to
    /// `cfg.credentials`).
    pub fn with_credentials(http: Http, cfg: GogConfig, creds: GogCredentials) -> Self {
        let chunks = Mutex::new(ChunkLru::new(cfg.chunk_cache_chunks, cfg.chunk_cache_bytes));
        GogContent {
            inner: Arc::new(Inner {
                api: Api::new(http.clone(), cfg.clone(), creds),
                http,
                cfg,
                manifests: Mutex::default(),
                links: tokio::sync::Mutex::default(),
                chunks,
                inflight: Mutex::default(),
            }),
        }
    }

    pub fn config(&self) -> &GogConfig {
        &self.inner.cfg
    }

    /// The v2 builds of `product` for `os`, as GOG lists them (newest
    /// first). Empty when GOG knows none for this account.
    pub async fn builds(&self, product: ProductId, os: Os) -> Result<Vec<Build>, GogError> {
        let mut url = endpoint(
            &self.inner.cfg.content_base,
            &format!("/products/{product}/os/{os}/builds"),
        );
        url.set_query(Some("generation=2"));
        match self.inner.api.get(&url, MAX_JSON_BODY).await {
            Ok(body) => parse_builds(&body),
            Err(GogError::Source(SourceError::NotFound { .. })) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// A build's details: its depots.
    pub async fn build_details(&self, build: &Build) -> Result<BuildDetails, GogError> {
        let body = self
            .inner
            .http
            .get_bytes(&build.link, MAX_META_BODY)
            .await?;
        BuildDetails::parse(&body)
    }

    /// A depot's manifest: from memory, the disk cache, or the CDN.
    pub async fn depot(&self, depot: &DepotRef) -> Result<Arc<DepotManifest>, GogError> {
        let id = depot.manifest.to_ascii_lowercase();
        if !valid_manifest_id(&id) {
            return Err(GogError::json(
                "GOG build details",
                format!("bad depot manifest id {:?}", depot.manifest),
            ));
        }
        if let Some(m) = self.inner.manifests.lock().unwrap().get(&id) {
            return Ok(m.clone());
        }
        let path = self.manifest_path(&id);
        let cached = match read_optional(&path)? {
            Some(body) => match DepotManifest::parse(&body) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::warn!(manifest = %id, error = %e, "ignoring bad cached GOG manifest");
                    None
                }
            },
            None => None,
        };
        let manifest = match cached {
            Some(m) => m,
            None => {
                let url = endpoint(
                    &self.inner.cfg.cdn_meta_base,
                    &format!("/content-system/v2/meta/{}/{}/{id}", &id[..2], &id[2..4]),
                );
                let body = self
                    .inner
                    .http
                    .get_bytes(url.as_str(), MAX_META_BODY)
                    .await?;
                let m = DepotManifest::parse(&body)?;
                write_atomic(&path, &body, false)?;
                m
            }
        };
        let m = Arc::new(manifest);
        self.inner.manifests.lock().unwrap().insert(id, m.clone());
        Ok(m)
    }

    /// Open `path` (any case, `/` or `\`) of `depot`, whose chunks are served
    /// under `product`'s secure link (a depot's [`DepotRef::product_id`]).
    /// Fetches the secure link now, so a product the account does not own
    /// fails here ([`GogError::NotOwned`]).
    pub async fn file(
        &self,
        product: ProductId,
        depot: &DepotManifest,
        path: &str,
    ) -> Result<GogDepotFile, GogError> {
        let item = depot
            .find(path)
            .ok_or_else(|| GogError::NotInDepot(path.to_string()))?;
        let (chunks, base) = match (item.sfc_ref, &depot.small_files_container) {
            (Some(r), Some(sfc)) => (sfc.clone(), r.offset),
            _ => (item.chunks.clone(), 0),
        };
        let have: u64 = chunks.iter().map(|c| c.size).sum();
        if base.checked_add(item.size).is_none_or(|end| end > have) {
            return Err(GogError::json(
                "GOG depot manifest",
                format!(
                    "{}: {} bytes at {base} but its chunks hold {have}",
                    item.path, item.size
                ),
            ));
        }
        self.secure_link(product).await?;
        Ok(GogDepotFile::new(
            self.clone(),
            product,
            chunks.into(),
            base,
            item.size,
        ))
    }

    fn manifest_path(&self, id: &str) -> PathBuf {
        self.inner.cfg.cache_dir.join("manifests").join(id)
    }

    /// `product`'s CDN link, cached until shortly before it expires.
    pub(crate) async fn secure_link(&self, product: ProductId) -> Result<SecureLink, GogError> {
        let mut links = self.inner.links.lock().await;
        if let Some(l) = links.get(&product)
            && l.valid_until > now_unix()
        {
            return Ok(l.clone());
        }
        let mut url = endpoint(
            &self.inner.cfg.content_base,
            &format!("/products/{product}/secure_link"),
        );
        url.query_pairs_mut()
            .append_pair("generation", "2")
            .append_pair("_version", "2")
            .append_pair("path", "/");
        let body = match self.inner.api.get(&url, MAX_JSON_BODY).await {
            Err(GogError::Source(SourceError::Status { status: 403, .. })) => {
                return Err(GogError::NotOwned(product));
            }
            r => r?,
        };
        let link = SecureLink::parse(&body)?;
        links.insert(product, link.clone());
        Ok(link)
    }

    /// Forget `product`'s link if it is still `refused`, the one the CDN
    /// rejected. A download whose refusal comes after another's has
    /// already put a fresh link in its place drops nothing.
    pub(crate) async fn drop_secure_link(&self, product: ProductId, refused: &SecureLink) {
        let mut links = self.inner.links.lock().await;
        if links.get(&product) == Some(refused) {
            links.remove(&product);
        }
    }
}

/// A CDN location for one product's chunks: `url_format` with `{name}`
/// placeholders filled from `parameters`, the chunk's
/// `/{md5[0..2]}/{md5[2..4]}/{md5}` appended to the `path` parameter, as
/// NexusMods.App's `ChunkedStreamSource.cs` and heroic-gogdl's
/// `merge_url_with_params` build it.
///
/// The template may put secrets in the URL *path* (`~token=…`), where
/// [`aether_net::error::redact`] does not look, so errors name a chunk by
/// [`shown_chunk_url`](Self::shown_chunk_url) and pass through
/// [`scrub`](Self::scrub).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SecureLink {
    url_format: String,
    params: Vec<(String, String)>,
    path: String,
    valid_until: u64,
}

#[derive(Deserialize)]
struct RawLinks {
    urls: Vec<RawLink>,
}

#[derive(Deserialize)]
struct RawLink {
    url_format: String,
    parameters: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    priority: i64,
}

impl SecureLink {
    fn parse(body: &[u8]) -> Result<SecureLink, GogError> {
        let what = "GOG secure link";
        let mut raw: RawLinks =
            serde_json::from_slice(body).map_err(|e| GogError::json(what, e))?;
        // As NexusMods.App: order by priority, use the first.
        raw.urls.sort_by_key(|u| u.priority);
        let first = raw
            .urls
            .into_iter()
            .next()
            .ok_or_else(|| GogError::json(what, "no URLs"))?;
        let mut path = None;
        let mut expires = None;
        let mut params = Vec::new();
        for (k, v) in first.parameters {
            let s = match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            match k.as_str() {
                "path" => path = Some(s),
                _ => {
                    if k == "expires_at" {
                        expires = s.parse::<u64>().ok();
                    }
                    params.push((k, s));
                }
            }
        }
        let path = path.ok_or_else(|| GogError::json(what, "no `path` parameter"))?;
        let now = now_unix();
        let valid_until = match expires {
            Some(e) => e.saturating_sub(LINK_MARGIN_SECS),
            None => now + LINK_DEFAULT_TTL_SECS,
        };
        Ok(SecureLink {
            url_format: first.url_format,
            params,
            path,
            valid_until,
        })
    }

    /// The URL of the chunk whose compressed MD5 is `md5` (lowercase hex).
    pub(crate) fn chunk_url(&self, md5: &str) -> Result<Url, SourceError> {
        Url::parse(&self.fill(md5, false)).map_err(|e| SourceError::Protocol {
            url: aether_net::error::redact_raw(&self.fill(md5, true)),
            msg: format!("GOG secure link gave a bad URL: {e}"),
        })
    }

    /// [`chunk_url`](Self::chunk_url) with every parameter but `base_url`
    /// and `path` replaced by `REDACTED`: what errors and logs may show.
    pub(crate) fn shown_chunk_url(&self, md5: &str) -> Url {
        let shown = self.fill(md5, true);
        Url::parse(&shown)
            .unwrap_or_else(|_| Url::parse(&format!("gog-cdn:chunk/{md5}")).expect("a valid URL"))
    }

    fn fill(&self, md5: &str, redacted: bool) -> String {
        let path = format!("{}/{}/{}/{md5}", self.path, &md5[..2], &md5[2..4]);
        let mut u = self.url_format.clone();
        for (k, v) in self
            .params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .chain([("path", path.as_str())])
        {
            let v = if redacted && k != "base_url" && k != "path" {
                "REDACTED"
            } else {
                v
            };
            u = u.replace(&format!("{{{k}}}"), v);
        }
        u
    }

    /// `e` with this link's secret parameter values cut out of every text
    /// it carries (an error page may echo the request path).
    pub(crate) fn scrub(&self, e: SourceError) -> SourceError {
        let clean = |s: String| {
            self.params
                .iter()
                .filter(|(k, v)| k != "base_url" && v.len() >= MIN_SECRET_LEN)
                .fold(s, |s, (_, v)| s.replace(v.as_str(), "REDACTED"))
        };
        match e {
            SourceError::Status { url, status, body } => SourceError::Status {
                url: clean(url),
                status,
                body: clean(body),
            },
            SourceError::Protocol { url, msg } => SourceError::Protocol {
                url: clean(url),
                msg: clean(msg),
            },
            SourceError::NotFound { what } => SourceError::NotFound { what: clean(what) },
            SourceError::CorruptPart { what, msg } => SourceError::CorruptPart {
                what: clean(what),
                msg: clean(msg),
            },
            SourceError::Network { url, source } => SourceError::Network {
                url: clean(url),
                source,
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_urls_fill_the_template() {
        let body = br#"{"product_id":1,"type":"depot","urls":[
            {"endpoint_name":"b","url_format":"{base_url}/b{path}","parameters":
              {"base_url":"https://b.example","path":"/store/1"},"priority":5},
            {"endpoint_name":"a","url_format":"{base_url}/token=nva={expires_at}~dirs={dirs}~token={token}{path}",
             "parameters":{"base_url":"https://a.example","path":"/content-system/v2/store/1",
               "token":"T","expires_at":2000000000,"dirs":2},"priority":1}]}"#;
        let l = SecureLink::parse(body).unwrap();
        assert_eq!(l.valid_until, 2_000_000_000 - 60);
        let md5 = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            l.chunk_url(md5).unwrap().as_str(),
            "https://a.example/token=nva=2000000000~dirs=2~token=T/content-system/v2/store/1/01/23/0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            l.shown_chunk_url(md5).as_str(),
            "https://a.example/token=nva=REDACTED~dirs=REDACTED~token=REDACTED/content-system/v2/store/1/01/23/0123456789abcdef0123456789abcdef"
        );
        assert!(SecureLink::parse(br#"{"urls":[]}"#).is_err());
    }
}
