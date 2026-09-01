//! Resolving GE-Proton releases from the GitHub API.
//!
//! Parses with [`serde_json::Value`] rather than deriving structs: the
//! releases endpoint returns far more than we need (author objects,
//! reactions, upload URLs, ...) and a derived struct would break the moment
//! GitHub adds an unrelated field. [`parse_releases`] is pure and is the only
//! entry point any test exercises; [`fetch_releases`] is the thin networked
//! wrapper around it that no test calls.

use crate::runtime::cmp_tags;

/// One usable GE-Proton release: an x86_64 tarball plus its digest.
///
/// Releases lacking either asset never become a `Release` — see
/// [`parse_releases`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub tarball_url: String,
    pub digest_url: String,
    pub size: u64,
}

/// Why release resolution failed.
#[derive(Debug)]
pub enum ResolveError {
    /// The HTTP request itself failed (network, non-2xx status, ...).
    Http(String),
    /// The response body was not the JSON array of releases we expected.
    Json(String),
    /// No release had a usable x86_64 asset pair.
    NoAsset(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Http(e) => write!(f, "http error: {e}"),
            ResolveError::Json(e) => write!(f, "json error: {e}"),
            ResolveError::NoAsset(e) => write!(f, "no usable asset: {e}"),
        }
    }
}

impl std::error::Error for ResolveError {}

const RELEASES_URL: &str =
    "https://api.github.com/repos/GloriousEggroll/proton-ge-custom/releases?per_page=30";

/// Parses a GitHub releases API response body into the [`Release`]s that
/// have a usable x86_64 asset pair.
///
/// For each element of the top-level array, reads `tag_name` and scans
/// `assets` for names ending in `-x86_64.tar.gz` and `-x86_64.sha512sum`. A
/// release missing either asset is skipped rather than erroring: the
/// upstream project has published incomplete releases before, and an
/// aarch64-only release must never leak through as if it were usable on
/// x86_64.
pub fn parse_releases(json: &str) -> Result<Vec<Release>, ResolveError> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| ResolveError::Json(e.to_string()))?;
    let entries = value
        .as_array()
        .ok_or_else(|| ResolveError::Json("expected a top-level JSON array".to_string()))?;

    let mut releases = Vec::new();
    for entry in entries {
        let Some(tag) = entry.get("tag_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(assets) = entry.get("assets").and_then(|v| v.as_array()) else {
            continue;
        };

        let mut tarball: Option<(String, u64)> = None;
        let mut digest: Option<String> = None;
        for asset in assets {
            let Some(name) = asset.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(url) = asset.get("browser_download_url").and_then(|v| v.as_str()) else {
                continue;
            };
            if name.ends_with("-x86_64.tar.gz") {
                let size = asset.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                tarball = Some((url.to_string(), size));
            } else if name.ends_with("-x86_64.sha512sum") {
                digest = Some(url.to_string());
            }
        }

        if let (Some((tarball_url, size)), Some(digest_url)) = (tarball, digest) {
            releases.push(Release {
                tag: tag.to_string(),
                tarball_url,
                digest_url,
                size,
            });
        }
    }

    Ok(releases)
}

/// Picks the newest release by [`cmp_tags`], optionally constrained to a
/// single major series (the `N` in `GE-ProtonN-M`).
pub fn pick(releases: &[Release], major: Option<u32>) -> Option<&Release> {
    releases
        .iter()
        .filter(|r| match major {
            Some(major) => tag_major(&r.tag) == Some(major),
            None => true,
        })
        .max_by(|a, b| cmp_tags(&a.tag, &b.tag))
}

/// Extracts the `N` from a `GE-ProtonN-M` tag.
fn tag_major(tag: &str) -> Option<u32> {
    let rest = tag.strip_prefix("GE-Proton")?;
    let (n, _) = rest.split_once('-')?;
    n.parse().ok()
}

/// GETs the GitHub releases endpoint and parses the response with
/// [`parse_releases`]. GitHub rejects requests with no `User-Agent`, so one
/// is always set.
///
/// No test calls this: the real response is fetched from the network, and
/// the point of [`parse_releases`] being pure is that this wrapper needs no
/// test of its own beyond "it calls parse_releases on the body".
pub fn fetch_releases(agent: &ureq::Agent) -> Result<Vec<Release>, ResolveError> {
    let body = agent
        .get(RELEASES_URL)
        .header("User-Agent", "aether-vfs (vfs-proton)")
        .call()
        .map_err(|e| ResolveError::Http(e.to_string()))?
        .body_mut()
        .read_to_string()
        .map_err(|e| ResolveError::Http(e.to_string()))?;
    let releases = parse_releases(&body)?;
    if releases.is_empty() {
        return Err(ResolveError::NoAsset(
            "no release had a usable x86_64 asset pair".to_string(),
        ));
    }
    Ok(releases)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/releases.json"),
        )
        .unwrap()
    }

    #[test]
    fn parses_tag_urls_and_size() {
        let rs = parse_releases(&fixture()).unwrap();
        let r = rs.iter().find(|r| r.tag == "GE-Proton11-6").unwrap();
        assert!(r.tarball_url.ends_with("GE-Proton11-6-x86_64.tar.gz"));
        assert!(r.digest_url.ends_with("GE-Proton11-6-x86_64.sha512sum"));
        assert_eq!(r.size, 533_700_853);
    }

    #[test]
    fn skips_releases_with_no_x86_64_asset() {
        // The aarch64-only release must not appear: taking "the first asset"
        // would install an ARM runtime on an x86_64 host.
        let rs = parse_releases(&fixture()).unwrap();
        assert!(
            rs.iter().all(|r| r.tarball_url.contains("x86_64")),
            "an aarch64-only release leaked through"
        );
    }

    #[test]
    fn pick_takes_the_newest_and_honours_a_major_series() {
        let rs = parse_releases(&fixture()).unwrap();
        assert_eq!(pick(&rs, None).unwrap().tag, "GE-Proton11-6");
        assert_eq!(pick(&rs, Some(11)).unwrap().tag, "GE-Proton11-6");
        assert_eq!(pick(&rs, Some(9)).unwrap().tag, "GE-Proton9-1");
        assert!(pick(&rs, Some(42)).is_none());
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(matches!(parse_releases("not json"), Err(ResolveError::Json(_))));
        assert!(matches!(parse_releases("{}"), Err(ResolveError::Json(_))));
    }
}
