//! Top-wins layering of two providers (Clojure `layered-provider`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_core::fold;
use vfs_provider::{
    bad_fh, map_io_err, not_found, read_only, Access, Capabilities, DirEntry, Handle, Provider,
    SetAttr, Stat, VPath, OPEN_WRITE,
};

#[derive(Clone, Copy)]
enum Layer {
    Top,
    Bottom,
}

/// `top` shadows `bottom` on the same path; readdir unions with top-wins names.
pub struct LayeredProvider {
    top: Arc<dyn Provider>,
    bottom: Arc<dyn Provider>,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, (Layer, Handle)>>,
}

impl LayeredProvider {
    pub fn new(top: Arc<dyn Provider>, bottom: Arc<dyn Provider>) -> Self {
        Self {
            top,
            bottom,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        }
    }

    fn routed(&self, layer: &Layer) -> &Arc<dyn Provider> {
        match layer {
            Layer::Top => &self.top,
            Layer::Bottom => &self.bottom,
        }
    }

    /// The child a write op is routed to: the topmost declared `ReadWrite`,
    /// or `None` if neither child is writable.
    fn write_target(&self) -> Option<Layer> {
        if self.top.capabilities().access == Access::ReadWrite {
            Some(Layer::Top)
        } else if self.bottom.capabilities().access == Access::ReadWrite {
            Some(Layer::Bottom)
        } else {
            None
        }
    }

    /// Shared handle lookup for the ops below that address an existing open
    /// handle rather than a path.
    fn lookup(&self, h: Handle) -> Result<(Layer, Handle), i32> {
        let g = self.opens.lock().map_err(|_| map_io_err())?;
        let (l, i) = g.get(&h).ok_or_else(bad_fh)?;
        Ok((*l, *i))
    }
}

impl Provider for LayeredProvider {
    fn capabilities(&self) -> Capabilities {
        let top = self.top.capabilities();
        let bottom = self.bottom.capabilities();
        // `immutable`/`slow`/`preferred_block` combine conservatively via
        // `weakest` — those are genuinely "worst of both". Access is the one
        // exception: this stack can serve a write whenever *either* child
        // can, because every write op below routes to whichever child
        // actually declares `ReadWrite` (`write_target`), not to both. Using
        // `weakest` for access too would report the whole stack read-only
        // the moment just one child is — the exact bug this fix closes
        // (LayeredProvider advertised `ReadWrite` via `weakest` already, but
        // `open` then hard-rejected every write regardless). So: strongest
        // access, weakest everything else.
        let combined = Capabilities::weakest([top, bottom]);
        Capabilities {
            access: top.access.max(bottom.access),
            ..combined
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        match self.top.getattr(p)? {
            Some(s) => Ok(Some(s)),
            None => self.bottom.getattr(p),
        }
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let top_entries = match self.top.readdir(p) {
            Ok(e) => e,
            Err(e) if e == not_found() => {
                return self.bottom.readdir(p);
            }
            Err(e) => return Err(e),
        };
        let bottom_entries = self.bottom.readdir(p).unwrap_or_default();
        // Keyed by `vfs_core::fold` — the same fold the shim applies to vpath
        // components before they cross the ring. An ASCII-only key lets two
        // spellings of one Unicode name survive as two entries, so a
        // top-layer override of a non-ASCII-cased file stops overriding.
        let mut seen: HashMap<String, DirEntry> = HashMap::new();
        // Bottom first, top overwrites.
        for e in bottom_entries {
            seen.insert(fold(&e.name), e);
        }
        for e in top_entries {
            seen.insert(fold(&e.name), e);
        }
        let mut out: Vec<DirEntry> = seen.into_values().collect();
        out.sort_by_key(|a| fold(&a.name));
        Ok(out)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let (layer, inner) = if flags & OPEN_WRITE != 0 {
            // Every write op targets the same single child (whichever
            // declares `ReadWrite`), so a write open must too — there is no
            // top/bottom fallback here the way reads have one, because
            // falling back to a non-target child would open a handle that
            // later write_at/set_len/etc. calls could not honour.
            let layer = self.write_target().ok_or_else(read_only)?;
            (layer, self.routed(&layer).open(p, flags)?)
        } else {
            match self.top.open(p, flags) {
                Ok(r) => (Layer::Top, r),
                Err(e) if e == not_found() => {
                    let r = self.bottom.open(p, flags)?;
                    (Layer::Bottom, r)
                }
                Err(e) => return Err(e),
            }
        };
        let (bh, size, is_dir) = inner;
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .insert(h, (layer, bh));
        Ok((h, size, is_dir))
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (layer, inner) = self.lookup(h)?;
        self.routed(&layer).read_at(inner, offset, buf)
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let (layer, inner) = {
            let mut g = self.opens.lock().map_err(|_| map_io_err())?;
            g.remove(&h).ok_or_else(bad_fh)?
        };
        self.routed(&layer).close(inner)
    }

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let (layer, inner) = self.lookup(h)?;
        self.routed(&layer).write_at(inner, offset, buf)
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        let (layer, inner) = self.lookup(h)?;
        self.routed(&layer).set_len(inner, len)
    }

    fn flush(&self, h: Handle) -> Result<(), i32> {
        let (layer, inner) = self.lookup(h)?;
        self.routed(&layer).flush(inner)
    }

    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        let layer = self.write_target().ok_or_else(read_only)?;
        self.routed(&layer).mkdir(p)
    }

    fn remove(&self, p: VPath) -> Result<(), i32> {
        let layer = self.write_target().ok_or_else(read_only)?;
        self.routed(&layer).remove(p)
    }

    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        let layer = self.write_target().ok_or_else(read_only)?;
        self.routed(&layer).rename(from, to)
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        let layer = self.write_target().ok_or_else(read_only)?;
        self.routed(&layer).set_attr(p, attr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InlineProvider;
    use vfs_provider::{KIND_FILE, OPEN_READ};

    #[test]
    fn top_wins_shared_path() {
        let bottom = Arc::new(InlineProvider::from_files([
            ("shared.txt", b"FROM-BASE".as_slice()),
            ("meshes/base.nif", b"BASE".as_slice()),
        ]));
        let top = Arc::new(InlineProvider::from_files([
            ("shared.txt", b"MOD-WIN".as_slice()),
            ("textures/mod.dds", &[1u8; 10]),
        ]));
        let layered = LayeredProvider::new(top, bottom);

        let st = layered
            .getattr(VPath::at_default("shared.txt"))
            .unwrap()
            .unwrap();
        assert_eq!(st.size, 7);
        assert_eq!(st.kind, KIND_FILE);

        let st = layered
            .getattr(VPath::at_default("meshes/base.nif"))
            .unwrap()
            .unwrap();
        assert_eq!(st.size, 4);

        let st = layered
            .getattr(VPath::at_default("textures/mod.dds"))
            .unwrap()
            .unwrap();
        assert_eq!(st.size, 10);

        let (h, size, _) = layered
            .open(VPath::at_default("shared.txt"), OPEN_READ)
            .unwrap();
        assert_eq!(size, 7);
        let mut buf = [0u8; 16];
        let n = layered.read_at(h, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"MOD-WIN");
        layered.close(h).unwrap();
    }

    #[test]
    fn readdir_unions_with_top_winning_names() {
        let bottom = Arc::new(InlineProvider::from_files([
            ("a.txt", b"A".as_slice()),
            ("shared.txt", b"BASE".as_slice()),
        ]));
        let top = Arc::new(InlineProvider::from_files([
            ("b.txt", b"B".as_slice()),
            ("shared.txt", b"TOP".as_slice()),
        ]));
        let layered = LayeredProvider::new(top, bottom);
        let entries = layered.readdir(VPath::at_default("")).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case("a.txt")));
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case("b.txt")));
        let shared = entries
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case("shared.txt"))
            .unwrap();
        assert_eq!(shared.stat.size, 3); // "TOP"
    }

    #[test]
    fn open_missing_on_top_falls_through() {
        let bottom = Arc::new(InlineProvider::from_files([(
            "only-base.txt",
            b"BB".as_slice(),
        )]));
        let top = Arc::new(InlineProvider::from_files([(
            "only-top.txt",
            b"TT".as_slice(),
        )]));
        let layered = LayeredProvider::new(top, bottom);
        let (h, size, _) = layered
            .open(VPath::at_default("only-base.txt"), OPEN_READ)
            .unwrap();
        assert_eq!(size, 2);
        let mut buf = [0u8; 4];
        assert_eq!(layered.read_at(h, 0, &mut buf).unwrap(), 2);
        layered.close(h).unwrap();
    }

    // --- Fix 3: the systematic guard. `assert_conformance` over a wrapper
    // with a *writable* inner is what exercises `assert_writable`'s cases;
    // every pre-existing LayeredProvider test above uses read-only
    // `InlineProvider` children, so the write half of the trait never ran —
    // which is exactly how `open()`'s hard `bad_request()` on `OPEN_WRITE`
    // survived review undetected. These two reproduce the real production
    // shape (`SessionRegistry::add_source` stacks N `CachingProvider`-wrapped
    // `DiskProvider`s, every one `Access::ReadWrite`) in both possible
    // top/bottom arrangements.

    /// Both children writable — the actual "two-or-more root-mounted
    /// sources" production shape. Failed before the fix: `open()` refused
    /// every `OPEN_WRITE` unconditionally with `ST_BAD_REQUEST`, so
    /// `assert_writable`'s very first `open(f, OPEN_WRITE | OPEN_CREATE)`
    /// panicked.
    #[test]
    fn a_layered_stack_with_both_children_writable_passes_conformance() {
        use vfs_provider::RwMemFixture;
        let top: Arc<dyn Provider> = Arc::new(RwMemFixture::new());
        let bottom: Arc<dyn Provider> = Arc::new(RwMemFixture::new());
        let layered: Arc<dyn Provider> = Arc::new(LayeredProvider::new(top, bottom));
        vfs_provider::assert_conformance(layered);
    }

    /// Only the bottom child is writable — proves `write_target` correctly
    /// falls back past a read-only top instead of only ever considering top.
    #[test]
    fn a_layered_stack_with_only_bottom_writable_passes_conformance() {
        use vfs_provider::RwMemFixture;
        let top: Arc<dyn Provider> = Arc::new(InlineProvider::from_files(
            std::iter::empty::<(&str, &[u8])>(),
        ));
        let bottom: Arc<dyn Provider> = Arc::new(RwMemFixture::new());
        let layered: Arc<dyn Provider> = Arc::new(LayeredProvider::new(top, bottom));
        vfs_provider::assert_conformance(layered);
    }

    #[test]
    fn capabilities_report_the_strongest_child_access_not_the_weakest() {
        use vfs_provider::{Access, RwMemFixture};

        // Writable top, read-only bottom.
        let top: Arc<dyn Provider> = Arc::new(RwMemFixture::new());
        let bottom: Arc<dyn Provider> = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let layered = LayeredProvider::new(top, bottom);
        assert_eq!(
            layered.capabilities().access,
            Access::ReadWrite,
            "a writable top must make the stack writable"
        );

        // Read-only top, writable bottom — the asymmetric case `weakest`
        // would get wrong (it would report Read, the top's access).
        let top: Arc<dyn Provider> = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let bottom: Arc<dyn Provider> = Arc::new(RwMemFixture::new());
        let layered = LayeredProvider::new(top, bottom);
        assert_eq!(
            layered.capabilities().access,
            Access::ReadWrite,
            "a writable bottom must make the stack writable even though top is read-only"
        );
    }
}
