//! Model-based property test: random operation sequences against an in-memory model.

mod common;

use std::collections::HashMap;
use std::ops::Range;

use aether_block_store::{BlockStore, CompactOptions, Error};
use common::*;
use proptest::prelude::*;

const FILES: u8 = 4;
const MAX_BLOCKS: u64 = 6;

#[derive(Debug, Clone)]
enum Op {
    SetLen {
        file: u8,
        len: u64,
    },
    Write {
        file: u8,
        first: u64,
        seeds: Vec<u8>,
    },
    Delete {
        file: u8,
    },
    Compact,
    Flush,
    Reopen,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..FILES, 0..=MAX_BLOCKS * BS as u64).prop_map(|(file, len)| Op::SetLen { file, len }),
        6 => (0..FILES, 0..MAX_BLOCKS, prop::collection::vec(0..6u8, 1..6))
            .prop_map(|(file, first, seeds)| Op::Write { file, first, seeds }),
        1 => (0..FILES).prop_map(|file| Op::Delete { file }),
        1 => Just(Op::Compact),
        1 => Just(Op::Flush),
        1 => Just(Op::Reopen),
    ]
}

/// Model of one file: length and per-block content (`None` = not cached).
#[derive(Debug, Clone)]
struct ModelFile {
    len: u64,
    blocks: Vec<Option<Vec<u8>>>,
}

fn block_len(len: u64, idx: u64) -> usize {
    len.saturating_sub(idx * BS as u64).min(BS as u64) as usize
}

/// Few distinct seeds so dedup happens; even seeds compress, odd seeds do not.
fn content(seed: u8, len: usize) -> Vec<u8> {
    if seed.is_multiple_of(2) {
        pattern_bytes(seed as u64, len)
    } else {
        random_bytes(seed as u64, len)
    }
}

fn file_id(f: u8) -> [u8; 2] {
    [b'f', f]
}

fn resize(m: &mut ModelFile, new_len: u64) {
    let nb = new_len.div_ceil(BS as u64) as usize;
    let common = m.blocks.len().min(nb);
    if common > 0 && block_len(m.len, common as u64 - 1) != block_len(new_len, common as u64 - 1) {
        m.blocks[common - 1] = None;
    }
    m.blocks.resize(nb, None);
    m.len = new_len;
}

fn apply(
    store: &mut Option<BlockStore>,
    dir: &std::path::Path,
    model: &mut HashMap<u8, ModelFile>,
    op: &Op,
) {
    let s = store.as_ref().unwrap();
    match op {
        Op::SetLen { file, len } => {
            s.set_len(&file_id(*file), *len).unwrap();
            let m = model.entry(*file).or_insert(ModelFile {
                len: 0,
                blocks: Vec::new(),
            });
            resize(m, *len);
        }
        Op::Write { file, first, seeds } => {
            let Some(m) = model.get_mut(file) else {
                assert!(matches!(
                    s.write_blocks(&file_id(*file), 0, &[0u8; BS]),
                    Err(Error::NotFound)
                ));
                return;
            };
            let nb = m.blocks.len() as u64;
            if nb == 0 {
                assert!(matches!(
                    s.write_blocks(&file_id(*file), 0, &[0u8; BS]),
                    Err(Error::OutOfRange)
                ));
                return;
            }
            let first = first % nb;
            let count = (seeds.len() as u64).min(nb - first);
            let mut data = Vec::new();
            for k in 0..count {
                let idx = first + k;
                let block = content(seeds[k as usize], block_len(m.len, idx));
                data.extend_from_slice(&block);
                m.blocks[idx as usize] = Some(block);
            }
            s.write_blocks(&file_id(*file), first, &data).unwrap();
        }
        Op::Delete { file } => {
            let r = s.delete(&file_id(*file));
            if model.remove(file).is_some() {
                r.unwrap();
            } else {
                assert!(matches!(r, Err(Error::NotFound)));
            }
        }
        Op::Compact => {
            s.compact(CompactOptions {
                min_garbage_ratio: 0.0,
                max_bytes: u64::MAX,
            })
            .unwrap();
        }
        Op::Flush => s.flush().unwrap(),
        Op::Reopen => {
            store.take().unwrap().close().unwrap();
            *store = Some(open(dir));
        }
    }
}

fn check(store: &BlockStore, model: &HashMap<u8, ModelFile>) {
    for f in 0..FILES {
        let id = file_id(f);
        let Some(m) = model.get(&f) else {
            assert!(store.stat(&id).unwrap().is_none());
            continue;
        };
        assert_eq!(store.stat(&id).unwrap().unwrap().len, m.len);
        let mut buf = vec![0u8; m.len as usize];
        let r = store.read(&id, 0, &mut buf).unwrap();
        assert_eq!(r.bytes, m.len as usize);

        let mut expect_missing: Vec<Range<u64>> = Vec::new();
        let mut expect_cached: Vec<Range<u64>> = Vec::new();
        for (i, b) in m.blocks.iter().enumerate() {
            let start = i as u64 * BS as u64;
            let range = start..start + block_len(m.len, i as u64) as u64;
            let list = if b.is_some() {
                &mut expect_cached
            } else {
                &mut expect_missing
            };
            match list.last_mut() {
                Some(last) if last.end == range.start => last.end = range.end,
                _ => list.push(range.clone()),
            }
            if let Some(b) = b {
                assert!(
                    buf[range.start as usize..range.end as usize] == b[..],
                    "file {f} block {i}: wrong bytes"
                );
            }
        }
        assert_eq!(r.missing, expect_missing, "file {f}");
        assert_eq!(store.cached_ranges(&id).unwrap(), expect_cached, "file {f}");
    }
    let report = store.verify().unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn store_matches_model(ops in prop::collection::vec(op(), 1..40)) {
        let dir = vfs_testkit::tempdir().unwrap();
        let mut store = Some(open(dir.path()));
        let mut model = HashMap::new();
        for op in &ops {
            apply(&mut store, dir.path(), &mut model, op);
            check(store.as_ref().unwrap(), &model);
        }
    }
}
