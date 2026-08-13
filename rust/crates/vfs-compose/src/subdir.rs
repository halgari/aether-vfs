//! Strip a common archive root folder so zip entries like
//! `Skyrim Special Edition/SkyrimSE.exe` appear as `SkyrimSE.exe`.

use std::sync::Arc;

use vfs_provider::{Capabilities, DirEntry, Handle, Provider, SetAttr, Stat, VPath};

/// Forwards every op to `inner` after prepending `prefix/`. The one
/// combinator that rewrites addressing: every other combinator in this crate
/// forwards `VPath` unchanged, but this one rewrites `rel` while preserving
/// `root`.
pub struct SubdirProvider {
    inner: Arc<dyn Provider>,
    /// No leading/trailing slashes, e.g. `Skyrim Special Edition`.
    prefix: String,
}

impl SubdirProvider {
    pub fn new(inner: Arc<dyn Provider>, prefix: impl Into<String>) -> Self {
        let prefix = prefix
            .into()
            .replace('\\', "/")
            .trim_matches('/')
            .to_string();
        Self { inner, prefix }
    }

    fn map_path(&self, path: &str) -> String {
        let path = path.replace('\\', "/").trim_matches('/').to_string();
        if path.is_empty() {
            self.prefix.clone()
        } else {
            format!("{}/{}", self.prefix, path)
        }
    }
}

impl Provider for SubdirProvider {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let path = p.rel;
        let joined = self.map_path(path);
        self.inner.getattr(VPath {
            root: p.root,
            rel: &joined,
        })
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let path = p.rel;
        let joined = self.map_path(path);
        self.inner.readdir(VPath {
            root: p.root,
            rel: &joined,
        })
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let path = p.rel;
        let joined = self.map_path(path);
        self.inner.open(
            VPath {
                root: p.root,
                rel: &joined,
            },
            flags,
        )
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.inner.read_at(h, offset, buf)
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        self.inner.close(h)
    }

    // Handle-keyed ops need no path rewrite: `open` already resolved and
    // stashed the rewritten path with `inner`'s own handle.
    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        self.inner.write_at(h, offset, buf)
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        self.inner.set_len(h, len)
    }

    fn flush(&self, h: Handle) -> Result<(), i32> {
        self.inner.flush(h)
    }

    // Path-keyed ops must apply `map_path`, same as `getattr`/`readdir`/
    // `open` above — forgetting it here would write to the un-prefixed path
    // in `inner`, silently landing outside the mounted subtree instead of
    // failing.
    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        let path = p.rel;
        let joined = self.map_path(path);
        self.inner.mkdir(VPath {
            root: p.root,
            rel: &joined,
        })
    }

    fn remove(&self, p: VPath) -> Result<(), i32> {
        let path = p.rel;
        let joined = self.map_path(path);
        self.inner.remove(VPath {
            root: p.root,
            rel: &joined,
        })
    }

    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        let from_joined = self.map_path(from.rel);
        let to_joined = self.map_path(to.rel);
        self.inner.rename(
            VPath {
                root: from.root,
                rel: &from_joined,
            },
            VPath {
                root: to.root,
                rel: &to_joined,
            },
        )
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        let path = p.rel;
        let joined = self.map_path(path);
        self.inner.set_attr(
            VPath {
                root: p.root,
                rel: &joined,
            },
            attr,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InlineProvider;
    use vfs_provider::OPEN_READ;

    #[test]
    fn subdir_over_a_mounted_fixture_tree_passes_conformance() {
        // Mount the reference tree under a prefix, then wrap with
        // SubdirProvider so it appears at the root — the composition this
        // combinator exists for (stripping an archive's root folder).
        let mounted: Arc<dyn vfs_provider::Provider> = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES
                .iter()
                .map(|(rel, body)| (format!("mounted/{rel}"), *body)),
        ));
        let p: Arc<dyn vfs_provider::Provider> = Arc::new(SubdirProvider::new(mounted, "mounted"));
        vfs_provider::assert_conformance(p);
    }

    /// The systematic guard for this module's write-forwarding bug (same
    /// class as `RouterProvider`'s): `open()` already forwarded `OPEN_WRITE`
    /// through `map_path` and succeeded, but the rest of the write half
    /// (`write_at`, `mkdir`, `rename`, ...) fell through to
    /// `ST_NOT_SUPPORTED`, and — the sharper risk specific to this
    /// combinator — a fix that forwards without also applying `map_path` to
    /// the path-taking ops would write to the wrong path in `inner` instead
    /// of failing.
    ///
    /// `vfs_provider::RwMemFixture` cannot stand in for `inner` here: it
    /// always serves `FIXTURE_FILES` at its own root, but `SubdirProvider`
    /// addresses `inner` at `mounted/*`. `overlay::tests::MemUpper` is a
    /// blank writable store instead, so the fixture tree is seeded here,
    /// under the prefix, exactly where `map_path` will look for it.
    #[test]
    fn subdir_over_a_writable_inner_passes_conformance() {
        use crate::overlay::tests::MemUpper;
        use vfs_provider::{OPEN_CREATE, OPEN_WRITE};

        let inner: Arc<dyn vfs_provider::Provider> = Arc::new(MemUpper::default());
        inner.mkdir(VPath::at_default("mounted")).unwrap();
        inner.mkdir(VPath::at_default("mounted/sub")).unwrap();
        for (rel, body) in vfs_provider::FIXTURE_FILES {
            let path = format!("mounted/{rel}");
            let (h, _, _) = inner
                .open(VPath::at_default(&path), OPEN_WRITE | OPEN_CREATE)
                .unwrap();
            inner.write_at(h, 0, body).unwrap();
            inner.close(h).unwrap();
        }

        let p: Arc<dyn vfs_provider::Provider> = Arc::new(SubdirProvider::new(inner, "mounted"));
        vfs_provider::assert_conformance(p);
    }

    #[test]
    fn strips_archive_root() {
        let inner = Arc::new(InlineProvider::from_files([(
            "Game Root/Data/a.esp",
            b"ESP".as_slice(),
        )]));
        let be = SubdirProvider::new(inner, "Game Root");
        let st = be
            .getattr(VPath::at_default("Data/a.esp"))
            .unwrap()
            .unwrap();
        assert_eq!(st.size, 3);
        let (h, _, _) = be.open(VPath::at_default("Data/a.esp"), OPEN_READ).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(be.read_at(h, 0, &mut buf).unwrap(), 3);
        be.close(h).unwrap();
    }
}
