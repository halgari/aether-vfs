//! A layer file's shared state and its block read-modify-write.
//!
//! Every handle open on one GUID shares one [`FileCell`]. Its [`FileState`]
//! holds the file's live length and a small buffer of dirty blocks; reads see
//! the dirty buffer first, so a second handle sees a first handle's writes
//! before either closes. [`FileCell::commit`] turns the buffer into whole-block
//! `write_blocks` calls.
//!
//! **Locking.** A cell's `state` is a reader-writer lock. A read
//! ([`FileCell::read`], which takes `&FileState`) holds it **shared**, so any
//! number of reads of one file — on one handle or many — run at once; a game
//! issues eight 1 MiB reads of one plugin together, and a mutex here made
//! them finish one after another. Everything that changes the state — a
//! write, a truncate, a commit, a close's commit — holds it **exclusive**, so
//! a read sees the state before or after such a change, never part of it.
//! A shared holder takes nothing but leaf locks (the RAM tier's shards, the
//! block store's read path, a cell's `path` for a log line), never `gate` or
//! `ns`, so sharing it adds no lock-order edge.
//!
//! **Writers and a steady stream of reads.** A save's `write_at` or `close`
//! must not wait forever behind a game that keeps reading the file. The lock
//! is `std::sync::RwLock`, whose documentation leaves the priority policy to
//! the platform. The implementation std uses on Linux stops admitting new
//! readers once a writer is waiting, so a writer waits only for the reads
//! already inside; that is observed behaviour (and what a stress run of
//! eight continuous readers against one writer showed), not a documented
//! guarantee, and it has not been checked on other platforms. If std ever
//! changes it, replace the lock with one that documents writer preference
//! rather than rely on this note.
//!
//! The `state` lock is taken before the layer's namespace lock, never after
//! it (the layer updates the catalog row while it still holds the state lock
//! after a commit). Everything the namespace side needs from a cell without
//! taking `state` is mirrored outside it: `live_len`, `path`, `opens`. A
//! commit runs under the storage's durability gate, taken after `state` and
//! before `ns` (the full order is on [`Storage::gate`]).
//!
//! **RAM tier.** A layer file's blocks enter the RAM tier only from this module
//! and only under the cell's `state` lock — shared for a read's fill,
//! exclusive for a commit's invalidate-and-put. The two exclude each other, so
//! a reader cannot put a block it read before a commit back in after it (the
//! stale-refill race documented on [`crate::RamTier::invalidate_file`]): the
//! commit cannot start until every read that filled from the old store copy
//! has finished its fill. Two concurrent readers may fill the same block:
//! each decodes it, both hold the same committed bytes, and
//! [`crate::RamTier::fill`] keeps the first.
//! A commit also puts the blocks it wrote, so the next read of a just-written
//! block is a RAM hit.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use vfs_provider::{bad_request, map_io_err, ST_NO_SPACE};

use crate::ids::{layer_file_id, Guid};
use crate::storage::Storage;

/// Blocks per `write_blocks` call when a commit writes a long run.
const RUN_BLOCKS: u64 = 64;

/// [`RUN_BLOCKS`] for a bulk write ([`vfs_block_store::WriteClass::Bulk`]):
/// its blocks may be compressed on the GPU, where each call waits for a
/// batch, so a commit hands over more at once (16 MiB of 64 KiB blocks).
const BULK_RUN_BLOCKS: u64 = 256;

/// The most store blocks a commit reads back before a resize so it can put
/// them back if a later block write fails (see [`FileCell::commit`]).
pub(crate) const MAX_CAPTURE_BLOCKS: u64 = 64;

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
    /// Shared for reads, exclusive for every change (see the module docs).
    pub state: RwLock<FileState>,
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
    /// Test hook: the next commit that resizes the store fails at its first
    /// block write after the resize (a disk-full `write_blocks`).
    #[cfg(test)]
    pub fail_after_set_len: std::sync::atomic::AtomicBool,
    /// Test hook: the next commit that resizes the store flushes the store
    /// right before its first block write after the resize (what the block
    /// store's auto-flush can do there).
    #[cfg(test)]
    pub flush_after_set_len: std::sync::atomic::AtomicBool,
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
            state: RwLock::new(FileState {
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
            #[cfg(test)]
            fail_after_set_len: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            flush_after_set_len: std::sync::atomic::AtomicBool::new(false),
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
        // Decoded straight into the allocation the RAM tier keeps: a `Vec`
        // turned into an `Arc<[u8]>` afterwards is a second allocation and a
        // copy of the whole block.
        let mut data = crate::ram::zeroed_block(want);
        let buf = Arc::get_mut(&mut data).expect("a new block has one owner");
        match s.store.read(&self.id, b * bs, buf) {
            Ok(r) if r.missing.is_empty() && r.bytes == want => {
                #[cfg(test)]
                {
                    let hook = crate::cached::lock(&s.layer_fill_hook).clone();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                // Another read of this block may have filled it meanwhile
                // (reads share the state lock): the tier keeps that one, the
                // same committed bytes.
                Ok(s.ram.fill(&self.id, b, data))
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
        #[cfg(test)]
        {
            let hook = crate::cached::lock(&s.layer_read_hook).clone();
            if let Some(hook) = hook {
                hook();
            }
        }
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
    /// old short tail on a grow, the new tail on a shrink) and every block
    /// past a shrink's end. The file's own copy of the new tail is captured
    /// into the dirty buffer first; then every block the store now lacks —
    /// that tail and the zero blocks of a grown gap — is written along with
    /// the dirty blocks, so a layer file never has a missing block below its
    /// length.
    ///
    /// **Failure keeps the closed bytes.** The steps are ordered so a
    /// failure never leaves the store's copy — the last closed file, which
    /// the catalog's durable row describes — with a hole:
    ///
    /// 1. dirty blocks below the resized region are written first: they are
    ///    whole blocks at both lengths, so no resize is needed for them;
    /// 2. the store blocks the resize will drop are read back (at most
    ///    [`MAX_CAPTURE_BLOCKS`]; a shrink that would drop more first shrinks
    ///    the store to the block boundary above the new length, which drops
    ///    only bytes the handle has already cut, and needs no capture);
    /// 3. the store is resized, and the tail and gap blocks written;
    /// 4. if a write in step 3 fails (a full disk), the store is resized back
    ///    and the captured blocks rewritten, so it again holds the length and
    ///    bytes it had before step 3.
    ///
    /// On failure the dirty buffer is kept, so a later commit retries it, and
    /// `committed_len` is the length the store holds.
    ///
    /// **Residual crash window.** Step 3's resize and its block writes are
    /// separate store commits. Neither is durable until a store flush, but
    /// the store's own auto-flush (or a compaction) can make the resize
    /// durable before the writes that follow it; a crash in that window
    /// loses the dropped blocks — for a grow, the closed file's last partial
    /// block; for a shrink, its new tail block. Reconciliation at the next
    /// open then zero-fills or reports them (see `reconcile`). Closing the
    /// window needs a store primitive that resizes and writes in one commit.
    pub fn commit(&self, s: &Storage, layer: &str, st: &mut FileState) -> Result<bool, i32> {
        if !st.is_dirty() {
            return Ok(false);
        }
        #[cfg(test)]
        if self.fail_commit.swap(false, Ordering::SeqCst) {
            return Err(map_io_err());
        }
        if let Err(e) = self.commit_blocks(s, layer, st) {
            // The store's copy of some blocks may have changed under the
            // RAM tier's; the dirty buffer still covers every one of them.
            s.ram.invalidate_file(&self.id);
            return Err(e);
        }
        let bs = s.block_size();
        let new = st.len;
        s.ram.invalidate_file(&self.id);
        for (b, mut d) in std::mem::take(&mut st.dirty) {
            d.resize(block_len(new, bs, b), 0);
            s.ram.put(&self.id, b, Arc::from(d));
        }
        st.committed_len = new;
        st.valid_len = new;
        Ok(true)
    }

    /// The store writes of [`Self::commit`], in its failure-safe order.
    fn commit_blocks(&self, s: &Storage, layer: &str, st: &mut FileState) -> Result<(), i32> {
        let bs = s.block_size();
        let old = st.committed_len;
        // The store's content that is still the file's: below `valid`, the
        // store copy is used; from it on, every block below `new` is written.
        let valid = st.committed_valid();
        let new = st.len;
        let rewrite = new != old || valid < old;

        // The block holding `cut` changes length in the store (or holds bytes
        // past `valid` that must go): capture its valid part first.
        let cut = valid.min(new);
        if rewrite && !cut.is_multiple_of(bs) && (cut / bs) * bs < valid {
            self.make_dirty(s, layer, st, cut / bs)?;
        }
        let nb = block_count(new, bs);
        let _ = st.dirty.split_off(&nb);
        if !rewrite {
            // No length change: every dirty block is written in place.
            let all: Vec<u64> = st.dirty.keys().copied().collect();
            return self.write_runs(s, layer, all, new, &st.dirty, false);
        }

        // Every block from the one holding `cut` to the new end is written
        // after the resize; the dirty blocks below it are whole at both
        // lengths, so they go first (step 1).
        let gap = (cut / bs)..nb;
        let below: Vec<u64> = st.dirty.range(..gap.start).map(|(b, _)| *b).collect();
        self.write_runs(s, layer, below, new, &st.dirty, false)?;
        if new == old {
            // Only bytes past `valid` go; nothing is resized or dropped.
            return self.write_runs(s, layer, gap, new, &st.dirty, false);
        }

        // Step 2: what the resize drops, relative to what the store holds.
        let mut cur = old;
        if !gap.is_empty() && new < cur {
            let above = new.div_ceil(bs) * bs;
            if above < cur && block_count(cur, bs) - new / bs > MAX_CAPTURE_BLOCKS {
                s.store
                    .set_len(&self.id, above)
                    .map_err(|e| self.store_err(layer, "set_len", e.into()))?;
                cur = above;
                st.committed_len = above;
            }
        }
        let captured: BTreeMap<u64, Vec<u8>> = if gap.is_empty() {
            // Nothing is written after the resize, so nothing can fail
            // after it.
            BTreeMap::new()
        } else {
            let dropped = if new > cur {
                let tail = cur / bs;
                tail..if cur.is_multiple_of(bs) {
                    tail
                } else {
                    tail + 1
                }
            } else {
                new / bs..block_count(cur, bs)
            };
            let mut out = BTreeMap::new();
            for b in dropped {
                // A block that is already unreadable has nothing to put back.
                if let Ok(d) = self.committed_block(s, layer, st, b) {
                    out.insert(b, d.to_vec());
                }
            }
            out
        };

        // Step 3.
        s.store
            .set_len(&self.id, new)
            .map_err(|e| self.store_err(layer, "set_len", e.into()))?;
        let written = self.write_runs(s, layer, gap, new, &st.dirty, true);
        let Err(e) = written else {
            return Ok(());
        };

        // Step 4.
        let blocks: Vec<u64> = captured.keys().copied().collect();
        let restored = s
            .store
            .set_len(&self.id, cur)
            .map_err(|e| self.store_err(layer, "set_len (restore)", e.into()))
            .and_then(|()| self.write_runs(s, layer, blocks, cur, &captured, false));
        match restored {
            Ok(()) => tracing::warn!(
                layer,
                path = %self.path_for_log(),
                len = cur,
                "layer commit failed after a resize; the store's copy was put back"
            ),
            Err(_) => tracing::error!(
                layer,
                path = %self.path_for_log(),
                len = cur,
                "layer commit failed after a resize and the store's copy could not be \
                 put back: blocks past the new length may be missing until a commit succeeds"
            ),
        }
        Err(e)
    }

    fn store_err(&self, layer: &str, what: &str, e: crate::StorageError) -> i32 {
        tracing::error!(layer, path = %self.path_for_log(), error = %e, "layer {what} failed");
        map_io_err()
    }

    /// Writes `blocks` (ascending) of a file of `len` bytes from `src`, in
    /// runs of consecutive blocks; a block `src` lacks is zeros, and each is
    /// padded or cut to its length at `len`. `after_resize` arms the
    /// `fail_after_set_len` and `flush_after_set_len` test hooks.
    fn write_runs(
        &self,
        s: &Storage,
        layer: &str,
        blocks: impl IntoIterator<Item = u64>,
        len: u64,
        src: &BTreeMap<u64, Vec<u8>>,
        after_resize: bool,
    ) -> Result<(), i32> {
        let _ = after_resize;
        let bs = s.block_size();
        let run_blocks = match vfs_block_store::WriteClass::current() {
            vfs_block_store::WriteClass::Bulk => BULK_RUN_BLOCKS,
            vfs_block_store::WriteClass::Foreground => RUN_BLOCKS,
        };
        let zeros = vec![0u8; bs as usize];
        let mut run: Vec<u8> = Vec::new();
        let mut run_first = 0u64;
        let mut run_count = 0u64;
        let flush_run = |run: &mut Vec<u8>, first: u64, count: &mut u64| -> Result<(), i32> {
            if *count == 0 {
                return Ok(());
            }
            #[cfg(test)]
            if after_resize && self.fail_after_set_len.swap(false, Ordering::SeqCst) {
                return Err(map_io_err());
            }
            #[cfg(test)]
            if after_resize && self.flush_after_set_len.swap(false, Ordering::SeqCst) {
                s.store.flush().map_err(|_| map_io_err())?;
            }
            s.store
                .write_blocks(&self.id, first, run)
                .map_err(|e| self.store_err(layer, "write_blocks", e.into()))?;
            run.clear();
            *count = 0;
            Ok(())
        };
        for b in blocks {
            if run_count > 0 && (b != run_first + run_count || run_count == run_blocks) {
                flush_run(&mut run, run_first, &mut run_count)?;
            }
            if run_count == 0 {
                run_first = b;
            }
            let start = run.len();
            run.resize(start + block_len(len, bs, b), 0);
            copy_padded(
                src.get(&b).map_or(&zeros[..], |d| &d[..]),
                0,
                &mut run[start..],
            );
            run_count += 1;
        }
        flush_run(&mut run, run_first, &mut run_count)
    }
}
