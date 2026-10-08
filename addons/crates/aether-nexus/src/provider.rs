//! [`ArchiveProvider`]: one repacked Nexus zip as a read-only
//! [`vfs_provider::Provider`] tree.
//!
//! Names, sizes and directories come from the archive's central directory,
//! indexed once when the provider is built (no request). Bytes come from
//! [`NexusArchive::read_range`], which fetches only the zstd frames a read
//! covers. Lookups fold case with [`vfs_core::fold`]; listings show the
//! archive's spelling.
//!
//! Provider calls are synchronous and come from threads that are not on the
//! tokio runtime (director workers). Each read is spawned on the runtime
//! whose handle the provider was given and the calling thread waits for it.
//! A call from a runtime worker is refused with `ST_IO_ERROR` rather than
//! blocking the worker (which could deadlock the runtime); async code calls
//! the provider through `tokio::task::spawn_blocking`.
//!
//! Signed links: building the provider, lookups, listings and opens make no
//! request. A read goes through the archive like any other, so it uses the
//! cached signed link and signs a new one only when that has expired or the
//! file host refuses it.
//!
//! Reads are slow (a round trip per miss) and decode whole frames: wrap the
//! provider in a block cache (`aether_storage::Storage::cached`); its
//! [`preferred_block`](vfs_provider::Capabilities::preferred_block) is the
//! Nexus frame size, so one miss fetches and keeps one frame.

use std::collections::HashMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use aether_archive::zip::PLAIN_FRAME_MAX;
use tokio::runtime::Handle as RtHandle;
use tokio::sync::oneshot;
use vfs_core::fold;
use vfs_provider::{
    Access, Capabilities, CaseMatch, DirEntry, Handle, HandleTable, KIND_DIR, KIND_FILE,
    OPEN_APPEND, OPEN_CREATE, OPEN_EXCL, OPEN_TRUNC, OPEN_WRITE, Provider, ST_BAD_FH, Stat, VPath,
    io_error, not_a_dir, not_found, read_only,
};

use crate::NexusArchive;

/// Every file's and directory's mtime (2020-01-01, as Haskill's provider
/// serves). The repack's DOS times are Nexus's, not the author's; a constant
/// keeps "newer than" checks stable across runs.
pub const MTIME: i64 = 1_577_836_800;

/// One `read_at` returns at most this many bytes (short reads are legal).
const MAX_READ: usize = 32 << 20;

#[derive(Debug)]
enum Kind {
    Dir { children: Vec<u32> },
    File { id: usize, size: u64 },
}

#[derive(Debug)]
struct Node {
    /// The last path component as the archive spells it (the first entry,
    /// in central-directory order, that passed through it).
    name: String,
    kind: Kind,
}

impl Node {
    fn stat(&self) -> Stat {
        match self.kind {
            Kind::Dir { .. } => Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: MTIME,
            },
            Kind::File { size, .. } => Stat {
                kind: KIND_FILE,
                size,
                mtime: MTIME,
            },
        }
    }
}

/// An open file: its entry and decompressed size.
#[derive(Debug, Clone, Copy)]
struct Open {
    id: usize,
    size: u64,
}

/// A repacked Nexus zip as a read-only, case-insensitive tree. See the
/// module documentation for the threading model.
pub struct ArchiveProvider {
    archive: Arc<NexusArchive>,
    rt: RtHandle,
    /// Node 0 is the root directory.
    nodes: Vec<Node>,
    /// Folded full path (`/`-separated, no leading slash) -> node.
    by_fold: HashMap<String, u32>,
    opens: HandleTable<Open>,
}

impl std::fmt::Debug for ArchiveProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveProvider")
            .field("uid", &self.archive.uid())
            .field("nodes", &self.nodes.len())
            .finish_non_exhaustive()
    }
}

impl ArchiveProvider {
    /// Index `archive`'s entries; reads run on `rt`. Makes no request.
    ///
    /// Directories are implied by entry paths as well as listed by
    /// directory entries. `\` in entry names is a separator. Of two entries
    /// whose paths fold equal, the first serves; a path that is both a file
    /// and a directory is served as the directory.
    pub fn new(archive: impl Into<Arc<NexusArchive>>, rt: RtHandle) -> ArchiveProvider {
        let archive = archive.into();
        let mut b = Builder {
            nodes: vec![Node {
                name: String::new(),
                kind: Kind::Dir {
                    children: Vec::new(),
                },
            }],
            by_fold: HashMap::new(),
        };
        for (id, e) in archive.index().entries().iter().enumerate() {
            let path = e.name.replace('\\', "/");
            let is_dir = path.ends_with('/');
            let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            let Some((leaf, dirs)) = parts.split_last() else {
                continue;
            };
            let mut parent = 0;
            let mut full = String::new();
            for d in dirs {
                parent = b.dir(parent, &mut full, d);
            }
            if is_dir {
                b.dir(parent, &mut full, leaf);
            } else {
                b.file(parent, &mut full, leaf, id, e.uncompressed_size);
            }
        }
        ArchiveProvider {
            archive,
            rt,
            nodes: b.nodes,
            by_fold: b.by_fold,
            opens: HandleTable::new(),
        }
    }

    /// The archive read through (for saving its index and data offsets).
    pub fn archive(&self) -> &Arc<NexusArchive> {
        &self.archive
    }

    fn lookup(&self, rel: &str) -> Option<&Node> {
        let key = fold(rel.trim_matches('/'));
        if key.is_empty() {
            return Some(&self.nodes[0]);
        }
        self.by_fold.get(&key).map(|&n| &self.nodes[n as usize])
    }

    /// Run `fut` on the runtime and wait for it on this thread. `None` if
    /// this thread is a runtime worker (waiting would block it) or the task
    /// failed.
    fn block<F>(&self, fut: F) -> Option<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let task = self.rt.spawn(async move {
            let _ = tx.send(fut.await);
        });
        // `blocking_recv` panics on a runtime worker instead of blocking it.
        match catch_unwind(AssertUnwindSafe(|| rx.blocking_recv())) {
            Ok(Ok(v)) => Some(v),
            Ok(Err(_)) => {
                tracing::warn!(uid = self.archive.uid(), "Nexus archive read task failed");
                None
            }
            Err(_) => {
                task.abort();
                tracing::error!(
                    "ArchiveProvider called on a tokio worker; call it from a plain \
                     thread or tokio::task::spawn_blocking"
                );
                None
            }
        }
    }
}

struct Builder {
    nodes: Vec<Node>,
    by_fold: HashMap<String, u32>,
}

impl Builder {
    /// Append `name` to the folded path `full` and return the node there,
    /// if any.
    fn step(&self, full: &mut String, name: &str) -> Option<u32> {
        if !full.is_empty() {
            full.push('/');
        }
        full.push_str(&fold(name));
        self.by_fold.get(full.as_str()).copied()
    }

    fn push(&mut self, parent: u32, full: &str, node: Node) -> u32 {
        let n = self.nodes.len() as u32;
        self.nodes.push(node);
        self.by_fold.insert(full.to_string(), n);
        if let Kind::Dir { children } = &mut self.nodes[parent as usize].kind {
            children.push(n);
        }
        n
    }

    /// The directory `name` under `parent`, created if new.
    fn dir(&mut self, parent: u32, full: &mut String, name: &str) -> u32 {
        if let Some(n) = self.step(full, name) {
            let node = &mut self.nodes[n as usize];
            if let Kind::File { .. } = node.kind {
                tracing::warn!(
                    path = %full,
                    "a Nexus archive path is both a file and a directory; serving the directory"
                );
                node.kind = Kind::Dir {
                    children: Vec::new(),
                };
            }
            return n;
        }
        self.push(
            parent,
            full,
            Node {
                name: name.to_string(),
                kind: Kind::Dir {
                    children: Vec::new(),
                },
            },
        )
    }

    /// The file `name` under `parent`, unless something is already there.
    fn file(&mut self, parent: u32, full: &mut String, name: &str, id: usize, size: u64) {
        if self.step(full, name).is_some() {
            tracing::warn!(
                path = %full,
                "two Nexus archive entries have the same path up to case; serving the first"
            );
            return;
        }
        self.push(
            parent,
            full,
            Node {
                name: name.to_string(),
                kind: Kind::File { id, size },
            },
        );
    }
}

impl Provider for ArchiveProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::Read,
            // An open archive reads at the offsets it was opened with; a
            // re-upload of another length fails reads instead.
            immutable: true,
            slow: true,
            preferred_block: Some(PLAIN_FRAME_MAX as u32),
            case: CaseMatch::Insensitive,
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        Ok(self.lookup(p.rel).map(Node::stat))
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let node = self.lookup(p.rel).ok_or_else(not_found)?;
        let Kind::Dir { children } = &node.kind else {
            return Err(not_a_dir());
        };
        let mut out: Vec<DirEntry> = children
            .iter()
            .map(|&c| {
                let n = &self.nodes[c as usize];
                DirEntry {
                    name: n.name.clone(),
                    stat: n.stat(),
                }
            })
            .collect();
        out.sort_by_cached_key(|e| fold(&e.name));
        Ok(out)
    }

    fn stored_name(&self, p: VPath) -> Result<Option<String>, i32> {
        if p.rel.trim_matches('/').is_empty() {
            return Ok(None);
        }
        Ok(self.lookup(p.rel).map(|n| n.name.clone()))
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        if flags & (OPEN_WRITE | OPEN_CREATE | OPEN_EXCL | OPEN_TRUNC | OPEN_APPEND) != 0 {
            return Err(read_only());
        }
        match self.lookup(p.rel).ok_or_else(not_found)?.kind {
            Kind::Dir { .. } => Ok((self.opens.fresh(), 0, true)),
            Kind::File { id, size } => Ok((self.opens.insert(Open { id, size })?, size, false)),
        }
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        // A directory's handle is not in the table; closing it is fine.
        match self.opens.remove(h) {
            Ok(_) | Err(ST_BAD_FH) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let Open { id, size } = self.opens.get(h)?;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let n = (size - offset).min(buf.len().min(MAX_READ) as u64);
        let range = offset..offset + n;
        let archive = self.archive.clone();
        let r = self
            .block(async move { archive.read_range(id, range).await })
            .ok_or_else(io_error)?;
        let bytes = r.map_err(|e| {
            tracing::warn!(
                uid = self.archive.uid(),
                entry = id,
                offset,
                error = %e,
                "Nexus archive read failed"
            );
            io_error()
        })?;
        if bytes.len() as u64 != n {
            tracing::warn!(
                uid = self.archive.uid(),
                entry = id,
                got = bytes.len(),
                want = n,
                "Nexus archive read returned the wrong length"
            );
            return Err(io_error());
        }
        buf[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
}
