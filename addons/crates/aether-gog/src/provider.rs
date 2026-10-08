//! [`DepotProvider`]: one or more GOG depots as a single read-only
//! [`vfs_provider::Provider`] tree.
//!
//! The tree is indexed once, at construction, from the manifests: paths
//! split on `\` or `/`, directories implied by file paths, lookups
//! case-folded (as `vfs_core::fold`) while listings keep the manifest's
//! spelling. On a path collision (up to case and separators) the earlier
//! depot wins, and within one depot the earlier item.
//!
//! Reads go through [`GogDepotFile`], blocking on the runtime handle given
//! to [`DepotProvider::new`]. Provider methods are therefore for plain
//! threads (a VFS's request threads, `spawn_blocking`); on a tokio worker a
//! read fails with `ST_IO_ERROR` instead of stalling the runtime.
use std::collections::HashMap;
use std::sync::Arc;

use tokio::runtime::Handle as RtHandle;
use vfs_provider::{
    Access, Capabilities, CaseMatch, DirEntry, Handle, HandleTable, KIND_DIR, KIND_FILE,
    OPEN_APPEND, OPEN_CREATE, OPEN_TRUNC, OPEN_WRITE, Provider, ST_BAD_FH, ST_NOT_A_DIRECTORY,
    Stat, VPath, from_io, io_error, not_found, read_only,
};

use crate::content::GogContent;
use crate::ids::ProductId;
use crate::manifest::{Chunk, DepotManifest};
use crate::reader::{GogDepotFile, block_on};

/// Every file's and directory's mtime (2020-01-01): manifests carry none,
/// and a constant keeps "newer than" checks stable across runs.
pub const MTIME: i64 = 1_577_836_800;

/// The CDN chunk size GOG uses (10 MiB inflated): the read unit a cache in
/// front of this provider should use.
const PREFERRED_BLOCK: u32 = 10 << 20;

/// Depot manifests as one read-only, case-insensitive tree.
pub struct DepotProvider {
    content: GogContent,
    rt: RtHandle,
    /// Node 0 is the root directory.
    nodes: Vec<Node>,
    /// Folded path (`/`-separated, no leading `/`) -> node.
    by_path: HashMap<String, u32>,
    opens: HandleTable<GogDepotFile>,
}

struct Node {
    /// The last path component as the manifest spells it (empty for the
    /// root).
    name: String,
    kind: Kind,
}

enum Kind {
    Dir(Vec<u32>),
    File(FileRef),
}

/// Where a file's bytes are: a byte range of a chunk list.
struct FileRef {
    product: ProductId,
    chunks: Arc<[Chunk]>,
    base: u64,
    size: u64,
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
                size: f.size,
                mtime: MTIME,
            },
        }
    }
}

/// The non-empty components of `path`, split on `/` and `\`.
fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split(['/', '\\']).filter(|c| !c.is_empty())
}

/// Case-fold one component as `vfs_core::fold` does.
fn fold(s: &str) -> String {
    if s.is_ascii() {
        s.to_ascii_lowercase()
    } else {
        s.chars().flat_map(char::to_lowercase).collect()
    }
}

/// The lookup key of `path`: folded components joined by `/`.
fn key(path: &str) -> String {
    components(path).map(fold).collect::<Vec<_>>().join("/")
}

impl DepotProvider {
    /// Serve `depots` (each with the product whose secure link serves its
    /// chunks: a depot's [`DepotRef::product_id`](crate::DepotRef)) as one
    /// tree, earlier depots winning collisions. `rt` must belong to a
    /// **multi-thread** runtime; reads block on it.
    ///
    /// Nothing is fetched here or on `open`: the secure link and chunks are
    /// fetched by the first read that needs them.
    pub fn new(
        content: GogContent,
        depots: Vec<(ProductId, Arc<DepotManifest>)>,
        rt: RtHandle,
    ) -> Self {
        let mut p = DepotProvider {
            content,
            rt,
            nodes: vec![Node {
                name: String::new(),
                kind: Kind::Dir(Vec::new()),
            }],
            by_path: HashMap::from([(String::new(), 0)]),
            opens: HandleTable::new(),
        };
        for (product, depot) in &depots {
            let sfc: Option<Arc<[Chunk]>> = depot.small_files_container.clone().map(Into::into);
            for item in &depot.items {
                let (chunks, base) = match (item.sfc_ref, &sfc) {
                    (Some(r), Some(c)) => (c.clone(), r.offset),
                    _ => (item.chunks.clone().into(), 0),
                };
                let have: u64 = chunks.iter().map(|c| c.size).sum();
                if base.checked_add(item.size).is_none_or(|end| end > have) {
                    tracing::warn!(
                        path = %item.path,
                        "GOG depot item's range is outside its chunks; leaving it out"
                    );
                    continue;
                }
                p.add_file(
                    &item.path,
                    FileRef {
                        product: *product,
                        chunks,
                        base,
                        size: item.size,
                    },
                );
            }
        }
        p
    }

    /// Add the file at `path` with its directories, unless something is
    /// already there (an earlier depot's or item's).
    fn add_file(&mut self, path: &str, file: FileRef) {
        let comps: Vec<&str> = components(path).collect();
        let Some((name, dirs)) = comps.split_last() else {
            return;
        };
        let mut parent = 0u32;
        let mut k = String::new();
        for d in dirs {
            if !k.is_empty() {
                k.push('/');
            }
            k.push_str(&fold(d));
            parent = match self.by_path.get(&k) {
                Some(&n) if matches!(self.nodes[n as usize].kind, Kind::Dir(_)) => n,
                Some(_) => {
                    tracing::warn!(path, "a GOG depot path runs through a file; leaving it out");
                    return;
                }
                None => self.push(parent, &k, d, Kind::Dir(Vec::new())),
            };
        }
        if !k.is_empty() {
            k.push('/');
        }
        k.push_str(&fold(name));
        if self.by_path.contains_key(&k) {
            tracing::debug!(path, "GOG depot path already served by an earlier entry");
            return;
        }
        self.push(parent, &k, name, Kind::File(file));
    }

    fn push(&mut self, parent: u32, key: &str, name: &str, kind: Kind) -> u32 {
        let n = self.nodes.len() as u32;
        self.nodes.push(Node {
            name: name.to_string(),
            kind,
        });
        self.by_path.insert(key.to_string(), n);
        if let Kind::Dir(kids) = &mut self.nodes[parent as usize].kind {
            kids.push(n);
        }
        n
    }

    fn node(&self, p: VPath) -> Option<&Node> {
        self.by_path
            .get(&key(p.rel))
            .map(|&n| &self.nodes[n as usize])
    }
}

impl Provider for DepotProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::Read,
            immutable: true,
            slow: true,
            preferred_block: Some(PREFERRED_BLOCK),
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
            Kind::File(_) => Err(ST_NOT_A_DIRECTORY),
        }
    }

    fn stored_name(&self, p: VPath) -> Result<Option<String>, i32> {
        Ok(self
            .node(p)
            .map(|n| n.name.clone())
            .filter(|n| !n.is_empty()))
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let node = self.node(p).ok_or_else(not_found)?;
        if flags & (OPEN_WRITE | OPEN_CREATE | OPEN_TRUNC | OPEN_APPEND) != 0 {
            return Err(read_only());
        }
        match &node.kind {
            Kind::Dir(_) => Ok((self.opens.fresh(), 0, true)),
            Kind::File(f) => {
                let file = GogDepotFile::new(
                    self.content.clone(),
                    f.product,
                    f.chunks.clone(),
                    f.base,
                    f.size,
                );
                Ok((self.opens.insert(file)?, f.size, false))
            }
        }
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        // A directory's handle, or one already closed, is not an error.
        match self.opens.remove(h) {
            Ok(_) | Err(ST_BAD_FH) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let file = self.opens.get(h)?;
        let r = block_on(&self.rt, file.read_at(offset, buf)).map_err(|e| {
            tracing::warn!(error = %e, "GOG depot read on a tokio worker thread");
            io_error()
        })?;
        r.map_err(|e| {
            tracing::warn!(error = %e, offset, "GOG depot read failed");
            from_io(&e.into())
        })
    }
}
