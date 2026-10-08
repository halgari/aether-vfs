//! GOG's content-system v2 documents: the build list, a build's details
//! (its depots) and a depot manifest (its files and their chunks). Build
//! details and depot manifests are zlib-compressed JSON.
//!
//! Ported from NexusMods.App `src/NexusMods.Abstractions.GOG/DTOs` (GPL-3.0).
use std::io::Read;

use serde::Deserialize;

use crate::error::GogError;
use crate::ids::{BuildId, Os, ProductId, string_lenient, u64_lenient};

/// The largest inflated document (build details or depot manifest)
/// accepted. A big game's manifest is tens of MiB of JSON.
pub(crate) const MAX_DOCUMENT_BYTES: u64 = 512 << 20;

/// One build of a product, from `products/{id}/os/{os}/builds?generation=2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Build {
    pub build_id: BuildId,
    pub product_id: ProductId,
    pub os: Os,
    pub version_name: String,
    pub generation: u32,
    /// Where the build's details (zlib JSON) are.
    pub link: String,
    /// As GOG writes it, e.g. `2024-01-02T03:04:05+0000`.
    pub date_published: String,
}

/// A build's details: the depots that make it up, some of them from other
/// products (DLC).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildDetails {
    pub base_product_id: ProductId,
    pub install_directory: String,
    pub depots: Vec<DepotRef>,
}

/// One depot of a build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepotRef {
    /// The product the depot belongs to (the game or a DLC); its secure
    /// link serves the depot's chunks.
    pub product_id: ProductId,
    /// The manifest id (hex): `content-system/v2/meta/{m[0..2]}/{m[2..4]}/{m}`.
    pub manifest: String,
    /// Uncompressed size of the depot's files.
    pub size: u64,
    /// Language codes, `*` for all.
    pub languages: Vec<String>,
}

/// A depot's files. Directories and links in the manifest are dropped;
/// directories are implied by the file paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepotManifest {
    /// Files, with their paths spelled as in the manifest (GOG uses `\`).
    pub items: Vec<DepotItem>,
    /// The chunks of the depot's small-files container, if it has one:
    /// small files are byte ranges of it ([`DepotItem::sfc_ref`]).
    pub small_files_container: Option<Vec<Chunk>>,
}

/// One file of a depot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepotItem {
    pub path: String,
    /// The file's own chunks, in order. Ignored when the file is read from
    /// the small-files container.
    pub chunks: Vec<Chunk>,
    /// The file's length.
    pub size: u64,
    /// MD5 of the whole file, when the manifest gives one.
    pub md5: Option<[u8; 16]>,
    /// Where the file lives in the depot's small-files container. Only set
    /// when the manifest has a container.
    pub sfc_ref: Option<SfcRef>,
    /// Manifest flags, e.g. `executable`, `support`.
    pub flags: Vec<String>,
}

/// A byte range of the small-files container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SfcRef {
    pub offset: u64,
    pub size: u64,
}

/// One chunk: a zlib stream on the CDN named by its compressed MD5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chunk {
    pub compressed_md5: [u8; 16],
    /// MD5 of the inflated bytes.
    pub md5: [u8; 16],
    /// Inflated length.
    pub size: u64,
    pub compressed_size: u64,
}

impl DepotManifest {
    /// The file at `path`, in any case and with `/` or `\`.
    pub fn find(&self, path: &str) -> Option<&DepotItem> {
        let want = fold(path);
        self.items.iter().find(|i| fold(&i.path) == want)
    }

    /// Parse a depot manifest (zlib JSON, or plain JSON).
    pub fn parse(body: &[u8]) -> Result<DepotManifest, GogError> {
        let json = inflate_document(body, "GOG depot manifest")?;
        let raw: RawDepotDoc =
            serde_json::from_slice(&json).map_err(|e| GogError::json("GOG depot manifest", e))?;
        let sfc = match raw.depot.small_files_container {
            Some(c) => Some(convert_chunks(c.chunks)?),
            None => None,
        };
        let sfc_len: u64 = sfc.iter().flatten().map(|c| c.size).sum();
        let mut items = Vec::new();
        for item in raw.depot.items {
            if item.kind != "DepotFile" {
                continue;
            }
            let chunks = convert_chunks(item.chunks)?;
            let md5 = item.md5.as_deref().map(parse_md5).transpose()?;
            let sfc_ref = item.sfc_ref.map(SfcRef::from).filter(|_| sfc.is_some());
            if let Some(r) = sfc_ref
                && r.offset.checked_add(r.size).is_none_or(|end| end > sfc_len)
            {
                return Err(GogError::json(
                    "GOG depot manifest",
                    format!(
                        "{}: small-files range {}+{} is outside the {sfc_len}-byte container",
                        item.path, r.offset, r.size
                    ),
                ));
            }
            let size = match sfc_ref {
                Some(r) => r.size,
                None => chunks.iter().map(|c| c.size).sum(),
            };
            items.push(DepotItem {
                path: item.path,
                chunks,
                size,
                md5,
                sfc_ref,
                flags: item.flags,
            });
        }
        Ok(DepotManifest {
            items,
            small_files_container: sfc,
        })
    }
}

impl BuildDetails {
    /// Parse a build's details (zlib JSON, or plain JSON).
    pub fn parse(body: &[u8]) -> Result<BuildDetails, GogError> {
        let json = inflate_document(body, "GOG build details")?;
        let raw: RawBuildDetails =
            serde_json::from_slice(&json).map_err(|e| GogError::json("GOG build details", e))?;
        Ok(BuildDetails {
            base_product_id: raw.base_product_id,
            install_directory: raw.install_directory.unwrap_or_default(),
            depots: raw
                .depots
                .into_iter()
                .map(|d| DepotRef {
                    product_id: d.product_id,
                    manifest: d.manifest,
                    size: d.size,
                    languages: d.languages,
                })
                .collect(),
        })
    }
}

/// The `items` of a build list.
pub(crate) fn parse_builds(body: &[u8]) -> Result<Vec<Build>, GogError> {
    let raw: RawBuildList =
        serde_json::from_slice(body).map_err(|e| GogError::json("GOG build list", e))?;
    Ok(raw
        .items
        .into_iter()
        .map(|b| Build {
            build_id: b.build_id,
            product_id: b.product_id,
            os: b.os,
            version_name: b.version_name.unwrap_or_default(),
            generation: b.generation,
            link: b.link.unwrap_or_default(),
            date_published: b.date_published.unwrap_or_default(),
        })
        .collect())
}

/// Lookup key for a depot path: `\` as `/`, lowercase, no leading `/`.
pub(crate) fn fold(path: &str) -> String {
    aether_archive::path::fold(path)
        .trim_start_matches('/')
        .to_string()
}

/// A manifest id safe to put in a URL path and a file name.
pub(crate) fn valid_manifest_id(id: &str) -> bool {
    (4..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_md5(s: &str) -> Result<[u8; 16], GogError> {
    let bad = || GogError::json("GOG depot manifest", format!("not an MD5: {s:?}"));
    if s.len() != 32 || !s.is_ascii() {
        return Err(bad());
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
    }
    Ok(out)
}

fn convert_chunks(raw: Vec<RawChunk>) -> Result<Vec<Chunk>, GogError> {
    raw.into_iter()
        .map(|c| {
            Ok(Chunk {
                compressed_md5: parse_md5(&c.compressed_md5)?,
                md5: parse_md5(&c.md5)?,
                size: c.size,
                compressed_size: c.compressed_size,
            })
        })
        .collect()
}

/// Inflate a zlib document, at most [`MAX_DOCUMENT_BYTES`]. A body that is
/// not zlib but looks like JSON is taken as is (as heroic-gogdl does).
pub(crate) fn inflate_document(body: &[u8], what: &str) -> Result<Vec<u8>, GogError> {
    match inflate(body, MAX_DOCUMENT_BYTES) {
        Ok(v) => Ok(v),
        Err(_) if body.trim_ascii_start().first() == Some(&b'{') => Ok(body.to_vec()),
        Err(e) => Err(GogError::json(what, e)),
    }
}

/// Inflate a zlib stream of at most `max` bytes.
pub(crate) fn inflate(body: &[u8], max: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(body)
        .take(max + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("bad zlib data: {e}"))?;
    if out.len() as u64 > max {
        return Err(format!("inflates to more than {max} bytes"));
    }
    Ok(out)
}

#[derive(Deserialize)]
struct RawBuildList {
    items: Vec<RawBuild>,
}

#[derive(Deserialize)]
struct RawBuild {
    build_id: BuildId,
    product_id: ProductId,
    os: Os,
    #[serde(default)]
    version_name: Option<String>,
    #[serde(default)]
    generation: u32,
    #[serde(default)]
    link: Option<String>,
    #[serde(default)]
    date_published: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBuildDetails {
    base_product_id: ProductId,
    #[serde(default)]
    install_directory: Option<String>,
    depots: Vec<RawDepotRef>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDepotRef {
    product_id: ProductId,
    #[serde(deserialize_with = "string_lenient")]
    manifest: String,
    #[serde(default, deserialize_with = "u64_lenient")]
    size: u64,
    #[serde(default)]
    languages: Vec<String>,
}

#[derive(Deserialize)]
struct RawDepotDoc {
    depot: RawDepot,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDepot {
    items: Vec<RawItem>,
    #[serde(default)]
    small_files_container: Option<RawSfc>,
}

#[derive(Deserialize)]
struct RawSfc {
    chunks: Vec<RawChunk>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawItem {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    #[serde(default)]
    chunks: Vec<RawChunk>,
    #[serde(default)]
    md5: Option<String>,
    #[serde(default)]
    sfc_ref: Option<RawSfcRef>,
    #[serde(default)]
    flags: Vec<String>,
}

#[derive(Deserialize, Clone, Copy)]
struct RawSfcRef {
    #[serde(deserialize_with = "u64_lenient")]
    offset: u64,
    #[serde(deserialize_with = "u64_lenient")]
    size: u64,
}

impl From<RawSfcRef> for SfcRef {
    fn from(r: RawSfcRef) -> SfcRef {
        SfcRef {
            offset: r.offset,
            size: r.size,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawChunk {
    md5: String,
    #[serde(deserialize_with = "u64_lenient")]
    size: u64,
    compressed_md5: String,
    #[serde(deserialize_with = "u64_lenient")]
    compressed_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zlib(b: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(b).unwrap();
        e.finish().unwrap()
    }

    const C: &str = r#"{"md5":"00112233445566778899aabbccddeeff","size":"10",
        "compressedMd5":"ffeeddccbbaa99887766554433221100","compressedSize":12}"#;

    #[test]
    fn manifest_keeps_files_and_checks_small_file_ranges() {
        let doc = format!(
            r#"{{"version":2,"depot":{{"items":[
                {{"type":"DepotDirectory","path":"a"}},
                {{"type":"DepotFile","path":"a\\b.txt","chunks":[{C},{C}]}},
                {{"type":"DepotFile","path":"s.ini","chunks":[],"sfcRef":{{"offset":2,"size":8}}}}],
              "smallFilesContainer":{{"chunks":[{C}]}}}}}}"#
        );
        let m = DepotManifest::parse(&zlib(doc.as_bytes())).unwrap();
        assert_eq!(m.items.len(), 2);
        assert_eq!(m.items[0].size, 20);
        assert_eq!(m.items[0].chunks[0].md5[0], 0x00);
        assert_eq!(m.items[0].chunks[0].compressed_md5[0], 0xff);
        assert_eq!(m.find("/A/B.TXT").unwrap().path, "a\\b.txt");
        assert_eq!(m.find("s.ini").unwrap().size, 8);
        // Plain JSON is accepted too.
        assert_eq!(DepotManifest::parse(doc.as_bytes()).unwrap(), m);

        let out_of_range = doc.replace(r#""size":8"#, r#""size":9"#);
        assert!(DepotManifest::parse(out_of_range.as_bytes()).is_err());
        assert!(DepotManifest::parse(b"\x78\x9cgarbage").is_err());
    }

    #[test]
    fn a_small_file_ref_without_a_container_reads_the_files_own_chunks() {
        let doc = format!(
            r#"{{"depot":{{"items":[{{"type":"DepotFile","path":"s","chunks":[{C}],
                "sfcRef":{{"offset":0,"size":3}}}}]}}}}"#
        );
        let m = DepotManifest::parse(doc.as_bytes()).unwrap();
        assert_eq!((m.items[0].sfc_ref, m.items[0].size), (None, 10));
    }

    #[test]
    fn inflate_is_capped() {
        let big = zlib(&vec![0u8; 10_000]);
        assert_eq!(inflate(&big, 10_000).unwrap().len(), 10_000);
        assert!(inflate(&big, 9_999).is_err());
    }

    #[test]
    fn manifest_ids_are_hex() {
        assert!(valid_manifest_id("aa11ff"));
        for bad in ["", "abc", "../../etc", "aa/11", "zzzz"] {
            assert!(!valid_manifest_id(bad), "{bad}");
        }
    }
}
