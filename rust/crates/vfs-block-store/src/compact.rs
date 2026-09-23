//! Compaction: copy live records out of mostly-garbage packs, then retire and delete them.

use std::fs::File;
use std::io::{self, BufReader, Read};

use crate::codec::{HEADER_LEN, RecordHeader, checksum};
use crate::config::CompactOptions;
use crate::crash;
use crate::error::{Error, Result};
use crate::index::{BlockLoc, PackState};
use crate::pack::{pack_path, remove_pack_file};
use crate::store::BlockStore;

/// What one [`BlockStore::compact`] call did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactReport {
    pub packs_compacted: u32,
    /// Pack bytes scanned.
    pub bytes_read: u64,
    /// Live record bytes copied into the active pack.
    pub bytes_moved: u64,
}

/// A live record to move: its block id, current location, and raw record bytes.
struct LiveRecord {
    id: u64,
    loc: BlockLoc,
    bytes: Vec<u8>,
}

/// A record found while scanning a pack, not yet checked for liveness.
struct Scanned {
    offset: u64,
    header: RecordHeader,
    bytes: Vec<u8>,
}

impl BlockStore {
    /// Compacts sealed packs whose garbage ratio is at least `opts.min_garbage_ratio`, worst first,
    /// until `opts.max_bytes` pack bytes have been processed. Readers and writers keep working.
    pub fn compact(&self, opts: CompactOptions) -> Result<CompactReport> {
        let _serial = self.compact_lock.lock().unwrap();
        self.delete_retired()?;
        let mut candidates = Vec::new();
        for p in self.stats()?.packs {
            if p.sealed && p.file_bytes > 0 && p.garbage_ratio() >= opts.min_garbage_ratio {
                candidates.push(p);
            }
        }
        candidates.sort_by(|a, b| b.garbage_ratio().total_cmp(&a.garbage_ratio()));

        let mut report = CompactReport::default();
        for p in candidates {
            if report.bytes_read >= opts.max_bytes {
                break;
            }
            report.bytes_moved += self.compact_pack(p.id)?;
            report.bytes_read += p.file_bytes;
            report.packs_compacted += 1;
        }
        self.delete_retired()?;
        Ok(report)
    }

    /// Moves every live record out of `pack` and retires it. Returns bytes moved.
    fn compact_pack(&self, pack: u32) -> Result<u64> {
        let file = File::open(pack_path(&self.pack_dir, pack))?;
        let size = file.metadata()?.len();
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut offset = 0u64;
        let mut moved = 0u64;
        let mut batch = Vec::new();
        let mut batch_bytes = 0usize;

        // Sequential scan. Stops at the first unreadable header (a torn tail after a crash);
        // anything live beyond it is picked up by `evacuate_by_index` below.
        while offset + HEADER_LEN as u64 <= size {
            let mut bytes = vec![0u8; HEADER_LEN];
            reader.read_exact(&mut bytes)?;
            let Ok(header) = RecordHeader::decode(&bytes) else {
                break;
            };
            if offset + header.record_len() > size {
                break;
            }
            bytes.resize(header.record_len() as usize, 0);
            reader.read_exact(&mut bytes[HEADER_LEN..])?;
            batch_bytes += bytes.len();
            batch.push(Scanned {
                offset,
                header,
                bytes,
            });
            offset += header.record_len();
            if batch_bytes >= self.cfg.write_txn_bytes {
                moved += self.move_scanned(pack, std::mem::take(&mut batch))?;
                batch_bytes = 0;
            }
        }
        moved += self.move_scanned(pack, batch)?;
        moved += self.evacuate_by_index(pack)?;

        crash::point("compact_before_retire");
        self.durable_commit(|t| {
            let mut info = t
                .pack(pack)?
                .ok_or_else(|| Error::Corrupt(format!("pack {pack} vanished during compaction")))?;
            if info.live_bytes != 0 {
                return Err(Error::Corrupt(format!(
                    "pack {pack} still has live data after compaction"
                )));
            }
            info.state = PackState::Retired;
            t.put_pack(pack, &info)
        })?;
        let generation = self.tracker.advance();
        self.retired.lock().unwrap().push((pack, generation));
        crash::point("compact_after_retire");
        Ok(moved)
    }

    /// Keeps the scanned records that are still the live copy of their block, then moves them.
    fn move_scanned(&self, pack: u32, scanned: Vec<Scanned>) -> Result<u64> {
        let mut live = Vec::new();
        {
            let r = self.index.read()?;
            for s in scanned {
                let Some(id) = r.dedup(&s.header.hash)? else {
                    continue;
                };
                let Some(loc) = r.block(id)? else { continue };
                if loc.pack != pack || loc.offset != s.offset {
                    continue;
                }
                if checksum(&s.bytes[HEADER_LEN..]) != s.header.checksum
                    || s.header.stored_len != loc.stored_len
                {
                    self.heal(id, &loc, "checksum mismatch during compaction")?;
                    continue;
                }
                live.push(LiveRecord {
                    id,
                    loc,
                    bytes: s.bytes,
                });
            }
        }
        self.move_records(live)
    }

    /// Moves any block still recorded in `pack` by looking it up in the index. Normally finds
    /// nothing; it covers records the sequential scan could not reach (corrupt headers).
    fn evacuate_by_index(&self, pack: u32) -> Result<u64> {
        let mut locs = Vec::new();
        self.index.read()?.for_each_block(|id, loc| {
            if loc.pack == pack {
                locs.push((id, loc));
            }
            Ok(())
        })?;
        let mut moved = 0;
        for chunk in locs.chunks(256) {
            let mut live = Vec::new();
            for &(id, loc) in chunk {
                let mut bytes = vec![0u8; loc.record_len() as usize];
                let ok = match self.packs.read_exact_at(pack, &mut bytes, loc.offset) {
                    Ok(()) => RecordHeader::decode(&bytes[..HEADER_LEN]).is_ok_and(|h| {
                        h.hash == loc.hash
                            && h.stored_len == loc.stored_len
                            && checksum(&bytes[HEADER_LEN..]) == h.checksum
                    }),
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => false,
                    Err(e) => return Err(e.into()),
                };
                if ok {
                    live.push(LiveRecord { id, loc, bytes });
                } else {
                    self.heal(id, &loc, "unreadable record during compaction")?;
                }
            }
            moved += self.move_records(live)?;
        }
        Ok(moved)
    }

    /// Appends records to the active pack and repoints their blocks, skipping any block that
    /// was freed or moved meanwhile (its copy becomes garbage).
    fn move_records(&self, live: Vec<LiveRecord>) -> Result<u64> {
        if live.is_empty() {
            return Ok(0);
        }
        let new_locs = self.append_records(
            live.iter()
                .map(|l| (&l.bytes[..HEADER_LEN], &l.bytes[HEADER_LEN..])),
        )?;
        crash::point("compact_after_copy");
        self.index.update(false, |t| {
            let mut moved = 0u64;
            for (l, (new_pack, new_offset)) in live.iter().zip(new_locs) {
                let Some(cur) = t.block(l.id)? else { continue };
                if cur.pack != l.loc.pack || cur.offset != l.loc.offset {
                    continue;
                }
                let len = cur.record_len() as i64;
                t.put_block(
                    l.id,
                    &BlockLoc {
                        pack: new_pack,
                        offset: new_offset,
                        ..cur
                    },
                )?;
                t.add_live(cur.pack, -len)?;
                t.add_live(new_pack, len)?;
                moved += len as u64;
            }
            Ok(moved)
        })
    }

    /// Deletes retired packs that no running read can still reference.
    pub(crate) fn delete_retired(&self) -> Result<()> {
        let pending = std::mem::take(&mut *self.retired.lock().unwrap());
        let mut keep = Vec::new();
        for (pack, generation) in pending {
            if !self.tracker.is_clear_before(generation) {
                keep.push((pack, generation));
                continue;
            }
            self.packs.close(pack);
            if let Err(e) = remove_pack_file(&self.pack_dir, pack) {
                // Windows: another program (for example a virus scanner) may hold the file.
                tracing::warn!(pack, error = %e, "could not delete retired pack; will retry");
                keep.push((pack, generation));
                continue;
            }
            self.index.update(false, |t| t.remove_pack(pack))?;
        }
        self.retired.lock().unwrap().extend(keep);
        Ok(())
    }
}
