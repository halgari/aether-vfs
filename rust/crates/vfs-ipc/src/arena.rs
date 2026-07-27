//! Shared bulk data arena — banks sized per ring slot for concurrent READs.
//! Uses `SharedSeg` helpers only (no extra unsafe beyond `seg`).

use crate::seg::SharedSeg;

/// Default ring payload capacity (1 MiB) for director / server setups.
pub const DEFAULT_PAYLOAD_CAP: u32 = 1_048_576;
/// Default bulk arena size after the control ring (32 MiB).
pub const DEFAULT_ARENA_BYTES: usize = 32 * 1024 * 1024;
/// Default number of ring server worker threads (host IPC).
pub const DEFAULT_WORKER_COUNT: usize = 4;

/// View over arena memory inside an existing shared segment.
pub struct DataArena<'a> {
    seg: &'a SharedSeg,
    /// Byte offset of arena start within the shared mapping.
    pub mapping_offset: usize,
    pub bank_size: usize,
    pub banks: usize,
}

impl<'a> DataArena<'a> {
    pub fn new(seg: &'a SharedSeg, mapping_offset: usize, arena_len: usize, banks: usize) -> Self {
        let banks = banks.max(1);
        let bank_size = (arena_len / banks).max(4096);
        DataArena {
            seg,
            mapping_offset,
            bank_size,
            banks,
        }
    }

    pub fn bank_index(&self, slot: u32) -> usize {
        (slot as usize) % self.banks
    }

    /// Mapping-relative offset of the start of `slot`'s bank.
    pub fn bank_mapping_offset(&self, slot: u32) -> u64 {
        (self.mapping_offset + self.bank_index(slot) * self.bank_size) as u64
    }

    /// Write `data` into the bank for `slot`. Returns mapping-relative offset of data.
    pub fn write_bank(&self, slot: u32, data: &[u8]) -> Result<u64, ()> {
        if data.len() > self.bank_size {
            return Err(());
        }
        let off = self.mapping_offset + self.bank_index(slot) * self.bank_size;
        if !self.seg.write_bytes(off, data) {
            return Err(());
        }
        Ok(off as u64)
    }

    /// Fill the bank for `slot` by letting `f` write into a mutable bank slice.
    ///
    /// Returns `(mapping_offset, bytes_written)`. Used by bulk READ to avoid an
    /// intermediate `Vec` (disk/zip → arena directly).
    pub fn fill_bank(
        &self,
        slot: u32,
        max_len: usize,
        f: impl FnOnce(&mut [u8]) -> Result<usize, i32>,
    ) -> Result<(u64, usize), i32> {
        let max = max_len.min(self.bank_size);
        if max == 0 {
            return Ok((self.bank_mapping_offset(slot), 0));
        }
        let off = self.mapping_offset + self.bank_index(slot) * self.bank_size;
        self.seg
            .with_mut_bytes(off, max, |buf| {
                let n = f(buf)?;
                let n = n.min(buf.len());
                Ok((off as u64, n))
            })
            .ok_or(vfs_protocol::ST_IO_ERROR)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OwnedSeg;

    #[test]
    fn write_bank_roundtrip() {
        let owned = OwnedSeg::new(256 * 1024);
        let arena = DataArena::new(owned.seg(), 0, 128 * 1024, 4);
        let data = b"hello-bulk";
        let off = arena.write_bank(1, data).unwrap();
        assert_eq!(off, arena.bank_size as u64);
        let got = owned.seg().read_bytes(off as usize, data.len()).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn fill_bank_writes_without_extra_copy() {
        let owned = OwnedSeg::new(256 * 1024);
        let arena = DataArena::new(owned.seg(), 0, 128 * 1024, 4);
        let (off, n) = arena
            .fill_bank(2, 64, |buf| {
                buf[..11].copy_from_slice(b"fill-direct");
                Ok(11)
            })
            .unwrap();
        assert_eq!(n, 11);
        assert_eq!(off, arena.bank_mapping_offset(2));
        let got = owned.seg().read_bytes(off as usize, 11).unwrap();
        assert_eq!(got, b"fill-direct");
    }
}
