//! Userspace FUSE kernel: one provider per root, global file handles.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use crate::registry::{RegistryGenSink, RegistryGeneration, RegistryHost};
use vfs_compose::path::normalize;
use vfs_provider::OPEN_APPEND;
use vfs_provider::{
    bad_request, is_dir, map_io_err, not_found, read_only, Access, DirEntry, Handle, Provider,
    RootId, SetAttr, Stat, VPath, OPEN_WRITE,
};

struct OpenRec {
    backend: Arc<dyn Provider>,
    bh: Handle,
    size: u64,
    is_dir: bool,
    /// Present, and equal to the file's size at open time, iff the handle
    /// was opened `OPEN_APPEND`. The director owns this cursor — providers
    /// stay purely positional — and a write on such a handle ignores the
    /// caller-supplied offset in favor of the cursor, advancing it by the
    /// number of bytes written.
    ///
    /// Known limitation: `Director::write` reads the cursor and writes it
    /// back under two separate lock acquisitions with the provider call
    /// unlocked in between, so two writes racing on *the same* `fh` — not
    /// just two distinct handles appending to the same file — can interleave
    /// incorrectly. Games write logs from a single handle used
    /// single-threaded, so this has not mattered; a per-path cursor (keyed
    /// by resolved provider + relative path rather than by `fh`), or holding
    /// the lock across the provider call, is the fix if it ever does.
    cursor: Option<u64>,
}

/// Userspace FUSE kernel. Maps each session root to exactly one provider and
/// hosts global file handles for getattr/open/read/write.
///
/// Resolution is a single map lookup, not a search: stage 2b task 3 deleted
/// the layer-ordered mount list and its reverse-iteration merge. Composition
/// across several sources — layering at the same path, or placing one at a
/// distinct sub-path within a root — now happens explicitly in the provider
/// graph *before* it reaches `mount` (see [`vfs_compose::MountGraph`]
/// and `vfs_compose::stack_layers`), where it is visible rather than
/// implicit here.
pub struct Director {
    roots: Mutex<BTreeMap<RootId, Arc<dyn Provider>>>,
    opens: Mutex<HashMap<u64, OpenRec>>,
    next_fh: AtomicU64,
    /// Bumped by every [`Director::mount`] and [`Director::unmount`], and
    /// reported with every open ([`Director::open_info`]): what lets a
    /// client tell an immutable file it has cached from the one a remount
    /// put at the same path.
    mount_gen: AtomicU32,
    /// The session's registry overlay, when registry virtualisation is on: what the ring's
    /// registry opcodes (15-22) answer from. `None` answers them `ST_NOT_SUPPORTED`.
    registry: RwLock<Option<Arc<RegistryHost>>>,
    /// The registry generation every ring of this director publishes ([`RegistryGeneration`]).
    /// Shared with each attached [`RegistryHost`], whose writes move it.
    reg_gen: Arc<RegistryGeneration>,
}

/// What [`Director::open_info`] says about a new handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenInfo {
    pub fh: u64,
    pub size: u64,
    pub is_dir: bool,
    /// The provider holding the handle says its bytes cannot change while it
    /// is open ([`Provider::is_immutable`]). Never true for a write open.
    pub immutable: bool,
    /// [`Director::mount_gen`] when the handle was opened.
    pub mount_gen: u32,
}

impl Default for Director {
    fn default() -> Self {
        Self::new()
    }
}

impl Director {
    pub fn new() -> Self {
        Director {
            roots: Mutex::new(BTreeMap::new()),
            opens: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            mount_gen: AtomicU32::new(1),
            registry: RwLock::new(None),
            reg_gen: Arc::new(RegistryGeneration::new()),
        }
    }

    /// Attach (`Some`) or detach (`None`) the session's registry overlay. Detaching does not
    /// flush: the host saves on its own, and when its last reference drops (which, if this
    /// held it, happens here, outside the lock).
    pub fn set_registry(&self, host: Option<Arc<RegistryHost>>) {
        if let Some(h) = &host {
            h.publish_to(self.reg_gen.clone());
        }
        let old = {
            let mut r = self.registry.write().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *r, host)
        };
        self.registry_changed();
        drop(old);
    }

    /// The current registry generation: see [`RegistryGeneration`].
    pub fn registry_generation(&self) -> u64 {
        self.reg_gen.current()
    }

    /// Publish the registry generation to `sink` now and after every change. A ring server
    /// calls this before it hands its ring to anyone.
    pub fn add_registry_sink(&self, sink: Weak<dyn RegistryGenSink>) {
        self.reg_gen.add_sink(sink);
    }

    /// The registry overlay changed outside a host write (a layer attached or detached): bump
    /// and publish the generation. Host writes publish on their own ([`RegistryHost`]).
    pub(crate) fn registry_changed(&self) {
        self.reg_gen.changed();
    }

    /// The attached registry overlay, if any.
    pub fn registry(&self) -> Option<Arc<RegistryHost>> {
        self.registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The current mount generation: see the field's docs.
    pub fn mount_gen(&self) -> u32 {
        self.mount_gen.load(Ordering::Acquire)
    }

    /// Set (or replace) the single provider serving `root`. Composition of
    /// several sources into that one provider is the caller's job — use
    /// [`crate::compose_root`] for it, so every route composes the same way.
    ///
    /// **A root mounted here is not a root the session knows about, and the
    /// two must not both own one.** This is legitimate for a provider the
    /// session cannot express — `skyrim-live`'s root 1, whose counters wrap
    /// the *composed* provider, which `Session` has no hook for. The hazard
    /// runs the other way: a later `Session::mount_at(RootId(1), …)` /
    /// `set_write_layer_at` would compose that root from the session's own
    /// (empty) inputs and replace what was mounted here — dropping the
    /// counters, the overlay, or both. `Session` refuses that rather than
    /// doing it silently (`ST_EXISTS`), so the two owners cannot fight; call
    /// [`Director::unmount`] first if a hand-mounted root really should
    /// become session-composed.
    ///
    /// A host that *can* express its root through the session should always
    /// prefer `Session::mount` / `Session::set_root_mounts` /
    /// `Session::set_write_layer_at`: they compose the mounts and the write
    /// layer together, and a graph built outside the session drops whichever
    /// half it did not know about — copy-on-write, most damagingly, which
    /// nothing but a write test notices (gate 4, Task 6b).
    pub fn mount(&self, root: RootId, backend: Arc<dyn Provider>) -> Result<(), i32> {
        let mut roots = self.roots.lock().map_err(|_| map_io_err())?;
        roots.insert(root, backend);
        // Bumped inside the critical section that swaps the provider, so an
        // open (which reads both under the same lock: `provider_and_gen`)
        // can never pair the new provider with the old generation.
        self.mount_gen.fetch_add(1, Ordering::AcqRel);
        drop(roots);
        Ok(())
    }

    /// Remove whatever provider serves `root`, if any (used when a session
    /// rebuilds that root's composition).
    pub fn unmount(&self, root: RootId) -> Result<(), i32> {
        let mut roots = self.roots.lock().map_err(|_| map_io_err())?;
        roots.remove(&root);
        // In the same critical section as the removal: see `mount`.
        self.mount_gen.fetch_add(1, Ordering::AcqRel);
        drop(roots);
        Ok(())
    }

    /// Whether anything currently serves `root`. `Session` uses this to
    /// refuse composing over a root someone mounted directly — see
    /// [`Director::mount`].
    pub fn serves(&self, root: RootId) -> Result<bool, i32> {
        Ok(self
            .roots
            .lock()
            .map_err(|_| map_io_err())?
            .contains_key(&root))
    }

    /// The provider serving `root` and the mount generation it belongs to,
    /// read as one snapshot: `mount`/`unmount` change both under this lock.
    fn provider_and_gen(&self, root: RootId) -> Result<(Option<Arc<dyn Provider>>, u32), i32> {
        let roots = self.roots.lock().map_err(|_| map_io_err())?;
        Ok((
            roots.get(&root).cloned(),
            self.mount_gen.load(Ordering::Acquire),
        ))
    }

    fn provider_for(&self, root: RootId) -> Result<Option<Arc<dyn Provider>>, i32> {
        Ok(self
            .roots
            .lock()
            .map_err(|_| map_io_err())?
            .get(&root)
            .cloned())
    }

    pub fn getattr(&self, root: RootId, path: &str) -> Result<Option<Stat>, i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        match self.provider_for(root)? {
            Some(p) => p.getattr(VPath::new(root, &path)),
            None => Ok(None),
        }
    }

    pub fn readdir(&self, root: RootId, path: &str) -> Result<Vec<DirEntry>, i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        match self.provider_for(root)? {
            Some(p) => p.readdir(VPath::new(root, &path)),
            None => Err(not_found()),
        }
    }

    /// The stored spelling of each component of `path` from the `skip`-th on
    /// — the names a listing of each parent shows. A component nothing has is
    /// answered as it was asked, so the reply always has one name per
    /// component asked for.
    ///
    /// For final-path queries: one call names a whole path with no listing
    /// crossing the ring, and `skip` lets a caller that already knows how the
    /// leading directories are spelled ask only about the rest.
    pub fn stored_names(&self, root: RootId, path: &str, skip: usize) -> Result<Vec<String>, i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let provider = self.provider_for(root)?.ok_or_else(not_found)?;
        let mut names = Vec::new();
        let mut end = 0;
        for (i, comp) in path.split('/').filter(|c| !c.is_empty()).enumerate() {
            // `normalize` leaves single separators, so the prefix ending at
            // this component is a slice of `path`.
            end = if i == 0 {
                comp.len()
            } else {
                end + 1 + comp.len()
            };
            if i < skip {
                continue;
            }
            let stored =
                vfs_compose::stored_name(provider.as_ref(), VPath::new(root, &path[..end]))?;
            names.push(stored.unwrap_or_else(|| comp.to_string()));
        }
        Ok(names)
    }

    /// Returns `(fh, size, is_dir)`.
    pub fn open(&self, root: RootId, path: &str, flags: u32) -> Result<(u64, u64, bool), i32> {
        self.open_info(root, path, flags)
            .map(|o| (o.fh, o.size, o.is_dir))
    }

    /// [`Director::open`], and also whether the new handle's bytes are
    /// immutable and under which mount generation it was opened — what the
    /// ring's open reply carries to the shim, whose read cache serves only
    /// immutable handles.
    pub fn open_info(&self, root: RootId, path: &str, flags: u32) -> Result<OpenInfo, i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        // The provider and its generation as one snapshot. Reading the
        // generation apart from the provider (it used to be read first) let
        // an open racing a remount label the new content with the old
        // generation — the key under which the client had cached the old
        // content, so it served the old bytes.
        let (provider, mount_gen) = self.provider_and_gen(root)?;
        let provider = provider.ok_or_else(not_found)?;
        if flags & OPEN_WRITE != 0 && provider.capabilities().access < Access::ReadWrite {
            // A configuration fact, not a caller mistake: this root has no
            // writable provider. Recorded by path so a later `vfs stats`
            // pass can surface it for discovery.
            vfs_compose::record_rejected_write(&path);
            return Err(read_only());
        }
        let (bh, size, is_dir_flag) = provider.open(VPath::new(root, &path), flags)?;
        let immutable = flags & OPEN_WRITE == 0 && !is_dir_flag && provider.is_immutable(bh);
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        let cursor = if flags & OPEN_APPEND != 0 {
            Some(size)
        } else {
            None
        };
        self.opens.lock().map_err(|_| map_io_err())?.insert(
            fh,
            OpenRec {
                backend: provider,
                bh,
                size,
                is_dir: is_dir_flag,
                cursor,
            },
        );
        Ok(OpenInfo {
            fh,
            size,
            is_dir: is_dir_flag,
            immutable,
            mount_gen,
        })
    }

    pub fn read(&self, fh: u64, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (backend, bh, size, is_dir_flag) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let rec = g.get(&fh).ok_or_else(vfs_provider::bad_fh)?;
            if rec.is_dir {
                return Err(is_dir());
            }
            (Arc::clone(&rec.backend), rec.bh, rec.size, rec.is_dir)
        };
        let _ = (size, is_dir_flag);
        backend.read_at(bh, offset, buf)
    }

    pub fn close(&self, fh: u64) -> Result<(), i32> {
        let rec = {
            let mut g = self.opens.lock().map_err(|_| map_io_err())?;
            g.remove(&fh).ok_or_else(vfs_provider::bad_fh)?
        };
        rec.backend.close(rec.bh)
    }

    /// Positional write, except on an append handle: there, the
    /// caller-supplied `offset` is ignored and the handle's own cursor is
    /// used instead, then advanced by the bytes actually written. See
    /// `OpenRec::cursor` for the caveat about two writes racing on the same
    /// handle.
    pub fn write(&self, fh: u64, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let (backend, bh, effective_offset) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let rec = g.get(&fh).ok_or_else(vfs_provider::bad_fh)?;
            if rec.is_dir {
                return Err(is_dir());
            }
            (
                Arc::clone(&rec.backend),
                rec.bh,
                rec.cursor.unwrap_or(offset),
            )
        };
        let result = backend.write_at(bh, effective_offset, buf);
        if let Ok(n) = result {
            if let Ok(mut g) = self.opens.lock() {
                if let Some(rec) = g.get_mut(&fh) {
                    if rec.cursor.is_some() {
                        rec.cursor = Some(effective_offset + n as u64);
                    }
                    rec.size = rec.size.max(effective_offset + n as u64);
                }
            }
        }
        result
    }

    /// Truncating or extending an append handle clamps its cursor to the new
    /// length rather than resetting it: a cursor already at or below `len`
    /// is still correct and must not jump forward, but one left past a
    /// shorter `len` would otherwise leave a hole on the next append.
    pub fn set_len(&self, fh: u64, len: u64) -> Result<(), i32> {
        let (backend, bh) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let rec = g.get(&fh).ok_or_else(vfs_provider::bad_fh)?;
            (Arc::clone(&rec.backend), rec.bh)
        };
        let result = backend.set_len(bh, len);
        if result.is_ok() {
            if let Ok(mut g) = self.opens.lock() {
                if let Some(rec) = g.get_mut(&fh) {
                    rec.size = len;
                    if let Some(c) = rec.cursor.as_mut() {
                        *c = (*c).min(len);
                    }
                }
            }
        }
        result
    }

    pub fn flush(&self, fh: u64) -> Result<(), i32> {
        let (backend, bh) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let rec = g.get(&fh).ok_or_else(vfs_provider::bad_fh)?;
            (Arc::clone(&rec.backend), rec.bh)
        };
        backend.flush(bh)
    }

    pub fn mkdir(&self, root: RootId, path: &str) -> Result<(), i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let provider = self.provider_for(root)?.ok_or_else(not_found)?;
        provider.mkdir(VPath::new(root, &path))
    }

    pub fn remove(&self, root: RootId, path: &str) -> Result<(), i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let provider = self.provider_for(root)?.ok_or_else(not_found)?;
        provider.remove(VPath::new(root, &path))
    }

    /// `from` and `to` are both resolved under `root` — a single root
    /// parameter makes a cross-root rename structurally impossible rather
    /// than a case to reject.
    pub fn rename(&self, root: RootId, from: &str, to: &str) -> Result<(), i32> {
        let from = normalize(from).map_err(|_| bad_request())?;
        let to = normalize(to).map_err(|_| bad_request())?;
        let provider = self.provider_for(root)?.ok_or_else(bad_request)?;
        provider.rename(VPath::new(root, &from), VPath::new(root, &to))
    }

    pub fn set_attr(&self, root: RootId, path: &str, attr: SetAttr) -> Result<(), i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let provider = self.provider_for(root)?.ok_or_else(not_found)?;
        provider.set_attr(VPath::new(root, &path), attr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_provider::OPEN_READ;

    /// What the shim's read cache relies on: a handle is reported immutable
    /// only when the provider that holds it is, even inside an overlay whose
    /// own capabilities say mutable — and never for a write open or a file
    /// that has been copied up into the writable layer.
    #[test]
    fn open_info_reports_immutability_per_handle_through_an_overlay() {
        let base = vfs_compose::MountGraph::new(vec![(
            String::new(),
            Arc::new(vfs_compose::InlineProvider::from_files([
                ("a.esm", b"base-a".as_slice()),
                ("b.ini", b"base-b".as_slice()),
            ])) as Arc<dyn Provider>,
        )])
        .unwrap();
        let upper = vfs_compose::MemoryProvider::new();
        let overlay = vfs_compose::OverlayProvider::new(Arc::new(base), upper).unwrap();
        let d = Director::new();
        d.mount(RootId::DEFAULT, Arc::new(overlay)).unwrap();

        let a = d.open_info(RootId::DEFAULT, "a.esm", OPEN_READ).unwrap();
        assert!(a.immutable, "a base file of an immutable base is immutable");
        let b = d.open_info(RootId::DEFAULT, "b.ini", OPEN_READ).unwrap();
        assert!(b.immutable);

        // A write open copies `b.ini` up; neither it nor any later read open
        // of that path (now served by the upper) is immutable.
        let w = d.open_info(RootId::DEFAULT, "b.ini", OPEN_WRITE).unwrap();
        assert!(!w.immutable, "a write open is never immutable");
        d.write(w.fh, 0, b"upper").unwrap();
        d.close(w.fh).unwrap();
        let b2 = d.open_info(RootId::DEFAULT, "b.ini", OPEN_READ).unwrap();
        assert!(
            !b2.immutable,
            "a copied-up file is served by the mutable upper"
        );
        for h in [a.fh, b.fh, b2.fh] {
            d.close(h).unwrap();
        }
    }

    /// **No generation ever names two contents**, however opens and
    /// remounts interleave. Two providers with the same path at the same
    /// size but different bytes are mounted in turn as fast as possible
    /// while four threads open and read; every (generation → bytes) pairing
    /// any open reports must be the only one for that generation. With the
    /// generation read apart from the provider this found thousands of
    /// generations tied to both contents in a few million opens.
    #[test]
    fn a_remount_racing_opens_never_pairs_new_content_with_an_old_generation() {
        use std::collections::HashMap;
        use std::sync::atomic::AtomicBool;
        let a: Arc<dyn Provider> = Arc::new(vfs_compose::InlineProvider::from_files([(
            "f",
            b"AAAA".as_slice(),
        )]));
        let b: Arc<dyn Provider> = Arc::new(vfs_compose::InlineProvider::from_files([(
            "f",
            b"BBBB".as_slice(),
        )]));
        let d = Director::new();
        d.mount(RootId::DEFAULT, Arc::clone(&a)).unwrap();
        let stop = AtomicBool::new(false);
        let seen: Mutex<HashMap<u32, u8>> = Mutex::new(HashMap::new());
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut flip = false;
                while !stop.load(Ordering::Relaxed) {
                    let p = if flip { &a } else { &b };
                    d.mount(RootId::DEFAULT, Arc::clone(p)).unwrap();
                    flip = !flip;
                }
            });
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        let mut local: Vec<(u32, u8)> = Vec::new();
                        while std::time::Instant::now() < deadline {
                            let o = d.open_info(RootId::DEFAULT, "f", OPEN_READ).unwrap();
                            let mut buf = [0u8; 4];
                            d.read(o.fh, 0, &mut buf).unwrap();
                            d.close(o.fh).unwrap();
                            local.push((o.mount_gen, buf[0]));
                        }
                        local
                    })
                })
                .collect();
            let all: Vec<(u32, u8)> = readers
                .into_iter()
                .flat_map(|r| r.join().unwrap())
                .collect();
            // Stop the remounts before asserting, or a failure never ends
            // the scope.
            stop.store(true, Ordering::Relaxed);
            let mut seen = seen.lock().unwrap();
            let mut both = 0usize;
            for (gen, byte) in all {
                if *seen.entry(gen).or_insert(byte) != byte {
                    both += 1;
                }
            }
            assert_eq!(
                both, 0,
                "{both} opens reported a generation already seen with other bytes"
            );
        });
        assert!(
            seen.lock().unwrap().len() > 1,
            "the remounts must have interleaved"
        );
    }

    /// A mutable provider's handles are mutable, and a remount moves the
    /// generation an open reports.
    #[test]
    fn open_info_reports_a_mutable_provider_and_the_mount_generation() {
        let dir = vfs_testkit::scratch_path("vfs-dirgen");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f"), b"disk").unwrap();
        let d = Director::new();
        d.mount(
            RootId::DEFAULT,
            Arc::new(vfs_compose::DiskProvider::new(&dir)),
        )
        .unwrap();
        let disk = d.open_info(RootId::DEFAULT, "f", OPEN_READ).unwrap();
        assert!(!disk.immutable, "a real directory can change underneath us");

        d.mount(
            RootId::DEFAULT,
            Arc::new(vfs_compose::InlineProvider::from_files([(
                "f",
                b"x".as_slice(),
            )])),
        )
        .unwrap();
        let inline = d.open_info(RootId::DEFAULT, "f", OPEN_READ).unwrap();
        assert!(inline.immutable);
        assert_ne!(
            disk.mount_gen, inline.mount_gen,
            "a remount must change the generation an open reports"
        );
        let again = d.open_info(RootId::DEFAULT, "f", OPEN_READ).unwrap();
        assert_eq!(inline.mount_gen, again.mount_gen, "and nothing else may");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_for_write_against_a_read_only_provider_is_read_only_not_bad_request() {
        // InlineProvider is Access::Read.
        let d = Director::new();
        d.mount(
            RootId::DEFAULT,
            Arc::new(vfs_compose::InlineProvider::from_files([(
                "f",
                b"x".as_slice(),
            )])),
        )
        .unwrap();
        assert_eq!(
            d.open(RootId::DEFAULT, "f", OPEN_WRITE),
            Err(vfs_provider::ST_READ_ONLY)
        );
    }

    #[test]
    fn a_rejected_write_is_recorded_for_discovery() {
        let d = Director::new();
        d.mount(
            RootId::DEFAULT,
            Arc::new(vfs_compose::InlineProvider::from_files([(
                "f",
                b"x".as_slice(),
            )])),
        )
        .unwrap();
        vfs_compose::reset_rejected_writes();
        let _ = d.open(RootId::DEFAULT, "f", OPEN_WRITE);
        let rejected = vfs_compose::rejected_writes();
        assert!(
            rejected
                .iter()
                .any(|(path, count)| path == "f" && *count >= 1),
            "a rejected write must be discoverable, got {rejected:?}"
        );
    }

    #[test]
    fn write_then_read_through_the_director_round_trips() {
        let dir = vfs_testkit::scratch_path("vfs-dirw");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = Director::new();
        d.mount(
            RootId::DEFAULT,
            Arc::new(vfs_compose::DiskProvider::new(&dir)),
        )
        .unwrap();

        let (fh, _, _) = d
            .open(
                RootId::DEFAULT,
                "w.txt",
                OPEN_WRITE | vfs_provider::OPEN_CREATE,
            )
            .unwrap();
        assert_eq!(d.write(fh, 0, b"hello").unwrap(), 5);
        d.close(fh).unwrap();

        let (fh, size, _) = d.open(RootId::DEFAULT, "w.txt", OPEN_READ).unwrap();
        assert_eq!(size, 5);
        let mut buf = [0u8; 8];
        let n = d.read(fh, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
        d.close(fh).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_handles_land_at_end_of_file() {
        let dir = vfs_testkit::scratch_path("vfs-dira");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log.txt"), b"one").unwrap();
        let d = Director::new();
        d.mount(
            RootId::DEFAULT,
            Arc::new(vfs_compose::DiskProvider::new(&dir)),
        )
        .unwrap();

        let (fh, _, _) = d
            .open(
                RootId::DEFAULT,
                "log.txt",
                OPEN_WRITE | vfs_provider::OPEN_APPEND,
            )
            .unwrap();
        // Offset 0 must be ignored on an append handle.
        d.write(fh, 0, b"two").unwrap();
        d.close(fh).unwrap();
        assert_eq!(std::fs::read(dir.join("log.txt")).unwrap(), b"onetwo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_len_clamps_the_append_cursor_so_a_later_append_lands_at_the_new_end() {
        let dir = vfs_testkit::scratch_path("vfs-dirsetlen");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log.txt"), b"0123456789").unwrap();
        let d = Director::new();
        d.mount(
            RootId::DEFAULT,
            Arc::new(vfs_compose::DiskProvider::new(&dir)),
        )
        .unwrap();

        let (fh, _, _) = d
            .open(
                RootId::DEFAULT,
                "log.txt",
                OPEN_WRITE | vfs_provider::OPEN_APPEND,
            )
            .unwrap();
        // Cursor starts at 10 (the size at open). Truncating to 4 must clamp
        // it down too, or the next append would write at the stale offset
        // 10, leaving a hole between byte 4 and byte 10 instead of
        // continuing right after the new end.
        d.set_len(fh, 4).unwrap();
        assert_eq!(d.write(fh, 0, b"AB").unwrap(), 2);
        d.close(fh).unwrap();
        assert_eq!(
            std::fs::read(dir.join("log.txt")).unwrap(),
            b"0123AB",
            "append after a truncate must land at the new end with no hole"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_open_does_not_fall_through_a_read_only_top_mount_to_a_writable_one_beneath_it() {
        // A `MountGraph` with a writable `DiskProvider` mounted first (so it
        // resolves *underneath*) and a read-only `InlineProvider` mounted
        // second — making it the topmost resolved mount, per "later mounts
        // override earlier for the same path". A write open must fail with
        // `ST_READ_ONLY` at the top mount rather than silently falling
        // through and landing in the layer beneath — falling through risks a
        // write silently landing in an unintended (possibly immutable)
        // layer.
        let dir = vfs_testkit::scratch_path("vfs-dirshadow");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let graph = vfs_compose::MountGraph::new(vec![
            (
                "/".to_string(),
                Arc::new(vfs_compose::DiskProvider::new(&dir)) as Arc<dyn Provider>,
            ),
            (
                "/".to_string(),
                Arc::new(vfs_compose::InlineProvider::from_files([(
                    "f",
                    b"x".as_slice(),
                )])),
            ),
        ])
        .unwrap();
        let d = Director::new();
        d.mount(RootId::DEFAULT, Arc::new(graph)).unwrap();

        assert_eq!(
            d.open(RootId::DEFAULT, "f", OPEN_WRITE),
            Err(vfs_provider::ST_READ_ONLY)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unmount_drops_visibility() {
        let dir = vfs_testkit::scratch_path("vfs-unmount");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.txt"), b"x").unwrap();
        let d = Director::new();
        d.mount(
            RootId::DEFAULT,
            Arc::new(vfs_compose::DiskProvider::new(&dir)),
        )
        .unwrap();
        assert!(d.getattr(RootId::DEFAULT, "x.txt").unwrap().is_some());
        d.unmount(RootId::DEFAULT).unwrap();
        assert!(d.getattr(RootId::DEFAULT, "x.txt").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_roots_resolve_the_same_relative_path_independently() {
        // The direct-lookup counterpart to the ring-level test in
        // `ring_dispatch.rs`: `[0, "a.txt"]` and `[1, "a.txt"]` must reach
        // different providers.
        let d = Director::new();
        d.mount(
            RootId(0),
            Arc::new(vfs_compose::InlineProvider::from_files([(
                "a.txt",
                b"ZERO".as_slice(),
            )])),
        )
        .unwrap();
        d.mount(
            RootId(1),
            Arc::new(vfs_compose::InlineProvider::from_files([(
                "a.txt",
                b"ONE".as_slice(),
            )])),
        )
        .unwrap();

        let (fh0, size0, _) = d.open(RootId(0), "a.txt", OPEN_READ).unwrap();
        let mut buf0 = [0u8; 8];
        let n0 = d.read(fh0, 0, &mut buf0).unwrap();
        d.close(fh0).unwrap();

        let (fh1, size1, _) = d.open(RootId(1), "a.txt", OPEN_READ).unwrap();
        let mut buf1 = [0u8; 8];
        let n1 = d.read(fh1, 0, &mut buf1).unwrap();
        d.close(fh1).unwrap();

        assert_eq!(size0, 4);
        assert_eq!(&buf0[..n0], b"ZERO");
        assert_eq!(size1, 3);
        assert_eq!(&buf1[..n1], b"ONE");
    }
}
