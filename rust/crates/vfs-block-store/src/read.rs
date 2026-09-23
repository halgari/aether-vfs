//! Read path: manifest lookup, positioned pack reads, verification, self-healing.

use std::cell::RefCell;
use std::io;
use std::sync::atomic::Ordering;

use crate::codec::{HEADER_LEN, RecordHeader, decode_payload};
use crate::error::{Error, Result};
use crate::index::BlockLoc;
use crate::manifest::{BLOCKS_PER_SEGMENT, MISSING, file_len, slot};
use crate::store::{BlockStore, ReadResult, push_range};

thread_local! {
    static RECORD_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static BLOCK_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

impl BlockStore {
    /// Reads up to `buf.len()` bytes at `offset`, clamped to end of file. Cached blocks are
    /// decoded into `buf`; blocks that are not cached are reported in `missing`.
    pub fn read(&self, file_id: &[u8], offset: u64, buf: &mut [u8]) -> Result<ReadResult> {
        self.check_id(file_id)?;
        let _guard = self.tracker.enter();
        let r = self.index.read()?;
        let seg0 = r.segment(file_id, 0)?.ok_or(Error::NotFound)?;
        let len = file_len(&seg0);
        if offset >= len || buf.is_empty() {
            return Ok(ReadResult {
                bytes: 0,
                missing: Vec::new(),
            });
        }
        let end = len.min(offset + buf.len() as u64);
        let bs = self.cfg.block_size as u64;
        let mut missing = Vec::new();

        let mut seg_no = 0u32;
        let mut seg = seg0;
        for b in offset / bs..=(end - 1) / bs {
            let want_seg = (b / BLOCKS_PER_SEGMENT) as u32;
            if want_seg != seg_no {
                seg = r.segment(file_id, want_seg)?.ok_or_else(|| {
                    Error::Corrupt(format!("manifest segment {want_seg} missing"))
                })?;
                seg_no = want_seg;
            }
            let id = slot(&seg, seg_no, (b % BLOCKS_PER_SEGMENT) as usize);
            let block_start = b * bs;
            let block_end = len.min(block_start + bs);
            let from = offset.max(block_start);
            let to = end.min(block_end);
            let dst = &mut buf[(from - offset) as usize..(to - offset) as usize];
            let found = match (id != MISSING).then(|| r.block(id)).transpose()?.flatten() {
                Some(loc) => self.read_block_into(id, &loc, (from - block_start) as usize, dst)?,
                None => false,
            };
            if !found {
                push_range(&mut missing, block_start..block_end);
            }
        }
        Ok(ReadResult {
            bytes: (end - offset) as usize,
            missing,
        })
    }

    /// Decodes bytes `[skip, skip + dst.len())` of block `id` into `dst`.
    /// Returns false (after healing) if the record is corrupt.
    fn read_block_into(
        &self,
        id: u64,
        loc: &BlockLoc,
        skip: usize,
        dst: &mut [u8],
    ) -> Result<bool> {
        if skip + dst.len() > loc.raw_len as usize {
            return Err(Error::Corrupt(format!(
                "block {id} is shorter than its file slot"
            )));
        }
        match self.read_record(loc, |header, payload| {
            if skip == 0 && dst.len() == header.raw_len as usize {
                return decode_payload(header, payload, dst);
            }
            BLOCK_BUF.with(|bb| {
                let mut bb = bb.borrow_mut();
                bb.resize(header.raw_len as usize, 0);
                decode_payload(header, payload, &mut bb)?;
                dst.copy_from_slice(&bb[skip..skip + dst.len()]);
                Ok(())
            })
        })? {
            Ok(()) => Ok(true),
            Err(reason) => {
                self.heal(id, loc, reason)?;
                Ok(false)
            }
        }
    }

    /// Reads the record at `loc`, checks its header against `loc`, and passes it to `f`.
    /// The outer `Result` carries I/O errors; the inner one describes corruption.
    pub(crate) fn read_record<R>(
        &self,
        loc: &BlockLoc,
        f: impl FnOnce(&RecordHeader, &[u8]) -> std::result::Result<R, &'static str>,
    ) -> Result<std::result::Result<R, &'static str>> {
        RECORD_BUF.with(|rb| {
            let mut rb = rb.borrow_mut();
            rb.resize(loc.record_len() as usize, 0);
            match self.packs.read_exact_at(loc.pack, &mut rb, loc.offset) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(Err("record truncated"));
                }
                Err(e) => return Err(e.into()),
            }
            let header = match RecordHeader::decode(&rb[..HEADER_LEN]) {
                Ok(h) => h,
                Err(reason) => return Ok(Err(reason)),
            };
            if header.stored_len != loc.stored_len
                || header.raw_len != loc.raw_len
                || header.hash != loc.hash
            {
                return Ok(Err("record does not match index"));
            }
            Ok(f(&header, &rb[HEADER_LEN..]))
        })
    }

    /// Drops a corrupt block from the index so it reads as missing and can be rewritten.
    pub(crate) fn heal(&self, id: u64, loc: &BlockLoc, reason: &'static str) -> Result<()> {
        tracing::warn!(
            block = id,
            pack = loc.pack,
            offset = loc.offset,
            reason,
            "dropping corrupt block"
        );
        self.index.update(false, |t| {
            if let Some(cur) = t.block(id)?
                && cur.pack == loc.pack
                && cur.offset == loc.offset
            {
                t.remove_block(id, &cur)?;
            }
            Ok(())
        })?;
        self.healed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
