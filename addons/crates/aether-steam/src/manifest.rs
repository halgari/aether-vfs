//! Haskill's validated view of a Steam depot manifest.
use crate::chunk::MAX_CHUNK_LEN;
use crate::error::SteamError;
use crate::ids::{ChunkId, DepotId, DepotKey, ManifestId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Steam's `EDepotFileFlag::Directory`.
const FLAG_DIRECTORY: u32 = 0x40;

/// One chunk of a file: `len` decompressed bytes at `offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    pub id: ChunkId,
    pub offset: u64,
    pub len: u32,
    /// Steam's zero-seeded Adler-32 of the decompressed bytes.
    pub adler: u32,
}

impl ChunkRef {
    pub fn end(&self) -> u64 {
        self.offset + u64::from(self.len)
    }
}

/// A regular file in a depot. `chunks` are sorted and tile `0..size` exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// The path as Steam stores it (backslash separators).
    pub path: String,
    pub size: u64,
    /// SHA-1 of the whole file, when the manifest records one.
    pub sha1: Option<[u8; 20]>,
    pub chunks: Vec<ChunkRef>,
}

impl FileEntry {
    /// The chunks overlapping `off..off + len`, clamped to the file. Empty
    /// when the range is empty or starts at or past the end.
    pub fn chunks_for(&self, off: u64, len: u64) -> &[ChunkRef] {
        let end = off.saturating_add(len).min(self.size);
        if off >= end {
            return &[];
        }
        let first = self.chunks.partition_point(|c| c.end() <= off);
        let last = self.chunks.partition_point(|c| c.offset < end);
        &self.chunks[first..last]
    }
}

/// Fold a path for case-insensitive lookup: `\` becomes `/`, ASCII and
/// Unicode letters are lowercased, and leading separators are dropped.
pub fn fold_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches('/')
        .to_lowercase()
}

/// A validated depot manifest with a case-insensitive path index.
#[derive(Clone, Debug)]
pub struct DepotManifest {
    depot: DepotId,
    id: ManifestId,
    files: Vec<FileEntry>,
    index: HashMap<String, usize>,
}

impl PartialEq for DepotManifest {
    fn eq(&self, o: &Self) -> bool {
        self.depot == o.depot && self.id == o.id && self.files == o.files
    }
}

impl DepotManifest {
    /// Validate `files` and build the path index. Rejects a manifest with a
    /// zero chunk id, a zero-length chunk, chunks that do not tile the file
    /// exactly, or two files whose paths fold to the same key.
    pub fn new(depot: DepotId, id: ManifestId, files: Vec<FileEntry>) -> Result<Self, SteamError> {
        let bad = |path: &str, msg: String| {
            SteamError::Integrity(format!("manifest {id} of depot {depot}: {path}: {msg}"))
        };
        let mut index = HashMap::with_capacity(files.len());
        for (i, f) in files.iter().enumerate() {
            let mut expect = 0u64;
            for c in &f.chunks {
                if c.id.0 == [0; 20] {
                    return Err(bad(&f.path, "chunk without an id".into()));
                }
                if c.len == 0 {
                    return Err(bad(&f.path, "zero-length chunk".into()));
                }
                if c.len > MAX_CHUNK_LEN {
                    return Err(bad(
                        &f.path,
                        format!(
                            "chunk of {} bytes exceeds the {MAX_CHUNK_LEN}-byte cap",
                            c.len
                        ),
                    ));
                }
                if c.offset != expect {
                    return Err(bad(
                        &f.path,
                        format!("chunk at {} but expected one at {expect}", c.offset),
                    ));
                }
                expect = c.end();
            }
            if expect != f.size {
                return Err(bad(
                    &f.path,
                    format!("chunks cover {expect} bytes of {}", f.size),
                ));
            }
            if index.insert(fold_path(&f.path), i).is_some() {
                return Err(bad(&f.path, "duplicate path".into()));
            }
        }
        Ok(DepotManifest {
            depot,
            id,
            files,
            index,
        })
    }

    pub fn depot(&self) -> DepotId {
        self.depot
    }

    pub fn id(&self) -> ManifestId {
        self.id
    }

    pub fn files(&self) -> &[FileEntry] {
        &self.files
    }

    /// Look up a file by path: `/` or `\` separators, any case.
    pub fn file(&self, path: &str) -> Option<&FileEntry> {
        self.index.get(&fold_path(path)).map(|&i| &self.files[i])
    }

    /// Index into [`files`](Self::files) of `path`, looked up like [`file`](Self::file).
    pub fn file_index(&self, path: &str) -> Option<usize> {
        self.index.get(&fold_path(path)).copied()
    }

    /// Parse the body Steam's CDN returns for a manifest (a zip holding the
    /// binary manifest, or the bare binary), decrypt file names with `key`,
    /// keep regular files only, and validate. `depot`/`id` must match the
    /// manifest's own metadata.
    pub fn from_cdn_bytes(
        body: &[u8],
        depot: DepotId,
        id: ManifestId,
        key: &DepotKey,
    ) -> Result<Self, SteamError> {
        let raw = unzip_single(body)?;
        let mut m = steamroom::depot::manifest::DepotManifest::parse(&raw)
            .map_err(|e| SteamError::Integrity(format!("manifest {id}: {e}")))?;
        if m.depot_id.map(|d| d.0) != Some(depot.0) || m.manifest_id.map(|g| g.0) != Some(id.0) {
            return Err(SteamError::Integrity(format!(
                "asked for manifest {id} of depot {depot}, got {:?} of {:?}",
                m.manifest_id.map(|g| g.0),
                m.depot_id.map(|d| d.0)
            )));
        }
        m.decrypt_filenames(&steamroom::depot::DepotKey(key.0))
            .map_err(|e| SteamError::Integrity(format!("manifest {id}: file names: {e}")))?;
        let files = m
            .files
            .into_iter()
            .filter(|f| f.flags & FLAG_DIRECTORY == 0 && f.link_target.is_none())
            .map(|f| FileEntry {
                path: f.filename,
                size: f.size,
                sha1: f.sha_content,
                chunks: f
                    .chunks
                    .into_iter()
                    .map(|c| ChunkRef {
                        id: ChunkId(c.id.0),
                        // A missing offset is only legal for a lone first
                        // chunk; validation catches anything else.
                        offset: c.offset.unwrap_or(0),
                        len: c.uncompressed_size,
                        adler: c.checksum,
                    })
                    .collect(),
            })
            .collect();
        DepotManifest::new(depot, id, files)
    }

    /// Encode for the on-disk cache: magic, format version, postcard body,
    /// then the body's xxHash64.
    pub fn encode(&self) -> Vec<u8> {
        let body = postcard::to_allocvec(&(self.depot, self.id, &self.files))
            .expect("postcard encoding of plain data cannot fail");
        let mut out = Vec::with_capacity(body.len() + 20);
        out.extend_from_slice(CACHE_MAGIC);
        out.extend_from_slice(&CACHE_VERSION.to_le_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&xxhash_rust::xxh64::xxh64(&body, 0).to_le_bytes());
        out
    }

    /// Decode a cache file written by [`encode`](Self::encode); `None` if it
    /// is damaged or from another format version.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(CACHE_MAGIC)?;
        let (ver, rest) = rest.split_first_chunk::<4>()?;
        if u32::from_le_bytes(*ver) != CACHE_VERSION {
            return None;
        }
        let (body, sum) = rest.split_last_chunk::<8>()?;
        if xxhash_rust::xxh64::xxh64(body, 0) != u64::from_le_bytes(*sum) {
            return None;
        }
        let (depot, id, files): (DepotId, ManifestId, Vec<FileEntry>) =
            postcard::from_bytes(body).ok()?;
        DepotManifest::new(depot, id, files).ok()
    }
}

const CACHE_MAGIC: &[u8; 4] = b"HSKM";
const CACHE_VERSION: u32 = 1;

/// The largest manifest body this crate will decompress from a CDN zip. A
/// real depot manifest is a few MiB at most; this is generous headroom
/// against a corrupt or hostile high-ratio deflate entry trying to exhaust
/// memory during decompression.
const MAX_MANIFEST_BYTES: u64 = 256 << 20;

fn unzip_single(body: &[u8]) -> Result<Vec<u8>, SteamError> {
    unzip_single_capped(body, MAX_MANIFEST_BYTES)
}

fn unzip_single_capped(body: &[u8], max: u64) -> Result<Vec<u8>, SteamError> {
    if !body.starts_with(b"PK\x03\x04") {
        return Ok(body.to_vec());
    }
    let bad = |e: String| SteamError::Integrity(format!("manifest zip: {e}"));
    let mut zip =
        zip::ZipArchive::new(std::io::Cursor::new(body)).map_err(|e| bad(e.to_string()))?;
    if zip.len() != 1 {
        return Err(bad(format!("{} entries, expected 1", zip.len())));
    }
    let entry = zip.by_index(0).map_err(|e| bad(e.to_string()))?;
    let declared = entry.size();
    let mut out = Vec::with_capacity(declared.min(max) as usize);
    // Cap the bytes actually read, regardless of what the entry claims: a
    // hostile high-ratio deflate stream must not be allowed to decompress
    // without bound.
    let mut limited = std::io::Read::take(entry, max + 1);
    std::io::Read::read_to_end(&mut limited, &mut out).map_err(|e| bad(e.to_string()))?;
    if out.len() as u64 > max {
        return Err(bad(format!("entry decompresses beyond the {max}-byte cap")));
    }
    if out.len() as u64 != declared {
        return Err(bad(format!(
            "entry declared {declared} bytes but decompressed to {}",
            out.len()
        )));
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::{FixtureFile, KEY, manifest_body, manifest_body_no_metadata};

    pub(crate) fn chunk(n: u8, offset: u64, len: u32) -> ChunkRef {
        ChunkRef {
            id: ChunkId([n; 20]),
            offset,
            len,
            adler: 0,
        }
    }

    pub(crate) fn file(path: &str, lens: &[u32]) -> FileEntry {
        let mut off = 0;
        let chunks = lens
            .iter()
            .enumerate()
            .map(|(i, &l)| {
                let c = chunk(i as u8 + 1, off, l);
                off += u64::from(l);
                c
            })
            .collect();
        FileEntry {
            path: path.into(),
            size: off,
            sha1: None,
            chunks,
        }
    }

    #[test]
    fn chunks_for_maps_ranges_to_covering_chunks() {
        let f = file("a", &[10, 10, 10]); // 0..10, 10..20, 20..30
        let ids = |off, len| {
            f.chunks_for(off, len)
                .iter()
                .map(|c| c.offset)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(0, 1), [0]);
        assert_eq!(ids(9, 1), [0]);
        assert_eq!(ids(9, 2), [0, 10]);
        assert_eq!(ids(10, 10), [10]);
        assert_eq!(ids(5, 25), [0, 10, 20]);
        assert_eq!(ids(29, 100), [20]); // clamped to the file
        assert!(ids(30, 1).is_empty()); // at EOF
        assert!(ids(3, 0).is_empty()); // empty range
        assert!(ids(u64::MAX, u64::MAX).is_empty()); // no overflow
    }

    #[test]
    fn chunks_for_handles_offsets_past_4_gib() {
        // `FileEntry::chunks_for` is a plain range query with no size cap of
        // its own (that cap is a `DepotManifest::new` construction-time
        // policy); exercised directly here so 1 GiB round numbers can stand
        // in for whatever real chunk size, without tripping it.
        let gib = 1u64 << 30;
        let mut f = file("Data\\Skyrim - Textures0.bsa", &[]);
        for i in 0..6u64 {
            f.chunks.push(chunk(i as u8 + 1, i * gib, gib as u32));
        }
        f.size = 6 * gib;
        let hit = f.chunks_for(5 * gib - 1, 2);
        assert_eq!(
            hit.iter().map(|c| c.offset).collect::<Vec<_>>(),
            [4 * gib, 5 * gib]
        );
    }

    #[test]
    fn lookup_folds_non_ascii_names() {
        let m = DepotManifest::new(
            DepotId(1),
            ManifestId(2),
            vec![file("Data\\Interface\\Translate_ÉSPAÑOL.txt", &[4])],
        )
        .unwrap();
        assert!(m.file("data/interface/translate_éspañol.TXT").is_some());
    }

    #[test]
    fn lookup_is_case_and_separator_insensitive() {
        let m = DepotManifest::new(
            DepotId(1),
            ManifestId(2),
            vec![
                file("Data\\Skyrim - Misc.bsa", &[4]),
                file("steam_api64.dll", &[4]),
            ],
        )
        .unwrap();
        assert!(m.file("data/skyrim - misc.BSA").is_some());
        assert!(m.file("DATA\\SKYRIM - MISC.BSA").is_some());
        assert!(m.file("/steam_api64.dll").is_some());
        assert!(m.file("Data\\Skyrim.esm").is_none());
    }

    #[test]
    fn rejects_gaps_overlaps_short_cover_zero_ids_and_duplicates() {
        let mk = |f: FileEntry| DepotManifest::new(DepotId(1), ManifestId(2), vec![f]);
        let mut gap = file("a", &[10, 10]);
        gap.chunks[1].offset = 11;
        assert!(mk(gap).is_err());
        let mut short = file("a", &[10, 10]);
        short.size = 25;
        assert!(mk(short).is_err());
        let mut zero = file("a", &[10]);
        zero.chunks[0].id = ChunkId([0; 20]);
        assert!(mk(zero).is_err());
        let mut empty_chunk = file("a", &[10]);
        empty_chunk.chunks[0].len = 0;
        assert!(mk(empty_chunk).is_err());
        let dup = DepotManifest::new(
            DepotId(1),
            ManifestId(2),
            vec![file("A", &[1]), file("a", &[1])],
        );
        assert!(dup.is_err());
        assert!(mk(file("empty", &[])).is_ok()); // a 0-byte file has no chunks
    }

    #[test]
    fn rejects_a_chunk_over_the_size_cap() {
        let big = file("a", &[MAX_CHUNK_LEN + 1]);
        let err = DepotManifest::new(DepotId(1), ManifestId(2), vec![big]).unwrap_err();
        assert!(matches!(err, SteamError::Integrity(_)));
    }

    #[test]
    fn cache_encoding_round_trips_and_detects_damage() {
        let m =
            DepotManifest::new(DepotId(7), ManifestId(9), vec![file("x\\y.txt", &[3, 5])]).unwrap();
        let bytes = m.encode();
        let back = DepotManifest::decode(&bytes).unwrap();
        assert_eq!(back, m);
        assert!(back.file("X/Y.TXT").is_some());
        let mut bad = bytes.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 1;
        assert!(DepotManifest::decode(&bad).is_none());
        assert!(DepotManifest::decode(&bytes[..bytes.len() - 1]).is_none());
        assert!(DepotManifest::decode(b"").is_none());
    }

    #[test]
    fn parses_a_cdn_manifest_body() {
        let a: Vec<u8> = (0..2500u32).map(|i| i as u8).collect();
        let body = manifest_body(
            DepotId(481),
            ManifestId(99),
            &[
                FixtureFile {
                    path: "Data\\a.bin",
                    data: &a,
                    chunk: 1000,
                },
                FixtureFile {
                    path: "empty.txt",
                    data: b"",
                    chunk: 1000,
                },
            ],
            &["Data"],
            None,
        );
        let m = DepotManifest::from_cdn_bytes(&body, DepotId(481), ManifestId(99), &KEY).unwrap();
        assert_eq!(m.files().len(), 2, "directory entry skipped");
        let f = m.file("data/A.BIN").unwrap();
        assert_eq!(f.size, 2500);
        assert_eq!(
            f.chunks
                .iter()
                .map(|c| (c.offset, c.len))
                .collect::<Vec<_>>(),
            [(0, 1000), (1000, 1000), (2000, 500)],
            "chunks sorted by offset"
        );
        assert_eq!(f.sha1, Some(sha1_smol::Sha1::from(&a).digest().bytes()));
    }

    #[test]
    fn decrypts_encrypted_file_names() {
        let body = manifest_body(
            DepotId(5),
            ManifestId(6),
            &[FixtureFile {
                path: "Data\\Skyrim.esm",
                data: b"TES4",
                chunk: 1024,
            }],
            &[],
            Some(&KEY),
        );
        let m = DepotManifest::from_cdn_bytes(&body, DepotId(5), ManifestId(6), &KEY).unwrap();
        assert_eq!(m.files()[0].path, "Data\\Skyrim.esm");
        assert!(
            DepotManifest::from_cdn_bytes(&body, DepotId(5), ManifestId(6), &DepotKey([1; 32]))
                .is_err()
        );
    }

    #[test]
    fn rejects_a_manifest_for_another_depot_or_id() {
        let body = manifest_body(DepotId(1), ManifestId(2), &[], &[], None);
        assert!(DepotManifest::from_cdn_bytes(&body, DepotId(1), ManifestId(3), &KEY).is_err());
        assert!(DepotManifest::from_cdn_bytes(&body, DepotId(9), ManifestId(2), &KEY).is_err());
        assert!(
            DepotManifest::from_cdn_bytes(b"PK\x03\x04junk", DepotId(1), ManifestId(2), &KEY)
                .is_err()
        );
    }

    #[test]
    fn rejects_a_manifest_whose_metadata_section_is_missing() {
        // steamroom's parser falls back to default (all-`None`) metadata
        // when the metadata section is absent or fails to decode, which
        // must never be treated as a match for whatever depot/id we asked
        // for.
        let body = manifest_body_no_metadata(&[FixtureFile {
            path: "a",
            data: b"x",
            chunk: 1024,
        }]);
        assert!(DepotManifest::from_cdn_bytes(&body, DepotId(1), ManifestId(2), &KEY).is_err());
    }

    #[test]
    fn unzip_single_rejects_entries_beyond_the_cap() {
        let data = vec![0u8; 1024];
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("z", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zip, &data).unwrap();
        let body = zip.finish().unwrap().into_inner();
        assert!(unzip_single_capped(&body, 100).is_err());
        assert!(unzip_single_capped(&body, data.len() as u64).is_ok());
    }

    #[test]
    fn file_index_matches_file_lookup() {
        let m =
            DepotManifest::new(DepotId(1), ManifestId(2), vec![file("Data\\a.txt", &[4])]).unwrap();
        assert_eq!(m.file_index("data/A.TXT"), Some(0));
        assert!(m.file_index("missing").is_none());
    }
}
