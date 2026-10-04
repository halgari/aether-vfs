//! The catalog: a redb database beside the block store that names what the
//! store holds.
//!
//! Tables:
//!
//! - `layers`: layer name → layer id. Ids come from the `meta` counter
//!   `next_layer_id` and are never reused.
//! - `entries`: `(layer id, folded path)` → encoded [`EntryRec`]. A layer's
//!   root directory is the row with the empty path, created by
//!   [`Catalog::create_layer`]. Paths are `/`-separated, relative to the layer
//!   root, and case-folded with [`vfs_core::fold`]; the original case is kept in
//!   [`EntryRec::name`].
//! - `cache_files`: cache identity hash → `(last access minute, logical bytes)`.
//! - `meta`: counters, and `clean_close` (present only from a clean close to
//!   the next open: see [`crate::Storage::open`]).
//!
//! ## Durability
//!
//! Every write commits with `Durability::None` unless it says otherwise (only
//! [`Catalog::put`] and [`Catalog::remove`] with `durable: true`), and
//! [`Catalog::commit_durable`] makes everything before it durable. The catalog
//! and the block store commit separately, so the caller orders them:
//! `BlockStore::flush()` first, then `commit_durable`, so that every durable
//! catalog row references durable store data. A durable commit makes *all*
//! earlier non-durable commits durable too, which is why the catalog never
//! makes one on its own initiative.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, Table, TableDefinition, WriteTransaction,
};

use crate::ids::Guid;
use crate::storage::StorageError;

const LAYERS: TableDefinition<&str, u64> = TableDefinition::new("layers");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const ENTRIES: TableDefinition<(u64, &str), &[u8]> = TableDefinition::new("entries");
const CACHE_FILES: TableDefinition<[u8; 16], (u64, u64)> = TableDefinition::new("cache_files");

const META_NEXT_LAYER_ID: &str = "next_layer_id";
/// Present only between a clean close and the next open: the random token
/// that close also left as the block store's clean-shutdown value. See
/// [`crate::Storage::close`].
const META_CLEAN_CLOSE: &str = "clean_close";
/// redb page cache. The catalog is small next to the store's index.
/// The catalog's redb page cache unless [`crate::StorageConfig::catalog_cache_bytes`]
/// says otherwise.
pub(crate) const CACHE_BYTES: usize = 16 << 20;
/// First byte of an encoded [`EntryRec`].
const ENTRY_VERSION: u8 = 1;
/// version + kind + guid + len + mtime, then the name.
const ENTRY_HEADER: usize = 1 + 1 + 16 + 8 + 8;

/// One file or directory of a layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRec {
    /// The last path component, in its original case.
    pub name: String,
    /// `vfs_provider::KIND_FILE` or `KIND_DIR`.
    pub kind: u8,
    /// A file's GUID; its store id is [`crate::layer_file_id`] of it. Zero for
    /// directories.
    pub guid: Guid,
    pub len: u64,
    pub mtime: i64,
}

impl EntryRec {
    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(ENTRY_HEADER + self.name.len());
        b.push(ENTRY_VERSION);
        b.push(self.kind);
        b.extend_from_slice(&self.guid);
        b.extend_from_slice(&self.len.to_le_bytes());
        b.extend_from_slice(&self.mtime.to_le_bytes());
        b.extend_from_slice(self.name.as_bytes());
        b
    }

    fn decode(b: &[u8]) -> Result<Self, StorageError> {
        if b.len() < ENTRY_HEADER || b[0] != ENTRY_VERSION {
            return Err(StorageError::Catalog(format!(
                "undecodable entry row ({} bytes, version {:?})",
                b.len(),
                b.first()
            )));
        }
        let name = std::str::from_utf8(&b[ENTRY_HEADER..])
            .map_err(|_| StorageError::Catalog("entry name is not UTF-8".into()))?;
        Ok(Self {
            name: name.to_owned(),
            kind: b[1],
            guid: b[2..18].try_into().unwrap(),
            len: u64::from_le_bytes(b[18..26].try_into().unwrap()),
            mtime: i64::from_le_bytes(b[26..34].try_into().unwrap()),
        })
    }
}

/// One cached file's eviction bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheRec {
    /// Minutes since the Unix epoch of the last access.
    pub last_access_min: u64,
    /// The file's length: bytes it counts against `cache_max_bytes`.
    pub logical_bytes: u64,
}

/// The catalog database. Cheap to share between threads; redb serialises
/// writers, so one write blocks while another is in progress.
///
/// **Each call is atomic; a sequence of calls is not.** A `get` then `put`, or
/// `children` then `remove`, can interleave with another thread's writes. The
/// refusals that protect the tree's shape (a non-empty directory in
/// [`Catalog::remove`] and as a [`Catalog::rename`] destination) are therefore
/// checked inside the call's own transaction, and a layer provider serialises
/// its read-modify-write sequences with a per-layer lock.
pub struct Catalog {
    db: Database,
    /// Non-durable commits since the last durable one (approximate upward:
    /// a commit racing a durable one may be counted after it made it
    /// durable). redb holds memory for them until a durable commit.
    unflushed: AtomicU64,
}

fn db_err(e: impl std::fmt::Display) -> StorageError {
    StorageError::Catalog(e.to_string())
}

/// The key prefix shared by every entry under `folded_dir`: `dir/`, or empty
/// for the layer root.
fn dir_prefix(folded_dir: &str) -> String {
    if folded_dir.is_empty() {
        String::new()
    } else {
        format!("{folded_dir}/")
    }
}

impl Catalog {
    /// Opens or creates the catalog at `path`.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        Self::open_with_cache(path, CACHE_BYTES)
    }

    /// [`Catalog::open`] with a redb page cache of `cache_bytes`.
    pub fn open_with_cache(path: &Path, cache_bytes: usize) -> Result<Self, StorageError> {
        let db = Database::builder()
            .set_cache_size(cache_bytes)
            .create(path)
            .map_err(db_err)?;
        let c = Self {
            db,
            unflushed: AtomicU64::new(0),
        };
        // Create every table so read transactions can always open them.
        c.write(false, |txn| {
            txn.open_table(LAYERS).map_err(db_err)?;
            txn.open_table(META).map_err(db_err)?;
            txn.open_table(ENTRIES).map_err(db_err)?;
            txn.open_table(CACHE_FILES).map_err(db_err)?;
            Ok(())
        })?;
        Ok(c)
    }

    /// Runs `f` in one write transaction and commits it if `f` returns `Ok`.
    fn write<R>(
        &self,
        durable: bool,
        f: impl FnOnce(&WriteTransaction) -> Result<R, StorageError>,
    ) -> Result<R, StorageError> {
        let mut txn = self.db.begin_write().map_err(db_err)?;
        txn.set_durability(if durable {
            Durability::Immediate
        } else {
            Durability::None
        })
        .map_err(db_err)?;
        let r = f(&txn)?;
        // Counted before a durable commit covers them, so a commit that
        // lands meanwhile is at worst counted again.
        let covered = if durable {
            self.unflushed.load(Ordering::Acquire)
        } else {
            0
        };
        txn.commit().map_err(db_err)?;
        if durable {
            let _ = self
                .unflushed
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                    Some(n.saturating_sub(covered))
                });
        } else {
            self.unflushed.fetch_add(1, Ordering::AcqRel);
        }
        Ok(r)
    }

    fn entries<R>(
        &self,
        f: impl FnOnce(
            &redb::ReadOnlyTable<(u64, &'static str), &'static [u8]>,
        ) -> Result<R, StorageError>,
    ) -> Result<R, StorageError> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let t = txn.open_table(ENTRIES).map_err(db_err)?;
        f(&t)
    }

    /// The id of the layer named exactly `name`.
    pub fn layer_id(&self, name: &str) -> Result<Option<u64>, StorageError> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let t = txn.open_table(LAYERS).map_err(db_err)?;
        Ok(t.get(name).map_err(db_err)?.map(|g| g.value()))
    }

    /// Creates a layer with an empty root directory and returns its id. Refuses
    /// a name that exists with [`StorageError::LayerExists`].
    pub fn create_layer(&self, name: &str) -> Result<u64, StorageError> {
        self.write(false, |txn| {
            let mut layers = txn.open_table(LAYERS).map_err(db_err)?;
            if layers.get(name).map_err(db_err)?.is_some() {
                return Err(StorageError::LayerExists(name.to_owned()));
            }
            let mut meta = txn.open_table(META).map_err(db_err)?;
            let id = meta
                .get(META_NEXT_LAYER_ID)
                .map_err(db_err)?
                .map_or(1, |g| g.value());
            meta.insert(META_NEXT_LAYER_ID, id + 1).map_err(db_err)?;
            layers.insert(name, id).map_err(db_err)?;
            let root = EntryRec {
                name: String::new(),
                kind: vfs_provider::KIND_DIR,
                guid: [0; 16],
                len: 0,
                mtime: 0,
            };
            let mut entries = txn.open_table(ENTRIES).map_err(db_err)?;
            entries
                .insert((id, ""), root.encode().as_slice())
                .map_err(db_err)?;
            Ok(id)
        })
    }

    /// Whether a layer was ever created in this catalog (its layer-id
    /// counter exists). A fresh or replaced catalog has none.
    pub fn has_layer_history(&self) -> Result<bool, StorageError> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let t = txn.open_table(META).map_err(db_err)?;
        Ok(t.get(META_NEXT_LAYER_ID).map_err(db_err)?.is_some())
    }

    /// Every layer, as `(name, id)`, in name order.
    pub fn layer_names(&self) -> Result<Vec<(String, u64)>, StorageError> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let t = txn.open_table(LAYERS).map_err(db_err)?;
        let mut out = Vec::new();
        for e in t.iter().map_err(db_err)? {
            let (k, v) = e.map_err(db_err)?;
            out.push((k.value().to_owned(), v.value()));
        }
        Ok(out)
    }

    /// Removes a layer: its name and every entry row, in one transaction.
    /// Returns the GUIDs of its files, for the caller to delete from the store
    /// (after making this durable — catalog first, store second).
    pub fn drop_layer(&self, layer: u64) -> Result<Vec<Guid>, StorageError> {
        self.write(false, |txn| {
            let mut layers = txn.open_table(LAYERS).map_err(db_err)?;
            let mut name = None;
            for e in layers.iter().map_err(db_err)? {
                let (k, v) = e.map_err(db_err)?;
                if v.value() == layer {
                    name = Some(k.value().to_owned());
                    break;
                }
            }
            let Some(name) = name else {
                return Err(StorageError::NoSuchLayer(format!("#{layer}")));
            };
            layers.remove(name.as_str()).map_err(db_err)?;
            let mut entries = txn.open_table(ENTRIES).map_err(db_err)?;
            let mut guids = Vec::new();
            let mut keys = Vec::new();
            for e in entries.range((layer, "")..).map_err(db_err)? {
                let (k, v) = e.map_err(db_err)?;
                let (l, path) = k.value();
                if l != layer {
                    break;
                }
                let rec = EntryRec::decode(v.value())?;
                if rec.kind == vfs_provider::KIND_FILE {
                    guids.push(rec.guid);
                }
                keys.push(path.to_owned());
            }
            for k in &keys {
                entries.remove((layer, k.as_str())).map_err(db_err)?;
            }
            Ok(guids)
        })
    }

    /// The entry at `folded` (folded again here, so a caller that passes
    /// original case still finds it).
    pub fn get(&self, layer: u64, folded: &str) -> Result<Option<EntryRec>, StorageError> {
        let key = vfs_core::fold(folded);
        self.entries(|t| {
            t.get((layer, key.as_str()))
                .map_err(db_err)?
                .map(|g| EntryRec::decode(g.value()))
                .transpose()
        })
    }

    /// Inserts or replaces the entry at `folded`.
    ///
    /// `durable: false` commits with no durability. `durable: true` commits
    /// with `Durability::Immediate`, and a durable commit also makes **every
    /// earlier non-durable row** durable, not just this one: call
    /// `BlockStore::flush()` first, so no durable row references store data that
    /// is not yet durable (spec §6).
    pub fn put(
        &self,
        layer: u64,
        folded: &str,
        rec: &EntryRec,
        durable: bool,
    ) -> Result<(), StorageError> {
        let key = vfs_core::fold(folded);
        self.write(durable, |txn| {
            let mut t = txn.open_table(ENTRIES).map_err(db_err)?;
            t.insert((layer, key.as_str()), rec.encode().as_slice())
                .map_err(db_err)?;
            Ok(())
        })
    }

    /// [`Self::put`] of every `(folded, rec)` of `rows`, in one commit.
    pub fn put_many(
        &self,
        layer: u64,
        rows: &[(String, EntryRec)],
        durable: bool,
    ) -> Result<(), StorageError> {
        self.write(durable, |txn| {
            let mut t = txn.open_table(ENTRIES).map_err(db_err)?;
            for (folded, rec) in rows {
                let key = vfs_core::fold(folded);
                t.insert((layer, key.as_str()), rec.encode().as_slice())
                    .map_err(db_err)?;
            }
            Ok(())
        })
    }

    /// Removes the entry at `folded`: a file, or an **empty** directory.
    ///
    /// Refused, inside the transaction, with [`StorageError::NotEmpty`] when any
    /// row exists under `folded/` — removing only the directory's row would
    /// orphan its children (invisible, never reconciled, and resurrected when
    /// the directory is re-created). [`StorageError::NotFound`] for a missing
    /// row, [`StorageError::BadRequest`] for the layer root.
    ///
    /// `durable` is as for [`Self::put`], including that a durable commit makes
    /// every earlier non-durable row durable: `BlockStore::flush()` first.
    pub fn remove(&self, layer: u64, folded: &str, durable: bool) -> Result<(), StorageError> {
        let key = vfs_core::fold(folded);
        if key.is_empty() {
            return Err(StorageError::BadRequest("remove of a layer root".into()));
        }
        self.write(durable, |txn| {
            let mut t = txn.open_table(ENTRIES).map_err(db_err)?;
            if has_children(&t, layer, &key)? {
                return Err(StorageError::NotEmpty(key.clone()));
            }
            if t.remove((layer, key.as_str())).map_err(db_err)?.is_none() {
                return Err(StorageError::NotFound(key.clone()));
            }
            Ok(())
        })
    }

    /// The direct children of `folded_dir` (`""` for the layer root), in key
    /// order.
    ///
    /// O(direct children), not O(subtree): keys under `prefix` are contiguous in
    /// byte order, and on meeting a grandchild `prefix + c + "/..."` the scan
    /// jumps to `prefix + c + "0"` — `'0'` is the byte after `'/'` — which is
    /// the first key past all of `c`'s subtree.
    pub fn children(&self, layer: u64, folded_dir: &str) -> Result<Vec<EntryRec>, StorageError> {
        let prefix = dir_prefix(&vfs_core::fold(folded_dir));
        self.entries(|t| {
            let mut out = Vec::new();
            let mut start = prefix.clone();
            loop {
                let mut skip_to = None;
                for e in t.range((layer, start.as_str())..).map_err(db_err)? {
                    let (k, v) = e.map_err(db_err)?;
                    let (l, path) = k.value();
                    if l != layer {
                        break;
                    }
                    let Some(rest) = path.strip_prefix(prefix.as_str()) else {
                        break;
                    };
                    match rest.find('/') {
                        Some(i) => {
                            skip_to = Some(format!("{prefix}{}0", &rest[..i]));
                            break;
                        }
                        None if !rest.is_empty() => out.push(EntryRec::decode(v.value())?),
                        None => {}
                    }
                }
                match skip_to {
                    Some(s) => start = s,
                    None => return Ok(out),
                }
            }
        })
    }

    /// Moves the entry at `from` to `to`, naming it `to_name`, with every entry
    /// under `from/` moved under `to/` — one transaction, so a crash leaves
    /// either the old tree or the new one. Non-durable; see the module docs.
    ///
    /// An existing `to` that is a file or an **empty** directory is replaced,
    /// and the GUIDs of the file rows replaced are returned — collected in the
    /// same transaction — for the caller to delete from the store. A `to` that
    /// is a directory with anything under it is refused with
    /// [`StorageError::Exists`]: there is no correct way to combine two
    /// subtrees. Whether a file may replace a directory (or the reverse) is the
    /// provider's rule to apply, not the catalog's.
    ///
    /// `from == to` after folding (a case-only rename) changes only the name.
    /// [`StorageError::NotFound`] for a missing `from`;
    /// [`StorageError::BadRequest`] for a layer root or a move into its own
    /// subtree.
    pub fn rename(
        &self,
        layer: u64,
        from: &str,
        to: &str,
        to_name: &str,
    ) -> Result<Vec<Guid>, StorageError> {
        let from = vfs_core::fold(from);
        let to = vfs_core::fold(to);
        if from.is_empty() || to.is_empty() {
            return Err(StorageError::BadRequest("rename of a layer root".into()));
        }
        if from != to && to.starts_with(&dir_prefix(&from)) {
            return Err(StorageError::BadRequest(format!(
                "rename of {from:?} into its own subtree {to:?}"
            )));
        }
        self.write(false, |txn| {
            let mut t = txn.open_table(ENTRIES).map_err(db_err)?;
            let mut rec = match t.get((layer, from.as_str())).map_err(db_err)? {
                Some(g) => EntryRec::decode(g.value())?,
                None => return Err(StorageError::NotFound(from.clone())),
            };
            rec.name = to_name.to_owned();
            if from == to {
                t.insert((layer, to.as_str()), rec.encode().as_slice())
                    .map_err(db_err)?;
                return Ok(Vec::new());
            }
            if has_children(&t, layer, &to)? {
                return Err(StorageError::Exists(to.clone()));
            }
            let mut replaced = Vec::new();
            if let Some(g) = t.remove((layer, to.as_str())).map_err(db_err)? {
                let old = EntryRec::decode(g.value())?;
                if old.kind == vfs_provider::KIND_FILE {
                    replaced.push(old.guid);
                }
            }
            let from_prefix = dir_prefix(&from);
            let to_prefix = dir_prefix(&to);
            for path in subtree(&t, layer, &from)? {
                let v = match t.remove((layer, path.as_str())).map_err(db_err)? {
                    Some(g) => g.value().to_vec(),
                    None => continue,
                };
                if path == from {
                    continue;
                }
                let moved = format!("{to_prefix}{}", &path[from_prefix.len()..]);
                t.insert((layer, moved.as_str()), v.as_slice())
                    .map_err(db_err)?;
            }
            t.insert((layer, to.as_str()), rec.encode().as_slice())
                .map_err(db_err)?;
            Ok(replaced)
        })
    }

    /// Every layer file, as `(layer id, folded path, GUID)`.
    pub fn all_layer_guids(&self) -> Result<Vec<(u64, String, Guid)>, StorageError> {
        self.entries(|t| {
            let mut out = Vec::new();
            for e in t.iter().map_err(db_err)? {
                let (k, v) = e.map_err(db_err)?;
                let rec = EntryRec::decode(v.value())?;
                if rec.kind == vfs_provider::KIND_FILE {
                    let (l, path) = k.value();
                    out.push((l, path.to_owned(), rec.guid));
                }
            }
            Ok(out)
        })
    }

    /// Per layer id, its file count and the sum of its files' row lengths.
    /// Layers without files are absent.
    pub(crate) fn layer_file_totals(
        &self,
    ) -> Result<std::collections::HashMap<u64, (u64, u64)>, StorageError> {
        self.entries(|t| {
            let mut out = std::collections::HashMap::<u64, (u64, u64)>::new();
            for e in t.iter().map_err(db_err)? {
                let (k, v) = e.map_err(db_err)?;
                let rec = EntryRec::decode(v.value())?;
                if rec.kind == vfs_provider::KIND_FILE {
                    let tot = out.entry(k.value().0).or_default();
                    tot.0 += 1;
                    tot.1 = tot.1.saturating_add(rec.len);
                }
            }
            Ok(out)
        })
    }

    pub fn cache_get(&self, id: &[u8; 16]) -> Result<Option<CacheRec>, StorageError> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let t = txn.open_table(CACHE_FILES).map_err(db_err)?;
        Ok(t.get(id).map_err(db_err)?.map(|g| cache_rec(g.value())))
    }

    /// Inserts or replaces several cache rows in one non-durable transaction.
    pub fn cache_put_many(&self, recs: &[([u8; 16], CacheRec)]) -> Result<(), StorageError> {
        self.write(false, |txn| {
            let mut t = txn.open_table(CACHE_FILES).map_err(db_err)?;
            for (id, r) in recs {
                t.insert(id, (r.last_access_min, r.logical_bytes))
                    .map_err(db_err)?;
            }
            Ok(())
        })
    }

    pub fn cache_remove(&self, id: &[u8; 16]) -> Result<(), StorageError> {
        self.write(false, |txn| {
            let mut t = txn.open_table(CACHE_FILES).map_err(db_err)?;
            t.remove(id).map_err(db_err)?;
            Ok(())
        })
    }

    pub fn cache_all(&self) -> Result<Vec<([u8; 16], CacheRec)>, StorageError> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let t = txn.open_table(CACHE_FILES).map_err(db_err)?;
        let mut out = Vec::new();
        for e in t.iter().map_err(db_err)? {
            let (k, v) = e.map_err(db_err)?;
            out.push((k.value(), cache_rec(v.value())));
        }
        Ok(out)
    }

    /// Makes every earlier non-durable write durable (an empty
    /// `Durability::Immediate` commit).
    pub fn commit_durable(&self) -> Result<(), StorageError> {
        self.write(true, |_| Ok(()))
    }

    /// Records, durably, that the storage closed cleanly, with the random
    /// `token` the block store's clean shutdown recorded too: see
    /// [`crate::Storage::close`]. The last write of a clean close.
    pub(crate) fn mark_clean_close(&self, token: u64) -> Result<(), StorageError> {
        self.write(true, |txn| {
            let mut meta = txn.open_table(META).map_err(db_err)?;
            meta.insert(META_CLEAN_CLOSE, token).map_err(db_err)?;
            Ok(())
        })
    }

    /// Removes the clean-close mark, durably, and returns the token it
    /// held (`None`, and no commit, when there was none). The first write of
    /// an open: no later write can be covered by a mark it did not earn.
    pub(crate) fn take_clean_close(&self) -> Result<Option<u64>, StorageError> {
        let held = {
            let txn = self.db.begin_read().map_err(db_err)?;
            let t = txn.open_table(META).map_err(db_err)?;
            t.get(META_CLEAN_CLOSE).map_err(db_err)?.map(|g| g.value())
        };
        if held.is_some() {
            self.write(true, |txn| {
                let mut meta = txn.open_table(META).map_err(db_err)?;
                meta.remove(META_CLEAN_CLOSE).map_err(db_err)?;
                Ok(())
            })?;
        }
        Ok(held)
    }

    /// Non-durable commits made since the last durable one; 0 when a durable
    /// commit would have nothing to make durable. May overcount.
    pub(crate) fn unflushed_commits(&self) -> u64 {
        self.unflushed.load(Ordering::Acquire)
    }
}

fn cache_rec((last_access_min, logical_bytes): (u64, u64)) -> CacheRec {
    CacheRec {
        last_access_min,
        logical_bytes,
    }
}

/// Whether any row exists under `path/` (one range probe).
fn has_children(
    t: &impl ReadableTable<(u64, &'static str), &'static [u8]>,
    layer: u64,
    path: &str,
) -> Result<bool, StorageError> {
    use std::ops::Bound;
    let prefix = dir_prefix(path);
    // Excluded: for the root the prefix is `""`, which is the root's own row.
    let from = (layer, prefix.as_str());
    let mut range = t
        .range::<(u64, &str)>((Bound::Excluded(from), Bound::Unbounded))
        .map_err(db_err)?;
    let Some(e) = range.next() else {
        return Ok(false);
    };
    let (k, _) = e.map_err(db_err)?;
    let (l, p) = k.value();
    Ok(l == layer && p.starts_with(prefix.as_str()))
}

/// `path` itself and every key under `path/`, in one layer.
fn subtree(
    t: &Table<(u64, &'static str), &'static [u8]>,
    layer: u64,
    path: &str,
) -> Result<Vec<String>, StorageError> {
    let mut out = Vec::new();
    if t.get((layer, path)).map_err(db_err)?.is_some() {
        out.push(path.to_owned());
    }
    let prefix = dir_prefix(path);
    for e in t.range((layer, prefix.as_str())..).map_err(db_err)? {
        let (k, _) = e.map_err(db_err)?;
        let (l, p) = k.value();
        if l != layer || !p.starts_with(prefix.as_str()) {
            break;
        }
        out.push(p.to_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StorageError;

    #[test]
    fn layers_are_named_unique_and_persistent() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let c = Catalog::open(&dir.path().join("c.redb")).unwrap();
            let id = c.create_layer("prof").unwrap();
            assert!(matches!(
                c.create_layer("prof"),
                Err(StorageError::LayerExists(_))
            ));
            c.commit_durable().unwrap();
            id
        };
        let c = Catalog::open(&dir.path().join("c.redb")).unwrap();
        assert_eq!(c.layer_id("prof").unwrap(), Some(id));
        assert!(c
            .get(id, "")
            .unwrap()
            .is_some_and(|r| r.kind == vfs_provider::KIND_DIR));
    }

    #[test]
    fn children_are_direct_only_and_case_preserving() {
        let dir = tempfile::tempdir().unwrap();
        let c = Catalog::open(&dir.path().join("c.redb")).unwrap();
        let l = c.create_layer("p").unwrap();
        let file = |name: &str| EntryRec {
            name: name.into(),
            kind: vfs_provider::KIND_FILE,
            guid: [1; 16],
            len: 1,
            mtime: 0,
        };
        let dirr = |name: &str| EntryRec {
            name: name.into(),
            kind: vfs_provider::KIND_DIR,
            guid: [0; 16],
            len: 0,
            mtime: 0,
        };
        c.put(l, "saves", &dirr("Saves"), false).unwrap();
        c.put(l, "saves/one.ess", &file("One.ess"), false).unwrap();
        c.put(l, "saves/sub", &dirr("Sub"), false).unwrap();
        c.put(l, "saves/sub/deep.ess", &file("deep.ess"), false)
            .unwrap();
        let mut names: Vec<_> = c
            .children(l, "saves")
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["One.ess", "Sub"]);
        assert_eq!(c.children(l, "").unwrap().len(), 1);
    }

    #[test]
    fn rename_moves_a_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let c = Catalog::open(&dir.path().join("c.redb")).unwrap();
        let l = c.create_layer("p").unwrap();
        let dirr = |name: &str| EntryRec {
            name: name.into(),
            kind: vfs_provider::KIND_DIR,
            guid: [0; 16],
            len: 0,
            mtime: 0,
        };
        let file = |name: &str| EntryRec {
            name: name.into(),
            kind: vfs_provider::KIND_FILE,
            guid: [7; 16],
            len: 3,
            mtime: 0,
        };
        c.put(l, "a", &dirr("A"), false).unwrap();
        c.put(l, "a/x", &file("x"), false).unwrap();
        c.rename(l, "a", "b", "B").unwrap();
        assert!(c.get(l, "a").unwrap().is_none() && c.get(l, "a/x").unwrap().is_none());
        assert_eq!(c.get(l, "b").unwrap().unwrap().name, "B");
        assert_eq!(c.get(l, "b/x").unwrap().unwrap().guid, [7; 16]);
    }

    fn file(name: &str, guid: u8) -> EntryRec {
        EntryRec {
            name: name.into(),
            kind: vfs_provider::KIND_FILE,
            guid: [guid; 16],
            len: 1,
            mtime: 5,
        }
    }

    fn dirr(name: &str) -> EntryRec {
        EntryRec {
            name: name.into(),
            kind: vfs_provider::KIND_DIR,
            guid: [0; 16],
            len: 0,
            mtime: 0,
        }
    }

    fn open(dir: &tempfile::TempDir) -> Catalog {
        Catalog::open(&dir.path().join("c.redb")).unwrap()
    }

    /// `saves-old` sorts between `saves` and `saves/`; the scan must neither
    /// stop early on it nor count it.
    #[test]
    fn children_ignore_a_sibling_sharing_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        let l = c.create_layer("p").unwrap();
        c.put(l, "saves", &dirr("saves"), false).unwrap();
        c.put(l, "saves-old", &dirr("saves-old"), false).unwrap();
        c.put(l, "saves-old/x", &file("x", 1), false).unwrap();
        c.put(l, "saves/a", &file("a", 2), false).unwrap();
        let names: Vec<_> = c
            .children(l, "saves")
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, vec!["a"]);
        assert_eq!(c.children(l, "").unwrap().len(), 2);
        assert!(c.children(l, "nothing").unwrap().is_empty());
    }

    #[test]
    fn lookups_fold_their_argument_and_layers_are_separate() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        let a = c.create_layer("a").unwrap();
        let b = c.create_layer("b").unwrap();
        assert_ne!(a, b);
        c.put(a, "Saves", &dirr("Saves"), false).unwrap();
        assert_eq!(c.get(a, "saves").unwrap().unwrap().name, "Saves");
        assert_eq!(c.get(a, "SAVES").unwrap().unwrap().name, "Saves");
        assert!(c.get(b, "saves").unwrap().is_none());
        assert!(c.children(b, "").unwrap().is_empty());
        c.remove(a, "SAVES", false).unwrap();
        assert!(c.get(a, "saves").unwrap().is_none());
        assert!(matches!(
            c.remove(a, "saves", false),
            Err(StorageError::NotFound(_))
        ));
    }

    /// Removing only a directory's own row would orphan its children:
    /// invisible, never reconciled, and back again when the directory is
    /// re-created.
    #[test]
    fn remove_refuses_a_directory_with_children() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        let l = c.create_layer("p").unwrap();
        c.put(l, "d", &dirr("d"), false).unwrap();
        c.put(l, "d/f", &file("f", 1), false).unwrap();
        c.put(l, "d-sibling", &file("s", 2), false).unwrap();
        let err = c.remove(l, "d", false).unwrap_err();
        assert!(matches!(err, StorageError::NotEmpty(_)), "{err}");
        assert_eq!(err.to_status(), vfs_provider::ST_IS_DIR);
        assert!(c.get(l, "d").unwrap().is_some() && c.get(l, "d/f").unwrap().is_some());
        c.remove(l, "d/f", false).unwrap();
        // Empty now; a sibling sharing the prefix `d` does not count as a child.
        c.remove(l, "d", false).unwrap();
        assert!(c.get(l, "d").unwrap().is_none());
        assert!(matches!(
            c.remove(l, "", false),
            Err(StorageError::BadRequest(_))
        ));
    }

    /// A directory whose children have their own deep subtrees still lists
    /// only its direct children (the scan skips each child's subtree).
    #[test]
    fn children_skip_deep_subtrees() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        let l = c.create_layer("p").unwrap();
        c.put(l, "root", &dirr("root"), false).unwrap();
        for d in ["a", "b", "c"] {
            c.put(l, &format!("root/{d}"), &dirr(d), false).unwrap();
            for i in 0..50 {
                c.put(l, &format!("root/{d}/x{i}"), &dirr("x"), false)
                    .unwrap();
                c.put(
                    l,
                    &format!("root/{d}/x{i}/deep.ess"),
                    &file("deep.ess", 1),
                    false,
                )
                .unwrap();
            }
        }
        c.put(l, "root/a.txt", &file("a.txt", 2), false).unwrap(); // sorts after "a/..."
        c.put(l, "root/b0", &file("b0", 3), false).unwrap(); // the skip target itself
        c.put(l, "root/orphan/child", &file("child", 4), false)
            .unwrap(); // no "root/orphan" row
        c.put(l, "root/z", &file("z", 5), false).unwrap();
        c.put(l, "root0", &file("root0", 6), false).unwrap(); // past the prefix
        let names: Vec<_> = c
            .children(l, "root")
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, vec!["a", "a.txt", "b", "b0", "c", "z"]);
        let top: Vec<_> = c
            .children(l, "")
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(top, vec!["root", "root0"]);
    }

    #[test]
    fn entry_records_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        let l = c.create_layer("p").unwrap();
        let rec = EntryRec {
            name: "Ünïcode Save.ess".into(),
            kind: vfs_provider::KIND_FILE,
            guid: [3; 16],
            len: u64::MAX,
            mtime: -7,
        };
        c.put(l, "ünïcode save.ess", &rec, true).unwrap();
        drop(c);
        let c = open(&dir);
        assert_eq!(c.get(l, "ünïcode save.ess").unwrap(), Some(rec));
    }

    #[test]
    fn drop_layer_returns_file_guids_and_removes_only_that_layer() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        let a = c.create_layer("a").unwrap();
        let b = c.create_layer("b").unwrap();
        c.put(a, "d", &dirr("d"), false).unwrap();
        c.put(a, "d/f", &file("f", 1), false).unwrap();
        c.put(a, "g", &file("g", 2), false).unwrap();
        c.put(b, "h", &file("h", 3), false).unwrap();
        let mut guids = c.drop_layer(a).unwrap();
        guids.sort();
        assert_eq!(guids, vec![[1; 16], [2; 16]]);
        assert_eq!(c.layer_id("a").unwrap(), None);
        assert!(c.get(a, "").unwrap().is_none() && c.get(a, "g").unwrap().is_none());
        assert_eq!(c.layer_names().unwrap(), vec![("b".to_string(), b)]);
        assert_eq!(
            c.all_layer_guids().unwrap(),
            vec![(b, "h".to_string(), [3; 16])]
        );
        assert!(matches!(c.drop_layer(a), Err(StorageError::NoSuchLayer(_))));
        // Ids are never reused.
        assert!(c.create_layer("a").unwrap() > b);
    }

    #[test]
    fn rename_moves_a_subtree_onto_an_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        let l = c.create_layer("p").unwrap();
        c.put(l, "a", &dirr("a"), false).unwrap();
        c.put(l, "a/x", &file("x", 1), false).unwrap();
        c.put(l, "a/sub", &dirr("sub"), false).unwrap();
        c.put(l, "a/sub/y", &file("y", 2), false).unwrap();
        c.put(l, "b", &dirr("b"), false).unwrap();
        c.put(l, "ab", &file("ab", 4), false).unwrap(); // shares the prefix "a", not "a/"
        assert_eq!(c.rename(l, "a", "b", "B").unwrap(), Vec::<Guid>::new());
        assert_eq!(c.get(l, "b/x").unwrap().unwrap().guid, [1; 16]);
        assert_eq!(c.get(l, "b/sub/y").unwrap().unwrap().guid, [2; 16]);
        assert_eq!(c.get(l, "ab").unwrap().unwrap().guid, [4; 16]);
        let mut names: Vec<_> = c
            .children(l, "")
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["B", "ab"]);
    }

    /// The replaced file's GUID comes back from the same transaction, so the
    /// caller can delete its store data without a racy get-then-rename.
    #[test]
    fn rename_over_a_file_returns_its_guid() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        let l = c.create_layer("p").unwrap();
        c.put(l, "new.ess", &file("new.ess", 1), false).unwrap();
        c.put(l, "save.ess", &file("Save.ess", 2), false).unwrap();
        assert_eq!(
            c.rename(l, "new.ess", "save.ess", "Save.ess").unwrap(),
            vec![[2; 16]]
        );
        assert!(c.get(l, "new.ess").unwrap().is_none());
        let got = c.get(l, "save.ess").unwrap().unwrap();
        assert_eq!((got.name.as_str(), got.guid), ("Save.ess", [1; 16]));
    }

    #[test]
    fn rename_refuses_a_destination_directory_with_children() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        let l = c.create_layer("p").unwrap();
        c.put(l, "a", &dirr("a"), false).unwrap();
        c.put(l, "a/x", &file("x", 1), false).unwrap();
        c.put(l, "b", &dirr("b"), false).unwrap();
        c.put(l, "b/keep", &file("keep", 9), false).unwrap();
        c.put(l, "f", &file("f", 3), false).unwrap();
        for from in ["a", "f"] {
            let err = c.rename(l, from, "b", "b").unwrap_err();
            assert!(matches!(err, StorageError::Exists(_)), "{err}");
            assert_eq!(err.to_status(), vfs_provider::ST_EXISTS);
        }
        // Refused inside the transaction: nothing moved, nothing was replaced.
        assert_eq!(c.get(l, "b/keep").unwrap().unwrap().guid, [9; 16]);
        assert_eq!(c.get(l, "a/x").unwrap().unwrap().guid, [1; 16]);
        assert_eq!(c.get(l, "f").unwrap().unwrap().guid, [3; 16]);
        // A directory onto its own parent is the same refusal: the parent holds it.
        c.put(l, "a/inner", &dirr("inner"), false).unwrap();
        assert!(matches!(
            c.rename(l, "a/inner", "a", "a"),
            Err(StorageError::Exists(_))
        ));
    }

    #[test]
    fn a_case_only_rename_keeps_the_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        let l = c.create_layer("p").unwrap();
        c.put(l, "a", &dirr("a"), false).unwrap();
        c.put(l, "a/x", &file("x", 1), false).unwrap();
        assert!(c.rename(l, "a", "A", "A").unwrap().is_empty());
        assert_eq!(c.get(l, "a").unwrap().unwrap().name, "A");
        assert_eq!(c.get(l, "a/x").unwrap().unwrap().guid, [1; 16]);
    }

    #[test]
    fn rename_refuses_a_missing_source_and_its_own_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        let l = c.create_layer("p").unwrap();
        c.put(l, "a", &dirr("a"), false).unwrap();
        let missing = c.rename(l, "nope", "b", "b").unwrap_err();
        assert!(matches!(missing, StorageError::NotFound(_)), "{missing}");
        assert_eq!(missing.to_status(), vfs_provider::ST_NOT_FOUND);
        let into_self = c.rename(l, "a", "a/b", "b").unwrap_err();
        assert!(
            matches!(into_self, StorageError::BadRequest(_)),
            "{into_self}"
        );
        assert_eq!(into_self.to_status(), vfs_provider::ST_BAD_REQUEST);
        assert!(matches!(
            c.rename(l, "", "b", "b"),
            Err(StorageError::BadRequest(_))
        ));
        assert_eq!(c.get(l, "a").unwrap().unwrap().name, "a");
    }

    #[test]
    fn cache_rows_round_trip_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(&dir);
        let r = |m, b| CacheRec {
            last_access_min: m,
            logical_bytes: b,
        };
        c.cache_put_many(&[([1; 16], r(10, 100)), ([2; 16], r(20, 200))])
            .unwrap();
        c.cache_put_many(&[([1; 16], r(11, 100))]).unwrap();
        assert_eq!(c.cache_get(&[1; 16]).unwrap(), Some(r(11, 100)));
        c.cache_remove(&[2; 16]).unwrap();
        assert_eq!(c.cache_get(&[2; 16]).unwrap(), None);
        c.commit_durable().unwrap();
        drop(c);
        let c = open(&dir);
        assert_eq!(c.cache_all().unwrap(), vec![([1; 16], r(11, 100))]);
    }
}
