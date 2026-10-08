//! Manifest edits inside an index write transaction.

use crate::error::{Error, Result};
use crate::index::Tables;
use crate::manifest::{
    BLOCKS_PER_SEGMENT, MISSING, block_count, block_len, decode_ids, encode_segment, file_len,
    segment_count, slots_in_segment,
};

fn missing_segment(file_id: &[u8], seg: u32) -> Error {
    Error::Corrupt(format!(
        "manifest segment {seg} missing for file {:?}",
        String::from_utf8_lossy(file_id)
    ))
}

/// File length, or `None` if the file does not exist.
pub fn len(t: &Tables<'_>, file_id: &[u8]) -> Result<Option<u64>> {
    Ok(t.segment(file_id, 0)?.map(|v| file_len(&v)))
}

/// Replaces slots `[first, first + ids.len())` and returns the ids they held before.
pub fn set_slots(
    t: &mut Tables<'_>,
    file_id: &[u8],
    len: u64,
    first: u64,
    ids: &[u64],
) -> Result<Vec<u64>> {
    let mut old = Vec::with_capacity(ids.len());
    let mut block = first;
    let mut done = 0;
    while done < ids.len() {
        let seg = (block / BLOCKS_PER_SEGMENT) as u32;
        let off = (block % BLOCKS_PER_SEGMENT) as usize;
        let value = t
            .segment(file_id, seg)?
            .ok_or_else(|| missing_segment(file_id, seg))?;
        let mut slots = decode_ids(&value, seg);
        let take = (slots.len() - off).min(ids.len() - done);
        for k in 0..take {
            old.push(std::mem::replace(&mut slots[off + k], ids[done + k]));
        }
        t.put_segment(
            file_id,
            seg,
            &encode_segment((seg == 0).then_some(len), &slots),
        )?;
        done += take;
        block += take as u64;
    }
    Ok(old)
}

/// Creates the file (all blocks missing) or changes its length.
/// Returns the non-missing ids of blocks that were dropped: blocks past the new end, and a
/// block whose length changes (the old or new last block).
/// Touches one segment at a time: it rewrites segment 0 (the length) and the segment holding the
/// last block both lengths share, writes new segments when growing, and removes old ones when
/// shrinking. Memory use does not grow with the file's length.
pub fn resize(
    t: &mut Tables<'_>,
    file_id: &[u8],
    new_len: u64,
    block_size: u32,
) -> Result<Vec<u64>> {
    let nb = block_count(new_len, block_size);
    let new_segs = segment_count(nb);
    let Some(seg0) = t.segment(file_id, 0)? else {
        for s in 0..new_segs {
            put_missing_segment(t, file_id, new_len, nb, s)?;
        }
        return Ok(Vec::new());
    };
    let old_len = file_len(&seg0);
    let ob = block_count(old_len, block_size);
    let old_segs = segment_count(ob);
    let common = ob.min(nb);
    // The segment holding the last block both lengths share. Segments before it are unchanged
    // (apart from segment 0's length); segments after it exist only in the old or only in the
    // new layout.
    let first_seg = (common.saturating_sub(1) / BLOCKS_PER_SEGMENT) as u32;
    let base = first_seg as u64 * BLOCKS_PER_SEGMENT;

    let mut dropped = Vec::new();
    let value = if first_seg == 0 {
        seg0.clone()
    } else {
        t.segment(file_id, first_seg)?
            .ok_or_else(|| missing_segment(file_id, first_seg))?
    };
    let mut slots = decode_ids(&value, first_seg);
    if common > 0 {
        let last = common - 1;
        if block_len(old_len, block_size, last) != block_len(new_len, block_size, last) {
            dropped.push(std::mem::replace(
                &mut slots[(last - base) as usize],
                MISSING,
            ));
        }
    }
    let keep = slots_in_segment(nb, first_seg);
    if slots.len() > keep {
        dropped.extend(slots.drain(keep..));
    } else {
        slots.resize(keep, MISSING);
    }
    t.put_segment(
        file_id,
        first_seg,
        &encode_segment((first_seg == 0).then_some(new_len), &slots),
    )?;
    if first_seg > 0 {
        // Segment 0 was not rewritten above but its header holds the length.
        t.put_segment(
            file_id,
            0,
            &encode_segment(Some(new_len), &decode_ids(&seg0, 0)),
        )?;
    }
    // Growing: new segments, every block missing.
    for s in first_seg + 1..new_segs {
        put_missing_segment(t, file_id, new_len, nb, s)?;
    }
    // Shrinking: old segments past the new end.
    for s in first_seg + 1..old_segs {
        let value = t
            .segment(file_id, s)?
            .ok_or_else(|| missing_segment(file_id, s))?;
        dropped.extend(decode_ids(&value, s));
        t.remove_segment(file_id, s)?;
    }
    dropped.retain(|&id| id != MISSING);
    Ok(dropped)
}

/// Writes segment `s` of a file with `blocks` blocks, every slot missing.
fn put_missing_segment(
    t: &mut Tables<'_>,
    file_id: &[u8],
    len: u64,
    blocks: u64,
    s: u32,
) -> Result<()> {
    let slots = vec![MISSING; slots_in_segment(blocks, s)];
    t.put_segment(file_id, s, &encode_segment((s == 0).then_some(len), &slots))
}

/// Removes every segment of a file. Returns its non-missing block ids, or `None` if it does not exist.
pub fn remove(t: &mut Tables<'_>, file_id: &[u8], block_size: u32) -> Result<Option<Vec<u64>>> {
    let Some(seg0) = t.segment(file_id, 0)? else {
        return Ok(None);
    };
    let segs = segment_count(block_count(file_len(&seg0), block_size));
    let mut ids = Vec::new();
    for s in 0..segs {
        let value = if s == 0 {
            seg0.clone()
        } else {
            t.segment(file_id, s)?
                .ok_or_else(|| missing_segment(file_id, s))?
        };
        ids.extend(
            decode_ids(&value, s)
                .into_iter()
                .filter(|&id| id != MISSING),
        );
        t.remove_segment(file_id, s)?;
    }
    Ok(Some(ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;

    const BS: u32 = 4096;
    const SEG_BYTES: u64 = BLOCKS_PER_SEGMENT * BS as u64;

    fn open() -> (tempfile::TempDir, Index) {
        let dir = vfs_testkit::tempdir().unwrap();
        let index = Index::open(&dir.path().join("i.redb"), 1 << 20).unwrap();
        (dir, index)
    }

    fn all_slots(index: &Index, id: &[u8]) -> Vec<u64> {
        index
            .update(false, |t| {
                let len = len(t, id)?.unwrap();
                let mut out = Vec::new();
                for s in 0..segment_count(block_count(len, BS)) {
                    out.extend(decode_ids(&t.segment(id, s)?.unwrap(), s));
                }
                Ok(out)
            })
            .unwrap()
    }

    #[test]
    fn create_set_and_shrink() {
        let (_d, index) = open();
        index
            .update(false, |t| resize(t, b"f", 3 * BS as u64, BS).map(|_| ()))
            .unwrap();
        assert_eq!(all_slots(&index, b"f"), vec![0, 0, 0]);
        let old = index
            .update(false, |t| set_slots(t, b"f", 3 * BS as u64, 1, &[7, 8]))
            .unwrap();
        assert_eq!(old, vec![0, 0]);
        assert_eq!(all_slots(&index, b"f"), vec![0, 7, 8]);
        // Cut through block 1: blocks 1 (length changes) and 2 (past end) are dropped.
        let dropped = index
            .update(false, |t| resize(t, b"f", BS as u64 + 10, BS))
            .unwrap();
        assert_eq!(dropped, vec![7, 8]);
        assert_eq!(all_slots(&index, b"f"), vec![0, 0]);
    }

    #[test]
    fn growing_drops_a_short_last_block() {
        let (_d, index) = open();
        index
            .update(false, |t| resize(t, b"f", 10, BS).map(|_| ()))
            .unwrap();
        index
            .update(false, |t| set_slots(t, b"f", 10, 0, &[5]).map(|_| ()))
            .unwrap();
        let dropped = index
            .update(false, |t| resize(t, b"f", 2 * BS as u64, BS))
            .unwrap();
        assert_eq!(dropped, vec![5]);
        assert_eq!(all_slots(&index, b"f"), vec![0, 0]);
        // Growing a file whose last block is full keeps it.
        index
            .update(false, |t| {
                set_slots(t, b"f", 2 * BS as u64, 0, &[1, 2]).map(|_| ())
            })
            .unwrap();
        let dropped = index
            .update(false, |t| resize(t, b"f", 3 * BS as u64, BS))
            .unwrap();
        assert!(dropped.is_empty());
        assert_eq!(all_slots(&index, b"f"), vec![1, 2, 0]);
    }

    #[test]
    fn multi_segment_resize_and_remove() {
        let (_d, index) = open();
        let size = 2 * SEG_BYTES + 5;
        index
            .update(false, |t| resize(t, b"big", size, BS).map(|_| ()))
            .unwrap();
        let n = block_count(size, BS) as usize;
        let ids: Vec<u64> = (1..=n as u64).collect();
        index
            .update(false, |t| set_slots(t, b"big", size, 0, &ids).map(|_| ()))
            .unwrap();
        assert_eq!(all_slots(&index, b"big"), ids);

        // Shrink into the second segment; segment 0 header must reflect the new length.
        let new_len = SEG_BYTES + 3 * BS as u64;
        let dropped = index
            .update(false, |t| resize(t, b"big", new_len, BS))
            .unwrap();
        assert_eq!(dropped, ids[4099..].to_vec());
        assert_eq!(all_slots(&index, b"big"), ids[..4099].to_vec());
        index
            .update(false, |t| {
                assert_eq!(len(t, b"big")?, Some(new_len));
                assert!(t.segment(b"big", 2)?.is_none());
                Ok(())
            })
            .unwrap();

        let removed = index
            .update(false, |t| remove(t, b"big", BS))
            .unwrap()
            .unwrap();
        assert_eq!(removed, ids[..4099].to_vec());
        assert!(
            index
                .update(false, |t| remove(t, b"big", BS))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn grow_across_segments_and_shrink_back() {
        let (_d, index) = open();
        let small = BS as u64;
        index
            .update(false, |t| resize(t, b"g", small, BS).map(|_| ()))
            .unwrap();
        index
            .update(false, |t| set_slots(t, b"g", small, 0, &[9]).map(|_| ()))
            .unwrap();
        let blocks = 3 * BLOCKS_PER_SEGMENT + 5;
        let big = blocks * BS as u64;
        let dropped = index.update(false, |t| resize(t, b"g", big, BS)).unwrap();
        assert!(dropped.is_empty());
        let mut expect = vec![MISSING; blocks as usize];
        expect[0] = 9;
        assert_eq!(all_slots(&index, b"g"), expect);
        index
            .update(false, |t| {
                assert_eq!(len(t, b"g")?, Some(big));
                assert_eq!(t.segment(b"g", 3)?.unwrap().len(), 5 * 8);
                assert!(t.segment(b"g", 4)?.is_none());
                Ok(())
            })
            .unwrap();

        // Fill a few slots in the later segments, then shrink back to one block.
        let ids = [11, 12, 13];
        let at = [BLOCKS_PER_SEGMENT, 2 * BLOCKS_PER_SEGMENT + 7, blocks - 1];
        for (&id, &b) in ids.iter().zip(&at) {
            index
                .update(false, |t| set_slots(t, b"g", big, b, &[id]).map(|_| ()))
                .unwrap();
        }
        let dropped = index.update(false, |t| resize(t, b"g", small, BS)).unwrap();
        assert_eq!(dropped, ids.to_vec());
        assert_eq!(all_slots(&index, b"g"), vec![9]);
        index
            .update(false, |t| {
                assert_eq!(len(t, b"g")?, Some(small));
                for s in 1..4 {
                    assert!(t.segment(b"g", s)?.is_none());
                }
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn empty_file() {
        let (_d, index) = open();
        index
            .update(false, |t| resize(t, b"e", 0, BS).map(|_| ()))
            .unwrap();
        assert_eq!(all_slots(&index, b"e"), Vec::<u64>::new());
        index
            .update(false, |t| resize(t, b"e", 1, BS).map(|_| ()))
            .unwrap();
        assert_eq!(all_slots(&index, b"e"), vec![0]);
    }
}
