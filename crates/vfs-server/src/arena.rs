//! **B1:** Shared bulk data arena — banks sized per ring slot for concurrent READs.
//! Uses `SharedSeg::write_bytes` (no extra unsafe in this crate).

use vfs_ipc::SharedSeg;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_ipc::OwnedSeg;

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
}
