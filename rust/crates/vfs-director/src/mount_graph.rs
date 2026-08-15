//! Compose several path-prefixed providers into one `Provider`.
//!
//! Before stage 2b task 3, this logic lived directly in `Director`: a flat
//! `Vec<Mount>` searched by prefix, merged by iteration order. Now that
//! `Director` maps `RootId` to exactly one provider (single lookup, no
//! merge), this is where that composition happens instead — one layer
//! below Director, as an ordinary `Provider` a session builds explicitly
//! before calling `Director::mount`. Nothing about *what* it does changed;
//! only where it lives.
//!
//! It survives because distinct, non-overlapping prefixes are a genuinely
//! different thing than layering at the same path: layering (several
//! sources answering the *same* path, later wins) has a direct replacement
//! in `vfs_compose::layered`/`stack_layers`. Placing one source at a
//! specific sub-path within a root (e.g. a single mod at `Data/SomeMod`,
//! distinct from the root's own content) does not — there is no existing
//! combinator that strips an outer prefix before forwarding to an inner
//! provider addressed at its own root (`vfs_compose::SubdirProvider` does
//! the opposite: it *descends into* an inner provider's subtree to expose
//! it as an outer root, the shape used for stripping a zip's wrapping
//! folder). So this module is the mount-placement primitive, relocated
//! rather than replaced.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_provider::{
    bad_fh, bad_request, map_io_err, not_a_dir, not_found, read_only, Access, Capabilities,
    DirEntry, Handle, Provider, SetAttr, Stat, VPath, KIND_DIR, OPEN_WRITE,
};

use crate::path::{normalize, strip_prefix};

struct Mount {
    prefix: String,
    backend: Arc<dyn Provider>,
}

/// If `mount_prefix` extends strictly below `path` (case-insensitively, on a
/// full path-segment boundary), returns the single next path component —
/// e.g. `path = "data"`, `mount_prefix = "data/a/b/c"` yields `"a"`, not
/// `"a/b/c"`. Returns `None` for a root mount (nothing to surface as a
/// child), for a mount at or above `path`, or for a mount on an unrelated
/// path that merely shares a string prefix (`"data2"` does not match
/// `"data"`).
///
/// Folds with [`vfs_core::fold`], matching `path::strip_prefix` and the shim's
/// own fold; see that function for why the comparison walks components instead
/// of slicing at a byte offset.
fn mount_child_name(path: &str, mount_prefix: &str) -> Option<String> {
    let mount_prefix = mount_prefix.trim_matches('/');
    if mount_prefix.is_empty() {
        return None;
    }
    let mut rest = mount_prefix;
    for have in path.split('/').filter(|c| !c.is_empty()) {
        // No `/` left in `rest` means the mount is at or above `path`, so it
        // has no child component to surface.
        let (head, tail) = rest.split_once('/')?;
        if vfs_core::fold(head) != vfs_core::fold(have) {
            return None;
        }
        rest = tail;
    }
    let name = rest.split('/').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// One open handle: the mount that answered, and the handle it returned.
type OpenEntry = (Arc<dyn Provider>, Handle);

/// One provider composed from several path-prefixed mounts. Later entries
/// override earlier ones on an overlapping path (last-registered tried
/// first), exactly the precedence `Director::mount` used to document
/// directly.
pub struct MountGraph {
    mounts: Vec<Mount>,
    opens: Mutex<HashMap<u64, OpenEntry>>,
    next: AtomicU64,
}

impl MountGraph {
    /// `mounts` in registration order (later wins on an overlapping path).
    /// A prefix that fails to normalize (escapes via `..`) is rejected.
    pub fn new(mounts: Vec<(String, Arc<dyn Provider>)>) -> Result<Self, i32> {
        let mounts = mounts
            .into_iter()
            .map(|(prefix, backend)| {
                let prefix = normalize(&prefix).map_err(|_| bad_request())?;
                Ok(Mount { prefix, backend })
            })
            .collect::<Result<Vec<_>, i32>>()?;
        Ok(MountGraph {
            mounts,
            opens: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        })
    }

    fn lookup(&self, h: Handle) -> Result<OpenEntry, i32> {
        let g = self.opens.lock().map_err(|_| map_io_err())?;
        let (p, i) = g.get(&h).ok_or_else(bad_fh)?;
        Ok((Arc::clone(p), *i))
    }
}

impl Provider for MountGraph {
    fn capabilities(&self) -> Capabilities {
        if self.mounts.is_empty() {
            return Capabilities::read_only();
        }
        let caps: Vec<Capabilities> = self.mounts.iter().map(|m| m.backend.capabilities()).collect();
        // Strongest access, weakest everything else — same reasoning as
        // `LayeredProvider`: this graph can serve a write whenever *some*
        // mount can, because every write routes to whichever mount actually
        // resolves and declares `ReadWrite`, not to all of them.
        let strongest_access = caps.iter().map(|c| c.access).max().unwrap();
        Capabilities {
            access: strongest_access,
            ..Capabilities::weakest(caps)
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let path = normalize(p.rel).map_err(|_| bad_request())?;
        for m in self.mounts.iter().rev() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            match m.backend.getattr(VPath::new(p.root, &rel))? {
                Some(s) => return Ok(Some(s)),
                None => continue,
            }
        }
        // Root always exists as a virtual dir if any mount is present.
        if path.is_empty() && !self.mounts.is_empty() {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        Ok(None)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let path = normalize(p.rel).map_err(|_| bad_request())?;
        let mut map: HashMap<String, DirEntry> = HashMap::new();
        let mut saw_dir = false;
        let mut not_dir = false;
        for m in self.mounts.iter() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            match m.backend.readdir(VPath::new(p.root, &rel)) {
                Ok(entries) => {
                    saw_dir = true;
                    for e in entries {
                        map.insert(vfs_core::fold(&e.name), e);
                    }
                }
                Err(e) if e == not_found() => {}
                Err(e) if e == not_a_dir() => not_dir = true,
                Err(e) => return Err(e),
            }
        }
        // A mount registered *below* the queried directory (e.g. `data/somemod`
        // while listing `data`) is otherwise invisible to readdir: it can be
        // opened by a known path but never discovered. Surface the mount's
        // next path component as a synthetic directory entry, alongside
        // whatever a provider already returned above. A provider-supplied
        // entry for the same name always wins — the `contains_key` check
        // below skips the mount entirely, without even probing it — so a
        // mount that shadows a real subdirectory does not clobber it with a
        // placeholder.
        let mut mount_derived = false;
        for m in self.mounts.iter() {
            let Some(name) = mount_child_name(&path, &m.prefix) else {
                continue;
            };
            let key = vfs_core::fold(&name);
            if map.contains_key(&key) {
                continue;
            }
            // A registered prefix alone does not prove the mount resolves to
            // anything — a mount whose backend has nothing at its own root
            // (e.g. a `DiskProvider` pointed at a directory that no longer
            // exists) would otherwise list a child that opens into nothing.
            // Probing `getattr` on the mount's own root (empty relative
            // path) both confirms it resolves and supplies the entry's real
            // kind/size/mtime, so a single-file mount is surfaced as a file
            // rather than an assumed, possibly-wrong `KIND_DIR`. Bounded by
            // the mount count already walked twice per `readdir` call, so
            // this adds no new order of growth.
            if let Ok(Some(stat)) = m.backend.getattr(VPath::new(p.root, "")) {
                mount_derived = true;
                map.insert(key, DirEntry { name, stat });
            }
        }
        if !saw_dir && !mount_derived {
            if not_dir {
                return Err(not_a_dir());
            }
            // Empty virtual root
            if path.is_empty() && !self.mounts.is_empty() {
                return Ok(Vec::new());
            }
            return Err(not_found());
        }
        let mut out: Vec<DirEntry> = map.into_values().collect();
        out.sort_by_key(|a| vfs_core::fold(&a.name));
        Ok(out)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let path = normalize(p.rel).map_err(|_| bad_request())?;
        for m in self.mounts.iter().rev() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            if flags & OPEN_WRITE != 0 && m.backend.capabilities().access < Access::ReadWrite {
                // Same discovery instrument `Director::open` records for a
                // bare provider: without this, a graph containing *any*
                // writable mount reports `ReadWrite` in aggregate
                // (`capabilities()` takes the strongest child), so
                // `Director`'s own coarse pre-check never fires and this was
                // the only place left that could still see the rejection
                // for this specific mount.
                crate::io_stats::record_rejected_write(&path);
                return Err(read_only());
            }
            match m.backend.open(VPath::new(p.root, &rel), flags) {
                Ok((bh, size, is_dir_flag)) => {
                    let h = self.next.fetch_add(1, Ordering::Relaxed);
                    self.opens
                        .lock()
                        .map_err(|_| map_io_err())?
                        .insert(h, (Arc::clone(&m.backend), bh));
                    return Ok((h, size, is_dir_flag));
                }
                Err(e) if e == not_found() => continue,
                Err(e) => return Err(e),
            }
        }
        Err(not_found())
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (backend, bh) = self.lookup(h)?;
        backend.read_at(bh, offset, buf)
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let (backend, bh) = {
            let mut g = self.opens.lock().map_err(|_| map_io_err())?;
            g.remove(&h).ok_or_else(bad_fh)?
        };
        backend.close(bh)
    }

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let (backend, bh) = self.lookup(h)?;
        backend.write_at(bh, offset, buf)
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        let (backend, bh) = self.lookup(h)?;
        backend.set_len(bh, len)
    }

    fn flush(&self, h: Handle) -> Result<(), i32> {
        let (backend, bh) = self.lookup(h)?;
        backend.flush(bh)
    }

    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        let path = normalize(p.rel).map_err(|_| bad_request())?;
        for m in self.mounts.iter().rev() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            return m.backend.mkdir(VPath::new(p.root, &rel));
        }
        Err(not_found())
    }

    fn remove(&self, p: VPath) -> Result<(), i32> {
        let path = normalize(p.rel).map_err(|_| bad_request())?;
        for m in self.mounts.iter().rev() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            return m.backend.remove(VPath::new(p.root, &rel));
        }
        Err(not_found())
    }

    /// Both paths must resolve into the same mount; a rename that would
    /// cross mounts is rejected as a bad request rather than silently
    /// picking one side's mount.
    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        let from_path = normalize(from.rel).map_err(|_| bad_request())?;
        let to_path = normalize(to.rel).map_err(|_| bad_request())?;
        for m in self.mounts.iter().rev() {
            let (Some(from_rel), Some(to_rel)) = (
                strip_prefix(&from_path, &m.prefix),
                strip_prefix(&to_path, &m.prefix),
            ) else {
                continue;
            };
            return m
                .backend
                .rename(VPath::new(from.root, &from_rel), VPath::new(to.root, &to_rel));
        }
        Err(bad_request())
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        let path = normalize(p.rel).map_err(|_| bad_request())?;
        for m in self.mounts.iter().rev() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            return m.backend.set_attr(VPath::new(p.root, &rel), attr);
        }
        Err(not_found())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_provider::{RootId, KIND_FILE, OPEN_READ};

    fn graph(mounts: Vec<(&str, Arc<dyn Provider>)>) -> MountGraph {
        MountGraph::new(mounts.into_iter().map(|(p, b)| (p.to_string(), b)).collect()).unwrap()
    }

    #[test]
    fn readdir_surfaces_a_mount_registered_below_the_queried_directory() {
        // A mount at "data/somemod" (no mount at "data" itself) must appear
        // as a synthetic "somemod" entry when listing "data" — otherwise a
        // non-root mount can be opened by a known path but never discovered.
        let g = graph(vec![(
            "data/somemod",
            Arc::new(vfs_compose::InlineProvider::from_files([("f", b"x".as_slice())])),
        )]);

        let entries = g.readdir(VPath::at_default("data")).unwrap();
        assert!(
            entries.iter().any(|e| e.name == "somemod" && e.stat.kind == KIND_DIR),
            "expected a synthetic 'somemod' dir entry, got {entries:?}"
        );
    }

    #[test]
    fn readdir_contributes_only_the_next_component_of_a_deeper_mount() {
        // A mount several levels below the queried directory
        // ("data/a/b/c") must contribute only "a", not "a/b/c".
        let g = graph(vec![(
            "data/a/b/c",
            Arc::new(vfs_compose::InlineProvider::from_files([("f", b"x".as_slice())])),
        )]);

        let entries = g.readdir(VPath::at_default("data")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a");
    }

    #[test]
    fn readdir_does_not_duplicate_a_name_a_provider_already_supplies() {
        // The parent mount already serves a real "somemod" *file* entry —
        // deliberately a file, not a directory, so its stat (KIND_FILE,
        // nonzero size) is distinguishable from the synthetic placeholder a
        // naive implementation would produce (always KIND_DIR, size 0).
        let g = graph(vec![
            (
                "data",
                Arc::new(vfs_compose::InlineProvider::from_files([(
                    "somemod",
                    b"real-file-not-a-directory".as_slice(),
                )])),
            ),
            (
                "data/somemod",
                Arc::new(vfs_compose::InlineProvider::from_files([("f", b"y".as_slice())])),
            ),
        ]);

        let entries = g.readdir(VPath::at_default("data")).unwrap();
        let matches: Vec<_> = entries.iter().filter(|e| e.name == "somemod").collect();
        assert_eq!(matches.len(), 1, "expected exactly one 'somemod' entry, got {entries:?}");
        assert_eq!(
            matches[0].stat.kind, KIND_FILE,
            "the real provider-supplied file entry must survive untouched, \
             not be reshaped into a directory placeholder: {:?}",
            matches[0]
        );
    }

    #[test]
    fn readdir_skips_a_synthetic_entry_when_the_deeper_mounts_own_root_does_not_resolve() {
        // A registered prefix alone does not prove the mount serves
        // anything. `InlineProvider` always answers its own root as an
        // (empty) directory regardless of content, so it can't demonstrate
        // this; a `DiskProvider` pointed at a directory that was never
        // created genuinely reports `None` for `getattr("")`, exactly the
        // "registered but resolves to nothing" case the probe must catch.
        let dir = std::env::temp_dir()
            .join(format!("vfs-mg-nonexistent-mount-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Deliberately do not create `dir` — the mount's own root must not
        // resolve.
        let g = graph(vec![("data/ghostmod", Arc::new(crate::DiskProvider::new(&dir)))]);

        let entries = g.readdir(VPath::at_default("data")).unwrap_or_default();
        assert!(
            entries.iter().all(|e| e.name != "ghostmod"),
            "a mount whose own root does not resolve must not list a child \
             the user would only open into nothing: {entries:?}"
        );
    }

    #[test]
    fn readdir_derives_the_synthetic_entrys_kind_from_the_mount_provider() {
        // A single-file mount (the backend's own root, addressed by an
        // empty relative path, resolves to a file rather than a directory)
        // must be surfaced as a file with its real size, not an assumed
        // KIND_DIR/0 placeholder — the same probe that confirms the mount
        // resolves at all also supplies its real shape.
        let dir = std::env::temp_dir().join(format!("vfs-mg-filemount-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("payload.bin"), b"12345").unwrap();
        // Mount a DiskProvider whose *root* is the file itself — resolve("")
        // returns the provider's root path, so this mount's own root stats
        // as a file, not a directory.
        let g = graph(vec![(
            "data/singlefile",
            Arc::new(crate::DiskProvider::new(dir.join("payload.bin"))),
        )]);

        let entries = g.readdir(VPath::at_default("data")).unwrap();
        let e = entries
            .iter()
            .find(|e| e.name == "singlefile")
            .unwrap_or_else(|| panic!("expected a 'singlefile' entry, got {entries:?}"));
        assert_eq!(e.stat.kind, KIND_FILE, "expected the file-shaped mount to surface as KIND_FILE");
        assert_eq!(e.stat.size, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn readdir_of_a_mounts_own_directory_does_not_synthesize_a_self_entry() {
        // A mount whose prefix is exactly the queried path must contribute
        // only its own provider's real entries, never a synthetic entry
        // for itself.
        let g = graph(vec![(
            "data",
            Arc::new(vfs_compose::InlineProvider::from_files([("a.txt", b"x".as_slice())])),
        )]);

        let entries = g.readdir(VPath::at_default("data")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.txt");
    }

    #[test]
    fn readdir_of_the_root_surfaces_only_the_first_component_of_a_deep_mount() {
        // Listing the virtual root with only a mount two levels down
        // present (nothing mounted at "" or at "data") must still surface
        // "data" — the boundary case most likely to be disturbed by this
        // logic's relocation out of `Director`.
        let g = graph(vec![(
            "data/somemod",
            Arc::new(vfs_compose::InlineProvider::from_files([("f", b"x".as_slice())])),
        )]);

        let entries = g.readdir(VPath::at_default("")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "data");
        assert_eq!(entries[0].stat.kind, KIND_DIR);
    }

    #[test]
    fn mount_prefix_matching_is_case_insensitive() {
        // A mount configured as "Data/SomeMod" (the spelling Mod Organizer
        // style configs use) must still resolve a lookup for the
        // lowercased vpath the shim always produces.
        let g = graph(vec![(
            "Data/SomeMod",
            Arc::new(vfs_compose::InlineProvider::from_files([("f.txt", b"x".as_slice())])),
        )]);

        assert!(g.getattr(VPath::at_default("data/somemod/f.txt")).unwrap().is_some());
        let (h, size, is_dir_flag) =
            g.open(VPath::at_default("data/somemod/f.txt"), OPEN_READ).unwrap();
        assert!(!is_dir_flag);
        assert_eq!(size, 1);
        g.close(h).unwrap();
    }

    #[test]
    fn a_specific_root_is_forwarded_unchanged_through_a_deeper_mount() {
        // Task 3: `VPath::root` must survive the prefix rewrite, not just
        // `rel` — a graph used for a non-default root must pass that root
        // through to the mounts it composes.
        let g = graph(vec![(
            "data/somemod",
            Arc::new(vfs_compose::InlineProvider::from_files([("f", b"x".as_slice())])),
        )]);
        let st = g
            .getattr(VPath::new(RootId(1), "data/somemod/f"))
            .unwrap()
            .unwrap();
        assert_eq!(st.kind, KIND_FILE);
    }

    #[test]
    fn open_for_write_against_a_read_only_mount_is_recorded_even_though_a_sibling_mount_is_writable() {
        // Task 3 review, Finding 1: `capabilities()` reports the *strongest*
        // child access, so a graph with any writable source reports
        // `ReadWrite` in aggregate — `Director`'s own coarse pre-check never
        // fires for it, which makes this per-mount recording the only place
        // left that can see a write refused by one *specific* mount. Gate
        // 4's whole workflow ("launch, ask what was rejected, add a
        // provider for it") depends on this staying discoverable.
        let dir = std::env::temp_dir()
            .join(format!("vfs-mg-rejected-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = graph(vec![
            ("rw", Arc::new(crate::DiskProvider::new(&dir)) as Arc<dyn Provider>),
            (
                "ro",
                Arc::new(vfs_compose::InlineProvider::from_files([("f", b"x".as_slice())])),
            ),
        ]);
        assert_eq!(
            g.capabilities().access,
            vfs_provider::Access::ReadWrite,
            "the graph as a whole must report writable — the masking Finding 1 warned about"
        );

        crate::io_stats::reset_rejected_writes();
        let result = g.open(VPath::at_default("ro/f"), vfs_provider::OPEN_WRITE);
        assert_eq!(result, Err(vfs_provider::ST_READ_ONLY));
        let rejected = crate::io_stats::rejected_writes();
        assert!(
            rejected.iter().any(|(path, count)| path == "ro/f" && *count >= 1),
            "a write refused by one mount in a graph containing a writable \
             sibling must still be discoverable, got {rejected:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
