//! A layer file's shared state and its block read-modify-write.
//!
//! Every handle open on one GUID shares one [`FileCell`]. Its [`FileState`]
//! holds the file's live length and a small buffer of dirty blocks; reads see
//! the dirty buffer first, so a second handle sees a first handle's writes
//! before either closes. [`FileCell::commit`] turns the buffer into whole-block
//! `write_blocks` calls.
//!
//! **Locking.** A cell's `state` lock is taken before the layer's namespace
//! lock, never after it (the layer updates the catalog row while it still
//! holds the state lock after a commit). Everything the namespace side needs
//! from a cell without taking `state` is mirrored outside it: `live_len`,
//! `path`, `opens`.
//!
//! **RAM tier.** A layer file's blocks enter the RAM tier only from this module
//! and only under the cell's `state` lock — a read's fill and a commit's
//! invalidate-and-put are serialised, so a reader cannot put a block it read
//! before a commit back in after it (the stale-refill race documented on
//! [`crate::RamTier::invalidate_file`]). A commit also puts the blocks it wrote,
//! so the next read of a just-written block is a RAM hit.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use vfs_provider::{bad_request, map_io_err, ST_NO_SPACE};

use crate::ids::{layer_file_id, Guid};
use crate::storage::Storage;

/// Blocks per `write_blocks` call when a commit writes a long run.
const RUN_BLOCKS: u64 = 64;

/// The largest layer file (1 PiB): far past anything a game writes, well
/// inside what the block store's manifest can address at any block size, and
/// small enough that block arithmetic cannot overflow.
///
/// It bounds correctness, not cost: growing a file commits every block of the
/// gap as an explicit zero block (spec §5, so a layer file never has a missing
/// block). Each one dedups to the same stored block, but each is still hashed
/// and compressed, so a grow of many GiB is slow and one near `MAX_LEN` would
/// effectively never finish. Games do not do this; a sparse-extent encoding
/// would be the fix if something ever does.
const MAX_LEN: u64 = 1 << 50;

/// The mutable part of a layer file, behind [`FileCell::state`].
pub(crate) struct FileState {
    /// The file's length as handles see it.
    pub len: u64,
    /// The length the block store holds. `len >= committed_len` except
    /// transiently inside `set_len`, which commits before it returns.
    pub committed_len: u64,
    /// The lowest length the file was truncated to since the last successful
    /// commit. Committed bytes at or past it are gone as far as handles are
    /// concerned, even while the store still holds them (a shrink whose
    /// commit failed): they read as zeros and are never loaded back.
    pub valid_len: u64,
    /// Block index → the block's bytes, possibly shorter than the block (the
    /// rest reads as zeros, up to `len`).
    pub dirty: BTreeMap<u64, Vec<u8>>,
}

impl FileState {
    /// Whether a commit has anything to do.
    pub fn is_dirty(&self) -> bool {
        !self.dirty.is_empty()
            || self.len != self.committed_len
            || self.valid_len < self.committed_len
    }

    /// How much of the store's copy is still the file's content.
    fn committed_valid(&self) -> u64 {
        self.committed_len.min(self.valid_len)
    }
}

/// One layer file, shared by every handle open on its GUID.
pub(crate) struct FileCell {
    pub guid: Guid,
    pub id: [u8; 17],
    pub state: Mutex<FileState>,
    /// `state.len`, readable without the state lock (getattr, readdir).
    pub live_len: AtomicU64,
    /// The folded path of the file's catalog row; `None` once the row is gone
    /// (removed, or replaced by a rename). Written under the namespace lock.
    pub path: Mutex<Option<String>>,
    /// Open handles, plus in-flight path operations (`set_attr`). Changed
    /// only under the namespace lock.
    pub opens: AtomicUsize,
    /// An mtime set through `set_attr` while the file was open; later commits
    /// keep it rather than stamping the current time.
    pub mtime_override: Mutex<Option<i64>>,
    /// Test hook: the next commit fails before it touches the store.
    #[cfg(test)]
    pub fail_commit: std::sync::atomic::AtomicBool,
}

/// The bytes of block `b` of a file of `len` bytes that the store holds.
fn block_len(len: u64, bs: u64, b: u64) -> usize {
    (len - b * bs).min(bs) as usize
}

fn block_count(len: u64, bs: u64) -> u64 {
    len.div_ceil(bs)
}

/// Copies `src[skip..]` into `dst`, zero-filling whatever `src` is too short
/// to cover.
fn copy_padded(src: &[u8], skip: usize, dst: &mut [u8]) {
    let avail = src.len().saturating_sub(skip).min(dst.len());
    dst[..avail].copy_from_slice(&src[skip..skip + avail]);
    dst[avail..].fill(0);
}

impl FileCell {
    pub fn new(guid: Guid, path: String, len: u64) -> Self {
        FileCell {
            guid,
            id: layer_file_id(&guid),
            state: Mutex::new(FileState {
                len,
                committed_len: len,
                valid_len: len,
                dirty: BTreeMap::new(),
            }),
            live_len: AtomicU64::new(len),
            path: Mutex::new(Some(path)),
            opens: AtomicUsize::new(0),
            mtime_override: Mutex::new(None),
            #[cfg(test)]
            fail_commit: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn path_for_log(&self) -> String {
        match self.path.lock() {
            Ok(p) => p.clone().unwrap_or_else(|| "<removed>".into()),
            Err(_) => "<unknown>".into(),
        }
    }

    fn set_len_field(&self, st: &mut FileState, len: u64) {
        st.len = len;
        self.live_len.store(len, Ordering::Release);
    }

    /// The committed bytes of block `b` (below `st.committed_len`): from the
    /// RAM tier, else the store, filling the RAM tier. A block the store
    /// reports missing is corruption — a layer file has no source to refetch
    /// from — and is `ST_IO_ERROR`, never zeros.
    fn committed_block(
        &self,
        s: &Storage,
        layer: &str,
        st: &FileState,
        b: u64,
    ) -> Result<Arc<[u8]>, i32> {
        if let Some(hit) = s.ram.get(&self.id, b) {
            return Ok(hit);
        }
        let bs = s.block_size();
        let want = block_len(st.committed_len, bs, b);
        let mut buf = vec![0u8; want];
        match s.store.read(&self.id, b * bs, &mut buf) {
            Ok(r) if r.missing.is_empty() && r.bytes == want => {
                let data: Arc<[u8]> = Arc::from(buf);
                s.ram.put(&self.id, b, Arc::clone(&data));
                Ok(data)
            }
            Ok(r) => {
                tracing::error!(
                    layer,
                    path = %self.path_for_log(),
                    block = b,
                    got = r.bytes,
                    want,
                    "layer block missing: corruption"
                );
                Err(map_io_err())
            }
            Err(e) => {
                tracing::error!(
                    layer,
                    path = %self.path_for_log(),
                    block = b,
                    error = %e,
                    "layer block missing: corruption"
                );
                Err(map_io_err())
            }
        }
    }

    /// Positional read against the live state: dirty blocks, then committed
    /// blocks, then zeros between `committed_len` and `len`.
    pub fn read(
        &self,
        s: &Storage,
        layer: &str,
        st: &FileState,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, i32> {
        if offset >= st.len || buf.is_empty() {
            return Ok(0);
        }
        let bs = s.block_size();
        let end = st.len.min(offset.saturating_add(buf.len() as u64));
        let mut pos = offset;
        while pos < end {
            let b = pos / bs;
            let skip = (pos - b * bs) as usize;
            let to = end.min((b + 1) * bs);
            let dst = &mut buf[(pos - offset) as usize..(to - offset) as usize];
            if let Some(d) = st.dirty.get(&b) {
                copy_padded(d, skip, dst);
            } else if b * bs < st.committed_valid() {
                let c = self.committed_block(s, layer, st, b)?;
                let valid = ((st.committed_valid() - b * bs) as usize).min(c.len());
                copy_padded(&c[..valid], skip, dst);
            } else {
                dst.fill(0);
            }
            pos = to;
        }
        Ok((end - offset) as usize)
    }

    /// Loads block `b` into the dirty buffer if it is not there already.
    fn make_dirty(&self, s: &Storage, layer: &str, st: &mut FileState, b: u64) -> Result<(), i32> {
        if !st.dirty.contains_key(&b) {
            let start = b * s.block_size();
            let v = if start < st.committed_valid() {
                let c = self.committed_block(s, layer, st, b)?;
                let valid = ((st.committed_valid() - start) as usize).min(c.len());
                c[..valid].to_vec()
            } else {
                Vec::new()
            };
            st.dirty.insert(b, v);
        }
        Ok(())
    }

    /// Patches `data` in at `offset`, block by block, into the dirty buffer.
    pub fn write(
        &self,
        s: &Storage,
        layer: &str,
        st: &mut FileState,
        offset: u64,
        data: &[u8],
    ) -> Result<(), i32> {
        if data.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(bad_request)?;
        if end > MAX_LEN {
            return Err(ST_NO_SPACE);
        }
        let bs = s.block_size();
        let mut pos = offset;
        while pos < end {
            let b = pos / bs;
            let from = (pos - b * bs) as usize;
            let to_abs = end.min((b + 1) * bs);
            let to = (to_abs - b * bs) as usize;
            self.make_dirty(s, layer, st, b)?;
            let blk = st.dirty.get_mut(&b).expect("just made dirty");
            if blk.len() < to {
                blk.resize(to, 0);
            }
            blk[from..to]
                .copy_from_slice(&data[(pos - offset) as usize..(to_abs - offset) as usize]);
            pos = to_abs;
        }
        if end > st.len {
            self.set_len_field(st, end);
        }
        Ok(())
    }

    /// Sets the live length: drops dirty blocks past the new end and, on a
    /// shrink, makes the new tail block dirty and truncated so the store's
    /// copy (which `set_len` will drop) is not needed again. The caller
    /// commits.
    pub fn truncate(
        &self,
        s: &Storage,
        layer: &str,
        st: &mut FileState,
        len: u64,
    ) -> Result<(), i32> {
        if len > MAX_LEN {
            return Err(ST_NO_SPACE);
        }
        let bs = s.block_size();
        if len < st.len {
            let _ = st.dirty.split_off(&block_count(len, bs));
            if !len.is_multiple_of(bs) {
                let tail = len / bs;
                let keep = (len - tail * bs) as usize;
                if st.dirty.contains_key(&tail) || tail * bs < st.committed_valid() {
                    self.make_dirty(s, layer, st, tail)?;
                    st.dirty
                        .get_mut(&tail)
                        .expect("just made dirty")
                        .truncate(keep);
                }
            }
        }
        st.valid_len = st.valid_len.min(len);
        self.set_len_field(st, len);
        Ok(())
    }

    /// Writes the dirty buffer, and any length change, to the store. Returns
    /// whether anything changed (so the caller updates the catalog row).
    ///
    /// On a length change the store drops a block whose length changes (the
    /// old short tail on a grow, the new tail on a shrink); that block is
    /// captured into the dirty buffer first. Then every block the store now
    /// lacks — the captured tail and the zero blocks of a grown gap — is
    /// written along with the dirty blocks, so a layer file never has a
    /// missing block below its length. On failure the dirty buffer is kept, so
    /// a later commit retries it.
    pub fn commit(&self, s: &Storage, layer: &str, st: &mut FileState) -> Result<bool, i32> {
        if !st.is_dirty() {
            return Ok(false);
        }
        #[cfg(test)]
        if self.fail_commit.swap(false, Ordering::SeqCst) {
            return Err(map_io_err());
        }
        let bs = s.block_size();
        let old = st.committed_len;
        // The store's content that is still the file's: below `valid`, the
        // store copy is used; from it on, every block below `new` is written.
        let valid = st.committed_valid();
        let new = st.len;
        let store_err = |what: &str, e: crate::StorageError| {
            tracing::error!(layer, path = %self.path_for_log(), error = %e, "layer {what} failed");
            map_io_err()
        };

        // The block holding `cut` changes length in the store (or holds bytes
        // past `valid` that must go): capture its valid part first.
        let cut = valid.min(new);
        if (new != old || valid < old) && !cut.is_multiple_of(bs) && (cut / bs) * bs < valid {
            self.make_dirty(s, layer, st, cut / bs)?;
        }
        if new != old {
            s.store
                .set_len(&self.id, new)
                .map_err(|e| store_err("set_len", e.into()))?;
        }

        let nb = block_count(new, bs);
        let _ = st.dirty.split_off(&nb);
        // Blocks the store lacks after a length change, beyond the dirty ones.
        let gap = if new != old || valid < old {
            (cut / bs)..nb
        } else {
            0..0
        };
        let before: Vec<u64> = st.dirty.range(..gap.start).map(|(b, _)| *b).collect();
        let after: Vec<u64> = st
            .dirty
            .range(gap.end.max(gap.start)..)
            .map(|(b, _)| *b)
            .collect();
        let order = before.into_iter().chain(gap.clone()).chain(after);

        let zeros = vec![0u8; bs as usize];
        let mut run: Vec<u8> = Vec::new();
        let mut run_first = 0u64;
        let mut run_count = 0u64;
        let flush_run = |run: &mut Vec<u8>, first: u64, count: &mut u64| -> Result<(), i32> {
            if *count > 0 {
                s.store
                    .write_blocks(&self.id, first, run)
                    .map_err(|e| store_err("write_blocks", e.into()))?;
                run.clear();
                *count = 0;
            }
            Ok(())
        };
        for b in order {
            if run_count > 0 && (b != run_first + run_count || run_count == RUN_BLOCKS) {
                flush_run(&mut run, run_first, &mut run_count)?;
            }
            if run_count == 0 {
                run_first = b;
            }
            let want = block_len(new, bs, b);
            let start = run.len();
            run.resize(start + want, 0);
            copy_padded(
                st.dirty.get(&b).map_or(&zeros[..], |d| &d[..]),
                0,
                &mut run[start..],
            );
            run_count += 1;
        }
        flush_run(&mut run, run_first, &mut run_count)?;

        s.ram.invalidate_file(&self.id);
        for (b, mut d) in std::mem::take(&mut st.dirty) {
            d.resize(block_len(new, bs, b), 0);
            s.ram.put(&self.id, b, Arc::from(d));
        }
        st.committed_len = new;
        st.valid_len = new;
        Ok(true)
    }
}
