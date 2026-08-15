//! One fold, both sides of the ring.
//!
//! The shim canonicalises an incoming NT path with `vfs_redirect::RootMap`,
//! which folds each surviving component through `vfs_core::fold` — Unicode
//! simple lowercasing. The joined result is the vpath that crosses the ring.
//! Everything the director does with that vpath afterwards (mount-prefix
//! stripping, zip lookup, directory merging) has to fold the *same* way, or a
//! component whose case only Unicode knows how to fold arrives spelled one way
//! and is looked up spelled another.
//!
//! `DiskProvider` never showed this: Windows folds Unicode itself, so a
//! `CreateFileW` for `data/über/a.esp` finds `Data\ÜBER\a.esp` no matter what
//! this codebase believes about case. Only the providers that keep their own
//! index — `ZipProvider`'s `by_fold`, `MountGraph`'s prefix match, the
//! `readdir` merge maps — can disagree, and they fail closed, so the file
//! simply is not there. That still counts as `routed` + `opens_err`, so the
//! reconciliation invariant balances while the content is missing.
//!
//! These tests therefore run a real path through the real canonicaliser and
//! demand real bytes back out of the real provider stack. They are not unit
//! tests of `fold`; swapping `fold` for `to_ascii_lowercase` anywhere below the
//! ring makes them fail.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vfs_compose::LayeredProvider;
use vfs_director::{DiskProvider, MountGraph};
use vfs_provider::{Provider, VPath, OPEN_READ};
use vfs_redirect::{RootMap, VolumeMap};
use vfs_zip::ZipProvider;

/// A directory component whose case only Unicode folds: `Ü`/`ü` and `Б`/`б`
/// are both invisible to `to_ascii_lowercase`.
const UPPER_DIR: &str = "ÜBERМОД";
const LOWER_DIR: &str = "überмод";
const FILE: &str = "a.esp";
const BYTES: &[u8] = b"the non-ascii-cased mod's real bytes";

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vfs-fold-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The vpath the shim would put on the wire for `abs`, produced by the same
/// `RootMap` the shim's `FuseClient::vpath_under_root` uses — not by
/// lowercasing a string here, which would beg the question this file exists to
/// ask.
fn vpath_from_the_shim(root: &Path, abs: &Path) -> String {
    let map = RootMap::new(&root.to_string_lossy(), VolumeMap::empty()).unwrap();
    let nt = format!(r"\??\{}", abs.to_string_lossy());
    map.remainder(&nt)
        .unwrap_or_else(|| panic!("{nt} did not resolve under {}", root.display()))
        .join("/")
}

fn read_all(p: &dyn Provider, vpath: &str) -> Vec<u8> {
    let (h, size, is_dir) = p
        .open(VPath::at_default(vpath), OPEN_READ)
        .unwrap_or_else(|e| panic!("open {vpath}: status {e}"));
    assert!(!is_dir, "{vpath} opened as a directory");
    let mut buf = vec![0u8; size as usize];
    let n = p.read_at(h, 0, &mut buf).unwrap();
    buf.truncate(n);
    p.close(h).unwrap();
    buf
}

// ---------------------------------------------------------------------------

/// A zip entry under a non-ASCII-cased directory, addressed by the folded
/// vpath the shim actually emits.
#[test]
fn a_zip_entry_under_a_non_ascii_cased_directory_resolves_from_a_shim_folded_vpath() {
    let dir = scratch("zip");
    let root = dir.join("root");
    std::fs::create_dir_all(root.join("Data")).unwrap();

    let entry = format!("Data/{UPPER_DIR}/{FILE}");
    let zip = dir.join("mod.zip");
    write_stored_zip(&zip, &entry, BYTES);

    // The path the game would open, spelled the way the content is spelled.
    let abs = root.join("Data").join(UPPER_DIR).join(FILE);
    let vpath = vpath_from_the_shim(&root, &abs);
    assert_eq!(
        vpath,
        format!("data/{LOWER_DIR}/{FILE}"),
        "the shim's own canonicaliser folds Unicode; if this line changes, the \
         wire format changed and both sides move together"
    );

    let provider = ZipProvider::open(&zip).unwrap();
    assert!(
        provider.getattr(VPath::at_default(&vpath)).unwrap().is_some(),
        "ZipProvider's fold index missed {vpath} — its `by_fold` map is keyed by \
         a different fold than the shim used"
    );
    assert_eq!(read_all(&provider, &vpath), BYTES);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A mount configured at a non-ASCII-cased prefix, queried with the folded
/// vpath. `MountGraph::strip_prefix` compares the as-authored prefix against
/// the folded query.
#[test]
fn a_mount_prefixed_with_a_non_ascii_cased_component_matches_a_shim_folded_vpath() {
    let dir = scratch("mount");
    let root = dir.join("root");
    let backing = dir.join("backing");
    std::fs::create_dir_all(root.join("Data")).unwrap();
    std::fs::create_dir_all(&backing).unwrap();
    std::fs::write(backing.join(FILE), BYTES).unwrap();

    let abs = root.join("Data").join(UPPER_DIR).join(FILE);
    let vpath = vpath_from_the_shim(&root, &abs);

    // Configured as-authored, the way a mod-manager config spells it.
    let graph = MountGraph::new(vec![(
        format!("Data/{UPPER_DIR}"),
        Arc::new(DiskProvider::new(&backing)) as Arc<dyn Provider>,
    )])
    .unwrap();

    assert!(
        graph.getattr(VPath::at_default(&vpath)).unwrap().is_some(),
        "the mount prefix Data/{UPPER_DIR} did not match {vpath} — prefix \
         matching folds differently than the shim does"
    );
    assert_eq!(read_all(&graph, &vpath), BYTES);

    // The mount must also be discoverable by listing its parent, which is a
    // separate comparison (`mount_child_name`) over the same two spellings.
    let parent = vpath_from_the_shim(&root, &root.join("Data"));
    let names: Vec<String> = graph
        .readdir(VPath::at_default(&parent))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(
        names.iter().any(|n| n == UPPER_DIR),
        "listing {parent} must surface the deeper mount, got {names:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Two layers each contributing the same file under a non-ASCII-cased name,
/// spelled with different case. The merge map must collapse them to one entry
/// with the top layer winning — the same job `to_ascii_lowercase` silently
/// stopped doing for these names.
#[test]
fn layered_readdir_collapses_two_case_spellings_of_a_non_ascii_name() {
    let dir = scratch("layered");
    let bottom_dir = dir.join("bottom");
    let top_dir = dir.join("top");
    std::fs::create_dir_all(&bottom_dir).unwrap();
    std::fs::create_dir_all(&top_dir).unwrap();
    let upper_file = format!("{UPPER_DIR}.esp");
    let lower_file = format!("{LOWER_DIR}.esp");
    std::fs::write(bottom_dir.join(&upper_file), b"FROM-BOTTOM").unwrap();
    std::fs::write(top_dir.join(&lower_file), b"FROM-TOP").unwrap();

    let layered = LayeredProvider::new(
        Arc::new(DiskProvider::new(&top_dir)) as Arc<dyn Provider>,
        Arc::new(DiskProvider::new(&bottom_dir)) as Arc<dyn Provider>,
    );

    let entries = layered.readdir(VPath::at_default("")).unwrap();
    let matching: Vec<&str> = entries
        .iter()
        .map(|e| e.name.as_str())
        .filter(|n| n.to_lowercase() == lower_file)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "two case spellings of one Unicode name must merge to a single entry, got {entries:?}"
    );
    assert_eq!(
        matching[0], lower_file,
        "the top layer's spelling must win the merge"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Minimal Stored-only zip writer (the only method `ZipProvider` supports).

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn write_stored_zip(path: &Path, entry: &str, content: &[u8]) {
    let mut buf = Vec::new();
    let crc = crc32(content);
    let n = entry.len() as u16;
    buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(entry.as_bytes());
    buf.extend_from_slice(content);
    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 6]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&[0u8; 8]);
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(entry.as_bytes());
    let cd_size = buf.len() as u32 - cd_start;
    buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&cd_size.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    std::fs::File::create(path).unwrap().write_all(&buf).unwrap();
}
