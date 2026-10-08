//! A game's depots as one read-only [`vfs_provider::Provider`].
//!
//! The tree is built once, from the manifests, when the provider is made:
//! every file of every depot, and every directory a file path implies (Steam
//! manifests as loaded here hold regular files only). Names match
//! case-insensitively (`vfs_core::fold`, `\` or `/`); listings show the
//! manifest's spelling. When two depots hold the same path, the earlier
//! depot in [`SteamGame`]'s order wins, and a directory both hold is listed
//! once, under the earlier depot's spelling, holding both depots' files.
//!
//! # Threading
//!
//! Provider methods are called from the director's threads, which are plain
//! OS threads, not tokio workers. Everything but `read_at` answers from the
//! in-memory tree. `read_at` fetches chunks from the CDN by blocking the
//! calling thread on the tokio runtime handle given to [`DepotProvider::new`]
//! (through [`BlockingDepotFile`]), so:
//!
//! - that handle must belong to a **multi-thread** runtime that outlives the
//!   provider (a `current_thread` runtime's IO is driven only by its own
//!   `block_on`, so a read from another thread would hang);
//! - `read_at` must not be called on a tokio worker thread (or inside any
//!   `block_on`). There it does not block the runtime: it fails with
//!   `ST_IO_ERROR`. Call it from a plain thread or `spawn_blocking`. This
//!   relies on catching tokio's "runtime within a runtime" panic, so it
//!   needs `panic = "unwind"`.

use crate::game::SteamGame;
use crate::reader::{BlockingDepotFile, SteamDepotFile};
use aether_archive::RangeRead;
use std::collections::HashMap;
use tokio::runtime::Handle as RtHandle;
use vfs_provider::{
    Access, Capabilities, CaseMatch, DirEntry, Handle, HandleTable, KIND_DIR, KIND_FILE,
    OPEN_APPEND, OPEN_CREATE, OPEN_TRUNC, OPEN_WRITE, Provider, ST_BAD_FH, Stat, VPath, io_error,
    not_a_dir, not_found, read_only,
};

/// Every file's and directory's mtime (2020-01-01). Manifests carry no
/// times; a constant keeps "newer than" checks stable across runs.
pub const MTIME: i64 = 1_577_836_800;

/// Steam's chunk size: the natural unit for a cache in front of this.
const STEAM_CHUNK: u32 = 1 << 20;

enum Kind {
    /// Children, in the order first seen.
    Dir(Vec<u32>),
    File(SteamDepotFile),
}

struct Node {
    /// The last path component as the (first) manifest spells it.
    name: String,
    kind: Kind,
}

impl Node {
    fn stat(&self) -> Stat {
        match &self.kind {
            Kind::Dir(_) => Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: MTIME,
            },
            Kind::File(f) => Stat {
                kind: KIND_FILE,
                size: f.len(),
                mtime: MTIME,
            },
        }
    }
}

/// Depot manifests as one read-only tree. Earlier depots win on collisions.
pub struct DepotProvider {
    /// Node 0 is the root directory.
    nodes: Vec<Node>,
    /// Folded full path (components joined by `/`) to node.
    by_path: HashMap<String, u32>,
    handles: HandleTable<BlockingDepotFile>,
    rt: RtHandle,
}

/// `rel`'s components, folded and joined by `/`; `""` for the root.
fn fold_rel(rel: &str) -> String {
    let mut key = String::with_capacity(rel.len());
    for c in rel.split(['/', '\\']).filter(|c| !c.is_empty()) {
        push_folded(&mut key, c);
    }
    key
}

fn push_folded(key: &mut String, component: &str) {
    if !key.is_empty() {
        key.push('/');
    }
    key.push_str(&vfs_core::fold(component));
}

impl DepotProvider {
    /// Index every depot of `game`. `rt` runs the CDN reads; see the module
    /// docs for what it must be and which threads may read.
    pub fn new(game: SteamGame, rt: RtHandle) -> Self {
        let mut nodes = vec![Node {
            name: String::new(),
            kind: Kind::Dir(Vec::new()),
        }];
        let mut by_path = HashMap::new();
        by_path.insert(String::new(), 0u32);
        for (manifest, reader) in game.depots() {
            'files: for entry in manifest.files() {
                let comps: Vec<&str> = entry
                    .path
                    .split(['\\', '/'])
                    .filter(|c| !c.is_empty())
                    .collect();
                let Some((&leaf, dirs)) = comps.split_last() else {
                    continue;
                };
                let mut key = String::new();
                let mut parent = 0u32;
                for &dir in dirs {
                    push_folded(&mut key, dir);
                    parent = match by_path.get(&key) {
                        Some(&n) if matches!(nodes[n as usize].kind, Kind::Dir(_)) => n,
                        Some(_) => {
                            tracing::warn!(
                                depot = %manifest.depot(),
                                path = %entry.path,
                                "a depot path runs through a file of an earlier entry; skipped"
                            );
                            continue 'files;
                        }
                        None => add(
                            &mut nodes,
                            &mut by_path,
                            parent,
                            &key,
                            dir,
                            Kind::Dir(Vec::new()),
                        ),
                    };
                }
                push_folded(&mut key, leaf);
                if by_path.contains_key(&key) {
                    tracing::debug!(
                        depot = %manifest.depot(),
                        path = %entry.path,
                        "path already served by an earlier depot"
                    );
                    continue;
                }
                match reader.open(manifest.clone(), &entry.path) {
                    Ok(f) => {
                        add(&mut nodes, &mut by_path, parent, &key, leaf, Kind::File(f));
                    }
                    Err(e) => tracing::warn!(
                        depot = %manifest.depot(),
                        path = %entry.path,
                        "cannot open a manifest file: {e}"
                    ),
                }
            }
        }
        DepotProvider {
            nodes,
            by_path,
            handles: HandleTable::new(),
            rt,
        }
    }

    fn node(&self, p: VPath) -> Option<&Node> {
        self.by_path
            .get(&fold_rel(p.rel))
            .map(|&n| &self.nodes[n as usize])
    }
}

/// Append a node named `name` at folded path `key` under directory `parent`.
fn add(
    nodes: &mut Vec<Node>,
    by_path: &mut HashMap<String, u32>,
    parent: u32,
    key: &str,
    name: &str,
    kind: Kind,
) -> u32 {
    let n = nodes.len() as u32;
    nodes.push(Node {
        name: name.to_string(),
        kind,
    });
    by_path.insert(key.to_string(), n);
    if let Kind::Dir(kids) = &mut nodes[parent as usize].kind {
        kids.push(n);
    }
    n
}

impl Provider for DepotProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::Read,
            // A manifest pins its content.
            immutable: true,
            // Reads go to the CDN.
            slow: true,
            preferred_block: Some(STEAM_CHUNK),
            case: CaseMatch::Insensitive,
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        Ok(self.node(p).map(Node::stat))
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        match &self.node(p).ok_or_else(not_found)?.kind {
            Kind::Dir(kids) => Ok(kids
                .iter()
                .map(|&k| {
                    let n = &self.nodes[k as usize];
                    DirEntry {
                        name: n.name.clone(),
                        stat: n.stat(),
                    }
                })
                .collect()),
            Kind::File(_) => Err(not_a_dir()),
        }
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        if flags & (OPEN_WRITE | OPEN_CREATE | OPEN_TRUNC | OPEN_APPEND) != 0 {
            return Err(read_only());
        }
        match &self.node(p).ok_or_else(not_found)?.kind {
            Kind::Dir(_) => Ok((self.handles.fresh(), 0, true)),
            Kind::File(f) => {
                let len = f.len();
                let h = self
                    .handles
                    .insert(f.clone().into_blocking(self.rt.clone()))?;
                Ok((h, len, false))
            }
        }
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        // A directory's handle was never stored; closing it is fine.
        match self.handles.remove(h) {
            Ok(_) | Err(ST_BAD_FH) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Blocks the calling thread on the CDN; see the module docs.
    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        // A clone (two `Arc`s and a runtime handle), so no lock is held
        // across the fetch.
        let file = self.handles.get(h)?;
        let len = file.len();
        if offset >= len || buf.is_empty() {
            return Ok(0);
        }
        let n = (len - offset).min(buf.len() as u64) as usize;
        file.read_at(offset, &mut buf[..n]).map_err(|e| {
            tracing::warn!(
                depot = %file.file().depot(),
                path = %file.file().entry().path,
                offset,
                len = n,
                "depot read failed: {e}"
            );
            io_error()
        })?;
        Ok(n)
    }

    fn stored_name(&self, p: VPath) -> Result<Option<String>, i32> {
        if fold_rel(p.rel).is_empty() {
            return Ok(None);
        }
        Ok(self.node(p).map(|n| n.name.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdn::CdnConfig;
    use crate::ids::{AppId, DepotId, ManifestId};
    use crate::manifest::DepotManifest;
    use crate::reader::DepotReader;
    use crate::testutil::{FakeCdn, FixtureFile, KEY, manifest_body, split};
    use std::sync::Arc;
    use std::time::Duration;
    use vfs_provider::{KIND_DIR, KIND_FILE, OPEN_READ, ST_IO_ERROR};

    /// One depot's files: path, bytes, chunk size.
    type Depot<'a> = &'a [(&'a str, &'a [u8], usize)];

    /// A provider over `depots` (depot ids 1, 2, ... in order), each served
    /// by its own fake CDN on a multi-thread runtime the fixture owns. The
    /// provider is driven from the test thread, which is not a runtime
    /// thread — as a director thread is not.
    struct Fixture {
        provider: Arc<DepotProvider>,
        _cdns: Vec<FakeCdn>,
        // Last: dropped after the CDNs whose tasks it runs.
        rt: tokio::runtime::Runtime,
    }

    fn fixture(depots: &[Depot]) -> Fixture {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let cfg = CdnConfig {
            request_timeout: Duration::from_secs(5),
            cooldown_base: Duration::from_millis(10),
            cooldown_max: Duration::from_millis(50),
            max_attempts: 2,
            ..CdnConfig::default()
        };
        let (parts, cdns) = rt.block_on(async {
            let mut parts = Vec::new();
            let mut cdns = Vec::new();
            for (i, files) in depots.iter().enumerate() {
                let depot = DepotId(i as u32 + 1);
                let id = ManifestId(100 + i as u64);
                let files: Vec<FixtureFile> = files
                    .iter()
                    .map(|&(path, data, chunk)| FixtureFile { path, data, chunk })
                    .collect();
                let cdn = FakeCdn::start().await;
                for f in &files {
                    cdn.put_chunks(depot, &split(f.data, f.chunk), &KEY);
                }
                let body = manifest_body(depot, id, &files, &[], None);
                let manifest =
                    Arc::new(DepotManifest::from_cdn_bytes(&body, depot, id, &KEY).unwrap());
                let reader =
                    DepotReader::new(AppId(7), depot, KEY, vec![cdn.server()], cfg.clone(), None)
                        .unwrap();
                parts.push((manifest, reader));
                cdns.push(cdn);
            }
            (parts, cdns)
        });
        let game = SteamGame::from_parts(AppId(7), parts);
        Fixture {
            provider: Arc::new(DepotProvider::new(game, rt.handle().clone())),
            _cdns: cdns,
            rt,
        }
    }

    fn at(rel: &str) -> VPath<'_> {
        VPath::at_default(rel)
    }

    fn names(p: &DepotProvider, rel: &str) -> Vec<String> {
        let mut v: Vec<String> = p
            .readdir(at(rel))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        v.sort();
        v
    }

    fn read(p: &DepotProvider, rel: &str, off: u64, len: usize) -> Vec<u8> {
        let (h, _, is_dir) = p.open(at(rel), OPEN_READ).unwrap();
        assert!(!is_dir);
        let mut buf = vec![0u8; len];
        let n = p.read_at(h, off, &mut buf).unwrap();
        p.close(h).unwrap();
        buf.truncate(n);
        buf
    }

    /// `FIXTURE_FILES` split across two depots, with chunks small enough
    /// that `read_all`'s 3-byte reads cross chunk boundaries.
    #[test]
    fn conformance() {
        let f = fixture(&[&[("a.txt", b"hello", 2)], &[("sub\\b.txt", b"world!", 4)]]);
        vfs_provider::assert_conformance(f.provider.clone());
    }

    /// Review focus 1: a game asks in any case; the manifest has one
    /// spelling, and that is the spelling a listing shows.
    #[test]
    fn case_insensitive_lookup() {
        let f = fixture(&[&[("Data\\Skyrim.esm", b"TES4 master", 4)]]);
        let p = &f.provider;
        let st = p.getattr(at("data/SKYRIM.ESM")).unwrap().unwrap();
        assert_eq!((st.kind, st.size), (KIND_FILE, 11));
        assert_eq!(p.getattr(at("DATA")).unwrap().unwrap().kind, KIND_DIR);
        assert_eq!(names(p, "DATA"), ["Skyrim.esm"]);
        assert_eq!(names(p, ""), ["Data"], "an implied directory is listed");
        assert_eq!(read(p, "dAtA/skyrim.ESM", 0, 64), b"TES4 master");
        assert_eq!(
            p.stored_name(at("DATA/SKYRIM.ESM")).unwrap().as_deref(),
            Some("Skyrim.esm")
        );
        assert_eq!(p.stored_name(at("data")).unwrap().as_deref(), Some("Data"));
        // Unicode folds too (`vfs_core::fold`, not ASCII lowercasing).
        let f = fixture(&[&[("Data\\ÜBER\\a.esp", b"x", 1)]]);
        assert_eq!(names(&f.provider, "data/über"), ["a.esp"]);
        assert_eq!(names(&f.provider, "data"), ["ÜBER"]);
    }

    /// Review focus 2: the earlier depot wins, and a listing shows the
    /// name once.
    #[test]
    fn earlier_depot_wins() {
        let f = fixture(&[
            &[("a.txt", b"depot one", 3), ("Data\\x.esp", b"1", 1)],
            &[("A.TXT", b"depot two!", 3), ("data\\y.esp", b"2", 1)],
        ]);
        let p = &f.provider;
        assert_eq!(read(p, "a.txt", 0, 64), b"depot one");
        assert_eq!(p.getattr(at("A.txt")).unwrap().unwrap().size, 9);
        assert_eq!(names(p, ""), ["Data", "a.txt"], "each name listed once");
        // A directory both depots have holds what each put there, under the
        // earlier depot's spelling.
        assert_eq!(names(p, "DATA"), ["x.esp", "y.esp"]);
        assert_eq!(read(p, "data/y.esp", 0, 4), b"2");
    }

    /// Review focus 3: a read crossing a chunk boundary, and reads at or
    /// past the end.
    #[test]
    fn read_spans_chunks_and_eof() {
        let body: Vec<u8> = (0..100u8).collect();
        let f = fixture(&[&[("big.bin", &body, 16)]]);
        let p = &f.provider;
        assert_eq!(read(p, "big.bin", 10, 30), &body[10..40], "three chunks");
        assert_eq!(read(p, "big.bin", 90, 64), &body[90..], "short at the end");
        assert_eq!(read(p, "big.bin", 100, 8), b"", "at EOF");
        assert_eq!(read(p, "big.bin", 1000, 8), b"", "past EOF");
    }

    /// Provider methods run on director threads. Called on a tokio worker
    /// instead, a read fails rather than blocking the runtime.
    #[test]
    fn a_read_on_a_tokio_worker_fails_instead_of_blocking() {
        let f = fixture(&[&[("a.txt", b"hello", 2)]]);
        let p = f.provider.clone();
        let (h, _, _) = p.open(at("a.txt"), OPEN_READ).unwrap();
        let task = f.rt.spawn(async move { p.read_at(h, 0, &mut [0u8; 4]) });
        let r = f.rt.block_on(task).unwrap();
        assert_eq!(r, Err(ST_IO_ERROR));
    }
}
