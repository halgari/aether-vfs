//! Write path: validation, dedup, parallel compression, append, index commit.

use std::collections::{HashMap, HashSet};

use rayon::prelude::*;

use crate::class::WriteClass;
use crate::codec::{EncodedBlock, HEADER_LEN, Hash128, hash128};
use crate::error::{Error, Result};
use crate::index::{BlockLoc, PackState, Tables};
use crate::manifest::{MISSING, block_count, block_len};
use crate::store::BlockStore;
use crate::{crash, files};

/// Checks that `data_len` bytes written at block `first` are block-aligned for a file of `len` bytes.
pub(crate) fn validate_write(len: u64, block_size: u32, first: u64, data_len: usize) -> Result<()> {
    if data_len == 0 {
        return Ok(());
    }
    let bs = block_size as u64;
    let count = (data_len as u64).div_ceil(bs);
    let end = first.checked_add(count).ok_or(Error::OutOfRange)?;
    if end > block_count(len, block_size) {
        return Err(Error::OutOfRange);
    }
    let tail = data_len as u64 - (count - 1) * bs;
    if tail != block_len(len, block_size, end - 1) {
        return Err(Error::Unaligned);
    }
    Ok(())
}

/// True if records in `pack` may still be committed: the pack is registered and not retired.
pub(crate) fn pack_accepts_records(t: &Tables<'_>, pack: u32) -> Result<bool> {
    Ok(t.pack(pack)?.is_some_and(|p| p.state != PackState::Retired))
}

impl BlockStore {
    /// Writes whole blocks starting at block index `first_block`. Every block must be exactly
    /// `block_size` bytes, except the file's final block, which must be exactly its remaining length.
    /// The file must exist (see [`BlockStore::set_len`]). Large writes are committed in chunks of
    /// about `write_txn_bytes`; if a later chunk fails the error is [`Error::PartialWrite`].
    ///
    /// New blocks are compressed as this thread's [`WriteClass::current`] says (see
    /// [`crate::with_write_class`]).
    pub fn write_blocks(&self, file_id: &[u8], first_block: u64, data: &[u8]) -> Result<()> {
        self.write_blocks_as(file_id, first_block, data, WriteClass::current())
    }

    /// [`BlockStore::write_blocks`] with an explicit write class.
    pub fn write_blocks_as(
        &self,
        file_id: &[u8],
        first_block: u64,
        data: &[u8],
        class: WriteClass,
    ) -> Result<()> {
        self.check_id(file_id)?;
        let bs = self.cfg.block_size;
        let len = self.stat(file_id)?.ok_or(Error::NotFound)?.len;
        validate_write(len, bs, first_block, data.len())?;

        let chunk_bytes = (self.cfg.write_txn_bytes / bs as usize).max(1) * bs as usize;
        let mut done = 0u64;
        for chunk in data.chunks(chunk_bytes) {
            let result = self
                .write_chunk(file_id, first_block + done, chunk, class)
                .map(|()| {
                    self.codec.wrote(class, chunk.len() as u64);
                    done += chunk.len().div_ceil(bs as usize) as u64
                })
                .and_then(|()| self.maybe_auto_flush());
            if let Err(e) = result {
                return Err(if done == 0 {
                    e
                } else {
                    Error::PartialWrite {
                        blocks_written: done,
                        source: Box::new(e),
                    }
                });
            }
        }
        Ok(())
    }

    /// Writes one chunk in one index transaction.
    fn write_chunk(
        &self,
        file_id: &[u8],
        first: u64,
        chunk: &[u8],
        class: WriteClass,
    ) -> Result<()> {
        let bs = self.cfg.block_size;
        let blocks: Vec<&[u8]> = chunk.chunks(bs as usize).collect();
        let hashes: Vec<Hash128> = self.install(|| blocks.par_iter().map(|b| hash128(b)).collect());

        // Blocks whose content is not stored yet.
        let mut need: Vec<usize> = {
            let r = self.index.read()?;
            let mut seen = HashSet::new();
            let mut need = Vec::new();
            for (i, h) in hashes.iter().enumerate() {
                if seen.insert(*h) && r.dedup(h)?.is_none() {
                    need.push(i);
                }
            }
            need
        };

        let mut new_records: HashMap<Hash128, BlockLoc> = HashMap::new();
        loop {
            self.encode_and_append(&blocks, &hashes, &need, &mut new_records, class)?;
            crash::point("write_after_append");
            #[cfg(test)]
            {
                let hook = self.hooks.before_write_commit.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook(self);
                }
            }
            let retry = self.commit(|t| {
                let len = files::len(t, file_id)?.ok_or(Error::NotFound)?;
                validate_write(len, bs, first, chunk.len())?;

                // A dedup hit seen earlier may have been freed by another commit since.
                // A new record whose pack was retired by a concurrent compaction must be appended again.
                // Check before modifying anything; committing an untouched transaction is harmless.
                let mut missing = Vec::new();
                for (i, h) in hashes.iter().enumerate() {
                    if t.dedup(h)?.is_some() {
                        continue;
                    }
                    match new_records.get(h) {
                        Some(loc) if pack_accepts_records(t, loc.pack)? => {}
                        _ => missing.push(i),
                    }
                }
                if !missing.is_empty() {
                    return Ok(Some(missing));
                }

                let mut ids = Vec::with_capacity(hashes.len());
                for h in &hashes {
                    let id = match t.dedup(h)? {
                        Some(id) => id,
                        None => t.insert_block(new_records[h])?,
                    };
                    ids.push(id);
                }
                let old = files::set_slots(t, file_id, len, first, &ids)?;
                // Increment before decrementing so rewriting a slot with the same block keeps it.
                for &id in &ids {
                    t.incref(id)?;
                }
                for id in old {
                    if id != MISSING {
                        t.decref(id)?;
                    }
                }
                Ok(None)
            })?;
            match retry {
                None => return Ok(()),
                Some(missing) => {
                    for &i in &missing {
                        new_records.remove(&hashes[i]);
                    }
                    need = missing;
                }
            }
        }
    }

    /// Compresses the blocks at `need` (skipping hashes already in `out`) and appends them.
    fn encode_and_append(
        &self,
        blocks: &[&[u8]],
        hashes: &[Hash128],
        need: &[usize],
        out: &mut HashMap<Hash128, BlockLoc>,
        class: WriteClass,
    ) -> Result<()> {
        let mut seen = HashSet::new();
        let todo: Vec<usize> = need
            .iter()
            .copied()
            .filter(|&i| !out.contains_key(&hashes[i]) && seen.insert(hashes[i]))
            .collect();
        if todo.is_empty() {
            return Ok(());
        }
        // Compressed before anything is appended or recorded, whichever compressor runs.
        let todo_blocks: Vec<&[u8]> = todo.iter().map(|&i| blocks[i]).collect();
        let todo_hashes: Vec<Hash128> = todo.iter().map(|&i| hashes[i]).collect();
        let encoded: Vec<EncodedBlock> =
            self.codec
                .encode(self.pool.as_ref(), &todo_blocks, &todo_hashes, class)?;
        let headers: Vec<[u8; HEADER_LEN]> = encoded.iter().map(|e| e.header.encode()).collect();
        let locs = self.append_records(
            headers
                .iter()
                .zip(&encoded)
                .map(|(h, e)| (h.as_slice(), e.payload.as_slice())),
        )?;
        for (e, (pack, offset)) in encoded.iter().zip(locs) {
            out.insert(
                e.header.hash,
                BlockLoc {
                    pack,
                    offset,
                    stored_len: e.header.stored_len,
                    raw_len: e.header.raw_len,
                    refcount: 0,
                    hash: e.header.hash,
                },
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{pack_accepts_records, validate_write};
    use crate::codec::hash128;
    use crate::config::CompactOptions;
    use crate::error::Error;
    use crate::index::{Index, PackInfo, PackState};
    use crate::pack::pack_path;
    use crate::store::BlockStore;
    use crate::store::tests::{BS, put, random_bytes, read_all, test_config};

    #[test]
    fn validation() {
        let bs = 4096;
        // 2.5 blocks
        let len = 2 * 4096 + 2048;
        assert!(validate_write(len, bs, 0, 4096).is_ok());
        assert!(validate_write(len, bs, 0, 2 * 4096 + 2048).is_ok());
        assert!(validate_write(len, bs, 2, 2048).is_ok());
        assert!(validate_write(len, bs, 1, 0).is_ok());
        assert!(matches!(
            validate_write(len, bs, 0, 100),
            Err(Error::Unaligned)
        ));
        assert!(matches!(
            validate_write(len, bs, 2, 4096),
            Err(Error::Unaligned)
        ));
        assert!(matches!(
            validate_write(len, bs, 2, 2047),
            Err(Error::Unaligned)
        ));
        assert!(matches!(
            validate_write(len, bs, 3, 1),
            Err(Error::OutOfRange)
        ));
        assert!(matches!(
            validate_write(len, bs, 2, 4096 + 2048),
            Err(Error::OutOfRange)
        ));
        assert!(matches!(
            validate_write(len, bs, u64::MAX, 1),
            Err(Error::OutOfRange)
        ));
    }

    #[test]
    fn retries_when_a_dedup_hit_is_freed_before_the_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::open(dir.path(), test_config()).unwrap();
        let d = random_bytes(1, 2 * BS);
        put(&store, b"x", &d).unwrap();
        store.flush().unwrap();
        store.set_len(b"y", d.len() as u64).unwrap();
        let h = hash128(&d[..BS]);
        // Every block of "y" is a dedup hit on "x"; free them all between append and commit.
        *store.hooks.before_write_commit.lock().unwrap() = Some(Box::new(move |s| {
            s.delete(b"x").unwrap();
            assert!(s.index.read().unwrap().dedup(&h).unwrap().is_none());
        }));
        store.write_blocks(b"y", 0, &d).unwrap();
        assert!(store.hooks.before_write_commit.lock().unwrap().is_none());
        // The retry had to append the blocks again.
        assert!(store.stats().unwrap().unflushed_bytes > 0);
        assert_eq!(read_all(&store, b"y"), d);
        assert!(store.verify().unwrap().is_ok());
        store.close().unwrap();
        let store = BlockStore::open(dir.path(), test_config()).unwrap();
        assert_eq!(read_all(&store, b"y"), d);
        assert!(store.verify().unwrap().is_ok());
    }

    #[test]
    fn retries_when_a_new_record_pack_is_retired_before_the_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::open(dir.path(), test_config()).unwrap();
        let n = random_bytes(2, BS);
        let z = random_bytes(3, 20 * BS);
        store.set_len(b"y", n.len() as u64).unwrap();
        let z2 = z.clone();
        // Between append and commit, fill the pack holding the new record, then compact it away.
        *store.hooks.before_write_commit.lock().unwrap() = Some(Box::new(move |s| {
            let p = s.writer.lock().unwrap().packs.active_id().unwrap();
            put(s, b"z", &z2).unwrap();
            let all = CompactOptions {
                min_garbage_ratio: 0.0,
                max_bytes: u64::MAX,
            };
            s.compact(all).unwrap();
            assert!(!s.stats().unwrap().packs.iter().any(|x| x.id == p));
            assert!(!pack_path(&s.pack_dir, p).exists());
        }));
        store.write_blocks(b"y", 0, &n).unwrap();
        assert!(store.hooks.before_write_commit.lock().unwrap().is_none());
        assert_eq!(read_all(&store, b"y"), n);
        assert_eq!(read_all(&store, b"z"), z);
        assert!(store.verify().unwrap().is_ok());
        store.close().unwrap();
        let store = BlockStore::open(dir.path(), test_config()).unwrap();
        assert_eq!(read_all(&store, b"y"), n);
        assert!(store.verify().unwrap().is_ok());
    }

    #[test]
    fn pack_accepts_records_only_for_live_packs() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("i.redb"), 1 << 20).unwrap();
        index
            .update(false, |t| {
                t.put_pack(
                    1,
                    &PackInfo {
                        live_bytes: 100,
                        state: PackState::Active,
                    },
                )?;
                t.put_pack(
                    2,
                    &PackInfo {
                        live_bytes: 100,
                        state: PackState::Sealed,
                    },
                )?;
                t.put_pack(
                    3,
                    &PackInfo {
                        live_bytes: 0,
                        state: PackState::Retired,
                    },
                )?;
                Ok(())
            })
            .unwrap();
        index
            .update(false, |t| {
                assert!(pack_accepts_records(t, 1).unwrap());
                assert!(pack_accepts_records(t, 2).unwrap());
                assert!(!pack_accepts_records(t, 3).unwrap());
                assert!(!pack_accepts_records(t, 4).unwrap());
                Ok(())
            })
            .unwrap();
    }
}
