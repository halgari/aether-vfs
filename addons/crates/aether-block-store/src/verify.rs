//! Full consistency check (fsck).

use std::collections::HashMap;

use crate::codec::decode_payload;
use crate::error::Result;
use crate::index::{BlockLoc, PackState};
use crate::manifest::{
    MISSING, block_count, decode_ids, file_len, segment_count, slots_in_segment,
};
use crate::pack::pack_path;
use crate::store::BlockStore;

/// Result of [`BlockStore::verify`].
#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub files: u64,
    pub blocks: u64,
    /// Human-readable descriptions of every inconsistency found.
    pub problems: Vec<String>,
}

impl VerifyReport {
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// (file id, block count, next expected segment) of the file being checked.
type FileCursor = Option<(Vec<u8>, u64, u32)>;

fn finish_file(cur: &FileCursor, problems: &mut Vec<String>) {
    if let Some((id, blocks, next)) = cur
        && *next != segment_count(*blocks)
    {
        problems.push(format!(
            "file {:?}: has {next} segments, expected {}",
            String::from_utf8_lossy(id),
            segment_count(*blocks)
        ));
    }
}

impl BlockStore {
    /// Checks every manifest, refcount, dedup entry, pack total and record checksum.
    /// Reads every stored block, so it is slow on large stores. Memory use grows with the
    /// number of distinct referenced blocks.
    pub fn verify(&self) -> Result<VerifyReport> {
        let _guard = self.tracker.enter();
        let r = self.index.read()?;
        let bs = self.cfg.block_size;
        let mut report = VerifyReport::default();
        let problems = &mut report.problems;

        // Manifests: segments are complete and sized correctly; count references.
        let mut refs: HashMap<u64, u32> = HashMap::new();
        let mut current: FileCursor = None;
        let mut files = 0u64;
        r.for_each_segment(|id, seg, value| {
            if seg == 0 {
                finish_file(&current, problems);
                files += 1;
                current = Some((id.to_vec(), block_count(file_len(value), bs), 0));
            }
            match &mut current {
                Some((cur_id, blocks, next)) if cur_id.as_slice() == id && *next == seg => {
                    let ids = decode_ids(value, seg);
                    let expect = slots_in_segment(*blocks, seg);
                    if ids.len() != expect {
                        problems.push(format!(
                            "file {:?} segment {seg}: {} slots, expected {expect}",
                            String::from_utf8_lossy(id),
                            ids.len()
                        ));
                    }
                    for bid in ids.into_iter().filter(|&b| b != MISSING) {
                        *refs.entry(bid).or_default() += 1;
                    }
                    *next += 1;
                }
                _ => problems.push(format!(
                    "file {:?}: unexpected segment {seg}",
                    String::from_utf8_lossy(id)
                )),
            }
            Ok(())
        })?;
        finish_file(&current, problems);
        report.files = files;

        // Blocks: refcounts, dedup entries, records.
        let mut live: HashMap<u32, u64> = HashMap::new();
        let mut blocks = 0u64;
        r.for_each_block(|id, loc| {
            blocks += 1;
            let n = refs.remove(&id).unwrap_or(0);
            if n != loc.refcount {
                problems.push(format!(
                    "block {id}: refcount {} but {n} references",
                    loc.refcount
                ));
            }
            if r.dedup(&loc.hash)? != Some(id) {
                problems.push(format!("block {id}: dedup entry missing or wrong"));
            }
            *live.entry(loc.pack).or_default() += loc.record_len();
            let bad = match self.check_record(&loc) {
                Ok(Ok(())) => None,
                Ok(Err(reason)) => Some(reason.to_string()),
                Err(e) => Some(e.to_string()),
            };
            if let Some(reason) = bad {
                problems.push(format!(
                    "block {id} (pack {} offset {}): {reason}",
                    loc.pack, loc.offset
                ));
            }
            Ok(())
        })?;
        report.blocks = blocks;
        // Ids left in `refs` have no block row: healed blocks, which read as missing. Not an error.

        r.for_each_dedup(|hash, id| {
            if r.block(id)?.is_none_or(|l| l.hash != hash) {
                problems.push(format!("dedup entry for block {id} has no matching block"));
            }
            Ok(())
        })?;

        for (id, info) in r.packs()? {
            if info.state == PackState::Retired {
                continue;
            }
            let expect = live.remove(&id).unwrap_or(0);
            if info.live_bytes != expect {
                problems.push(format!(
                    "pack {id}: live_bytes {} but blocks total {expect}",
                    info.live_bytes
                ));
            }
            if !pack_path(&self.pack_dir, id).exists() {
                problems.push(format!("pack {id}: file missing"));
            }
        }
        for id in live.keys() {
            problems.push(format!(
                "blocks reference pack {id}, which is not registered or is retired"
            ));
        }
        Ok(report)
    }

    fn check_record(&self, loc: &BlockLoc) -> Result<std::result::Result<(), &'static str>> {
        let mut out = vec![0u8; loc.raw_len as usize];
        self.read_record(loc, |header, payload| {
            decode_payload(header, payload, &mut out)
        })
    }
}
