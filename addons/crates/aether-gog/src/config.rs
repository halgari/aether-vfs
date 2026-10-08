use std::path::PathBuf;

use url::Url;

/// GOG Galaxy's own OAuth client, the one open-source GOG tools use for a
/// paste-the-redirect-URL login: heroic-gogdl / Heroic, lgogdownloader and
/// minigalaxy (`minigalaxy/api.py`) all send this id and secret with
/// [`GALAXY_REDIRECT_URI`]. It is not NexusMods.App's client
/// (`58276512461627742`, `Client.cs`): that one is registered for the
/// redirect `nxm://gog-auth`, which needs a URL-scheme handler and which
/// GOG's token endpoint requires to match, so it cannot serve a terminal
/// login. Both are overridable in [`GogConfig`].
pub const GALAXY_CLIENT_ID: &str = "46899977096215655";
/// See [`GALAXY_CLIENT_ID`]. Public: it ships in every GOG Galaxy install.
pub const GALAXY_CLIENT_SECRET: &str =
    "9d85c43b1482497dbbce61f6e4aa173a433796eeae2ca8c5f6129f2dc4de46d9";
/// The page GOG sends the browser to after login; its address carries the
/// `code` the user pastes back.
pub const GALAXY_REDIRECT_URI: &str = "https://embed.gog.com/on_login_success?origin=client";

/// Endpoints, OAuth client and file locations. Every URL is a field so tests
/// can point the crate at a local fake.
#[derive(Clone, Debug)]
pub struct GogConfig {
    /// `https://auth.gog.com`: `{auth_base}/auth` and `{auth_base}/token`.
    pub auth_base: Url,
    /// `https://api.gog.com` (product data; not used by the v2 content
    /// path yet, kept so every GOG host is configurable in one place).
    pub api_base: Url,
    /// `https://content-system.gog.com`: build lists and secure links.
    pub content_base: Url,
    /// `https://cdn.gog.com`: `content-system/v2/meta/…` documents.
    pub cdn_meta_base: Url,
    /// Depot manifests are cached under `{cache_dir}/manifests`.
    pub cache_dir: PathBuf,
    /// The saved login (JSON, mode 0600).
    pub credentials: PathBuf,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    /// Inflated chunks kept in memory, at most this many …
    pub chunk_cache_chunks: usize,
    /// … and at most this many bytes.
    pub chunk_cache_bytes: usize,
}

impl GogConfig {
    /// GOG's real endpoints and the Galaxy OAuth client.
    pub fn new(cache_dir: impl Into<PathBuf>, credentials: impl Into<PathBuf>) -> Self {
        let url = |s: &str| Url::parse(s).expect("constant URL");
        GogConfig {
            auth_base: url("https://auth.gog.com"),
            api_base: url("https://api.gog.com"),
            content_base: url("https://content-system.gog.com"),
            cdn_meta_base: url("https://cdn.gog.com"),
            cache_dir: cache_dir.into(),
            credentials: credentials.into(),
            client_id: GALAXY_CLIENT_ID.into(),
            client_secret: GALAXY_CLIENT_SECRET.into(),
            redirect_uri: GALAXY_REDIRECT_URI.into(),
            chunk_cache_chunks: 64,
            chunk_cache_bytes: 256 << 20,
        }
    }
}

/// `base` joined with `path` (which starts with `/`), keeping any path
/// `base` already has.
pub(crate) fn endpoint(base: &Url, path: &str) -> Url {
    let mut u = base.clone();
    let joined = format!("{}{path}", base.path().trim_end_matches('/'));
    u.set_path(&joined);
    u.set_query(None);
    u
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_keep_the_base_path() {
        let b = Url::parse("http://127.0.0.1:1/auth").unwrap();
        assert_eq!(
            endpoint(&b, "/token").as_str(),
            "http://127.0.0.1:1/auth/token"
        );
        let b = Url::parse("https://auth.gog.com").unwrap();
        assert_eq!(
            endpoint(&b, "/token").as_str(),
            "https://auth.gog.com/token"
        );
    }
}
