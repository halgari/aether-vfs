//! Write path: validation, dedup, parallel compression, append, index commit.

use std::collections::{HashMap, HashSet};

use rayon::prelude::*;

use crate::codec::{EncodedBlock, HEADER_LEN, Hash128, encode_block, hash128};
use crate::error::{Error, Result};
use crate::index::BlockLoc;
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

impl BlockStore {
    /// Writes whole blocks starting at block index `first_block`. Every block must be exactly
    /// `block_size` bytes, except the file's final block, which must be exactly its remaining length.
    /// The file must exist (see [`BlockStore::set_len`]). Large writes are committed in chunks of
    /// about `write_txn_bytes`; if a later chunk fails the error is [`Error::PartialWrite`].
    pub fn write_blocks(&self, file_id: &[u8], first_block: u64, data: &[u8]) -> Result<()> {
        self.check_id(file_id)?;
        let bs = self.cfg.block_size;
        let len = self.stat(file_id)?.ok_or(Error::NotFound)?.len;
        validate_write(len, bs, first_block, data.len())?;

        let chunk_bytes = (self.cfg.write_txn_bytes / bs as usize).max(1) * bs as usize;
        let mut done = 0u64;
        for chunk in data.chunks(chunk_bytes) {
            let result = self
                .write_chunk(file_id, first_block + done, chunk)
                .map(|()| done += chunk.len().div_ceil(bs as usize) as u64)
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
    fn write_chunk(&self, file_id: &[u8], first: u64, chunk: &[u8]) -> Result<()> {
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
            self.encode_and_append(&blocks, &hashes, &need, &mut new_records)?;
            crash::point("write_after_append");
            let retry = self.index.update(false, |t| {
                let len = files::len(t, file_id)?.ok_or(Error::NotFound)?;
                validate_write(len, bs, first, chunk.len())?;

                // A dedup hit seen earlier may have been freed by another commit since.
                // Check before modifying anything; committing an untouched transaction is harmless.
                let mut missing = Vec::new();
                for (i, h) in hashes.iter().enumerate() {
                    if !new_records.contains_key(h) && t.dedup(h)?.is_none() {
                        missing.push(i);
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
                Some(missing) => need = missing,
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
        let level = self.cfg.zstd_level;
        let encoded: Vec<EncodedBlock> = self.install(|| {
            todo.par_iter()
                .map(|&i| encode_block(blocks[i], hashes[i], level))
                .collect::<std::io::Result<_>>()
        })?;
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
    use super::validate_write;
    use crate::Error;

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
}
