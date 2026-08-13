//! Strip a common archive root folder so zip entries like
//! `Skyrim Special Edition/SkyrimSE.exe` appear as `SkyrimSE.exe`.

use std::sync::Arc;

use vfs_provider::{Capabilities, DirEntry, Handle, Provider, Stat, VPath};

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InlineProvider;
    use vfs_provider::OPEN_READ;

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
