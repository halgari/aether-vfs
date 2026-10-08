//! The Wabbajack CDN: a file is a gzipped JSON definition
//! ([`CdnDefinition`]) plus parts, each checked against its xxHash64 and
//! fetched in parallel through an [`aether_net::Http`]. [`remap_cdn_url`]
//! maps the legacy Bunny CDN hosts Wabbajack still lists to their current
//! names.

use std::io::Read;
use std::sync::Arc;

use aether_archive::Xxh64;
use futures_util::stream::{self, StreamExt};
use serde::Deserialize;
use url::Url;
use xxhash_rust::xxh64::Xxh64 as Hasher;

use aether_net::error::{Result, SourceError, redact, redact_raw};
use aether_net::events::Job;
use aether_net::http::{Http, check, read_body, read_body_max};
use aether_net::sink::{BlobSink, write_blocking};

/// Legacy Bunny CDN hosts and their current names, from Wabbajack's
/// `WabbajackCDNDownloader.DomainRemaps`.
const DOMAIN_REMAPS: [(&str, &str); 4] = [
    ("wabbajack.b-cdn.net", "authored-files.wabbajack.org"),
    ("wabbajack-mirror.b-cdn.net", "mirror.wabbajack.org"),
    ("wabbajack-patches.b-cdn.net", "patches.wabbajack.org"),
    ("wabbajacktest.b-cdn.net", "test-files.wabbajack.org"),
];
/// A gzipped definition larger than this is not a definition.
const MAX_DEFINITION: u64 = 64 << 20;
/// A part larger than this is refused (real parts are 2 MiB).
const MAX_CDN_PART: u64 = 64 << 20;

/// `<url>/definition.json.gz` of a Wabbajack CDN file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CdnDefinition {
    pub original_file_name: String,
    pub size: u64,
    pub hash: Xxh64,
    pub parts: Vec<CdnPart>,
    #[serde(default)]
    pub munged_name: String,
}

/// One part, fetched from `<url>/parts/<index>`; parts are 2 MiB except the last.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CdnPart {
    pub size: u64,
    pub offset: u64,
    pub hash: Xxh64,
    pub index: u64,
}

/// Apply Wabbajack's legacy-host remap (`*.b-cdn.net` -> `*.wabbajack.org`).
pub fn remap_cdn_url(url: &Url) -> Url {
    let mut out = url.clone();
    if let Some(host) = url.host_str()
        && let Some((_, to)) = DOMAIN_REMAPS
            .iter()
            .find(|(from, _)| host.eq_ignore_ascii_case(from))
    {
        out.set_host(Some(to))
            .expect("remap targets are valid hosts");
    }
    out
}

/// A file on the Wabbajack CDN: a definition plus 2 MiB parts, each
/// verified with xxHash64, fetched in parallel.
///
/// Requests use the full URL, query included (it may carry a token); labels
/// and errors show it only without its query and fragment.
#[derive(Clone)]
pub struct CdnFile {
    http: Http,
    /// The remapped file URL, query included.
    url: Url,
    /// `url` without its query, fragment or trailing slash, for labels.
    label: String,
}

impl std::fmt::Debug for CdnFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CdnFile")
            .field("url", &self.label)
            .finish_non_exhaustive()
    }
}

impl CdnFile {
    pub fn new(http: Http, url: &str) -> Result<CdnFile> {
        let parsed = Url::parse(url).map_err(|e| SourceError::Protocol {
            url: redact_raw(url),
            msg: format!("not a URL: {e}"),
        })?;
        let url = remap_cdn_url(&parsed);
        let label = redact(&url).trim_end_matches('/').to_string();
        Ok(CdnFile { http, url, label })
    }

    /// The file URL after remapping, without its query or fragment.
    pub fn url(&self) -> &str {
        &self.label
    }

    /// `<url>/<suffix>`, keeping the file URL's query.
    fn join(&self, suffix: &str) -> Url {
        let mut u = self.url.clone();
        let path = format!("{}/{suffix}", self.url.path().trim_end_matches('/'));
        u.set_path(&path);
        u.set_fragment(None);
        u
    }

    /// Fetch and check `definition.json.gz`.
    pub async fn definition(&self) -> Result<CdnDefinition> {
        let job = self
            .http
            .start(format!("cdn definition {}", self.label), None);
        let r = self.definition_inner(&job).await;
        job.complete(r)
    }

    async fn definition_inner(&self, job: &Job) -> Result<CdnDefinition> {
        let url = self.join("definition.json.gz");
        let bytes = self
            .http
            .config()
            .retry
            .run(job, || async {
                let _permit = self.http.permit(&url).await;
                let resp = self
                    .http
                    .client()
                    .get(url.clone())
                    .send()
                    .await
                    .map_err(|e| SourceError::network(&url, e))?;
                read_body_max(check(resp, &url).await?, &url, job, MAX_DEFINITION).await
            })
            .await?;
        parse_definition(&bytes, &url)
    }

    /// Download every part into `sink` (in order), verifying each part and
    /// the whole file; also checks the whole-file hash against `expected`
    /// (the modlist's archive hash). Returns the definition.
    pub async fn download(
        &self,
        sink: Arc<dyn BlobSink>,
        expected: Option<Xxh64>,
    ) -> Result<CdnDefinition> {
        let def = self.definition().await?;
        let job = self
            .http
            .start(format!("cdn {}", def.original_file_name), Some(def.size));
        let r = self.download_parts(&def, &sink, expected, &job).await;
        job.complete(r.map(|()| def))
    }

    async fn download_parts(
        &self,
        def: &CdnDefinition,
        sink: &Arc<dyn BlobSink>,
        expected: Option<Xxh64>,
        job: &Job,
    ) -> Result<()> {
        let cfg = self.http.config();
        // Iterate owned parts: a stream over `&def.parts` makes the
        // download future non-`Send` (higher-ranked borrow).
        let mut parts = stream::iter(def.parts.clone())
            .map(|p| async move {
                let data = cfg.retry.run(job, || self.fetch_part(def, &p, job)).await?;
                Ok::<_, SourceError>((p.offset, data))
            })
            .buffered(cfg.parallel_parts.max(1));
        let mut hasher = Hasher::new(0);
        while let Some(part) = parts.next().await {
            let (off, data) = part?;
            hasher.update(&data);
            write_blocking(sink, off, data).await?;
        }
        let actual = Xxh64(hasher.digest());
        for want in [Some(def.hash), expected].into_iter().flatten() {
            if want != actual {
                return Err(SourceError::HashMismatch {
                    what: def.original_file_name.clone(),
                    expected: want,
                    actual,
                });
            }
        }
        Ok(())
    }

    /// One attempt at one part.
    async fn fetch_part(&self, def: &CdnDefinition, p: &CdnPart, job: &Job) -> Result<Vec<u8>> {
        let url = self.join(&format!("parts/{}", p.index));
        check_part_size(p, &url)?;
        let _permit = self.http.permit(&url).await;
        let resp = self
            .http
            .client()
            .get(url.clone())
            .send()
            .await
            .map_err(|e| SourceError::network(&url, e))?;
        let data = read_body(check(resp, &url).await?, &url, job, Some(p.size)).await?;
        let got = Xxh64::of(&data);
        if got != p.hash {
            return Err(SourceError::CorruptPart {
                what: format!("{} part {}", def.original_file_name, p.index),
                msg: format!("xxHash64 {got}, expected {}", p.hash),
            });
        }
        Ok(data)
    }
}

/// Decode (gzip, or plain JSON if the server already decompressed it) and
/// check that the parts tile the file exactly.
fn parse_definition(bytes: &[u8], url: &Url) -> Result<CdnDefinition> {
    let json = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes)
            .take(MAX_DEFINITION)
            .read_to_end(&mut out)
            .map_err(|e| SourceError::protocol(url, format!("bad gzip: {e}")))?;
        out
    } else {
        bytes.to_vec()
    };
    let def: CdnDefinition = serde_json::from_slice(&json)
        .map_err(|e| SourceError::protocol(url, format!("bad definition JSON: {e}")))?;
    let mut next = 0u64;
    for (i, p) in def.parts.iter().enumerate() {
        if p.index != i as u64 || p.offset != next || p.size == 0 {
            return Err(SourceError::protocol(
                url,
                format!(
                    "part {i} (index {}, offset {}, size {}) does not follow offset {next}",
                    p.index, p.offset, p.size
                ),
            ));
        }
        next = next.checked_add(p.size).ok_or_else(|| {
            SourceError::protocol(
                url,
                format!("part {i} (size {}) overflows the file", p.size),
            )
        })?;
    }
    if next != def.size {
        return Err(SourceError::protocol(
            url,
            format!("parts cover {next} bytes, file is {}", def.size),
        ));
    }
    for p in &def.parts {
        check_part_size(p, url)?;
    }
    Ok(def)
}

/// Refuse a part too large to buffer (the definition comes from a
/// modlist-chosen host and can claim anything).
fn check_part_size(p: &CdnPart, url: &Url) -> Result<()> {
    if p.size > MAX_CDN_PART {
        return Err(SourceError::protocol(
            url,
            format!(
                "part {} is {} bytes, larger than the {MAX_CDN_PART}-byte limit",
                p.index, p.size
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaps_legacy_hosts_only() {
        let u = |s: &str| remap_cdn_url(&Url::parse(s).unwrap()).to_string();
        assert_eq!(
            u("https://wabbajack.b-cdn.net/a%20b.7z_1234"),
            "https://authored-files.wabbajack.org/a%20b.7z_1234"
        );
        assert_eq!(
            u("https://Wabbajack-Mirror.b-cdn.net/x"),
            "https://mirror.wabbajack.org/x"
        );
        assert_eq!(
            u("https://authored-files.wabbajack.org/x"),
            "https://authored-files.wabbajack.org/x"
        );
    }

    #[test]
    fn parses_the_real_nemesis_definition() {
        let json = br#"{"Author":"github/Codygits","OriginalFileName":"The Phoenix Flavour - Nemesis Output v4.17.1.7z","Size":2496689,"Hash":"0lc8NBL+6rA=","Parts":[{"Size":2097152,"Offset":0,"Hash":"M9CQHTMhRDI=","Index":0},{"Size":399537,"Offset":2097152,"Hash":"tpxRgNH+nyw=","Index":1}],"ServerAssignedUniqueId":"d412aa87-a256-4e49-9f86-de35f93d7107","MungedName":"The Phoenix Flavour - Nemesis Output v4.17.1.7z_d412aa87-a256-4e49-9f86-de35f93d7107"}"#;
        let url = Url::parse("https://authored-files.wabbajack.org/x").unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, json).unwrap();
        for bytes in [gz.finish().unwrap(), json.to_vec()] {
            let d = parse_definition(&bytes, &url).unwrap();
            assert_eq!(d.size, 2_496_689);
            assert_eq!(d.hash.to_base64(), "0lc8NBL+6rA=");
            assert_eq!(d.parts[1].offset, 2_097_152);
        }
        let gap = String::from_utf8_lossy(json).replace("\"Offset\":2097152", "\"Offset\":2097153");
        assert!(matches!(
            parse_definition(gap.as_bytes(), &url),
            Err(SourceError::Protocol { .. })
        ));
    }

    fn def_json(size: u64, parts: &[(u64, u64)]) -> Vec<u8> {
        let parts: Vec<_> = parts
            .iter()
            .enumerate()
            .map(|(i, (off, sz))| {
                serde_json::json!({"Size": sz, "Offset": off, "Hash": "AAAAAAAAAAA=", "Index": i})
            })
            .collect();
        serde_json::json!({"OriginalFileName": "f.7z", "Size": size, "Hash": "AAAAAAAAAAA=", "Parts": parts})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn a_part_larger_than_the_cap_is_refused() {
        let url = Url::parse("https://authored-files.wabbajack.org/x").unwrap();
        let big = MAX_CDN_PART + 1;
        let e = parse_definition(&def_json(big, &[(0, big)]), &url).unwrap_err();
        assert!(matches!(e, SourceError::Protocol { .. }), "{e}");
        assert!(e.to_string().contains("larger than"), "{e}");
        // At the cap is fine.
        parse_definition(&def_json(MAX_CDN_PART, &[(0, MAX_CDN_PART)]), &url).unwrap();
    }

    #[test]
    fn part_offsets_that_overflow_are_refused() {
        let url = Url::parse("https://authored-files.wabbajack.org/x").unwrap();
        let json = def_json(5, &[(0, u64::MAX), (u64::MAX, 6)]);
        let e = parse_definition(&json, &url).unwrap_err();
        assert!(matches!(e, SourceError::Protocol { .. }), "{e}");
        assert!(e.to_string().contains("overflow"), "{e}");
    }

    #[test]
    fn a_malformed_url_does_not_leak_its_query_in_the_error() {
        let http = Http::new(Default::default(), Default::default()).unwrap();
        let e = CdnFile::new(http, "ht!tp://host/p?sig=SECRET").unwrap_err();
        assert!(!e.to_string().contains("SECRET"), "{e}");
    }
}
