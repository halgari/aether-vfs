//! Strip a common archive root folder so zip entries like
//! `Skyrim Special Edition/SkyrimSE.exe` appear as `SkyrimSE.exe`.

use std::sync::Arc;

use vfs_protocol::{Backend, BackendHandle, DirEntry, Stat};

/// Forwards every op to `inner` after prepending `prefix/`.
pub struct StripPrefixBackend {
    inner: Arc<dyn Backend>,
    /// No leading/trailing slashes, e.g. `Skyrim Special Edition`.
    prefix: String,
}

impl StripPrefixBackend {
    pub fn new(inner: Arc<dyn Backend>, prefix: impl Into<String>) -> Self {
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

impl Backend for StripPrefixBackend {
    fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
        self.inner.getattr(&self.map_path(path))
    }

    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        self.inner.readdir(&self.map_path(path))
    }

    fn open(&self, path: &str, flags: u32) -> Result<(BackendHandle, u64, bool), i32> {
        self.inner.open(&self.map_path(path), flags)
    }

    fn read(&self, bh: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.inner.read(bh, offset, buf)
    }

    fn release(&self, bh: BackendHandle) -> Result<(), i32> {
        self.inner.release(bh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InlineBackend;
    use vfs_protocol::OPEN_READ;

    #[test]
    fn strips_archive_root() {
        let inner = Arc::new(InlineBackend::from_files([(
            "Game Root/Data/a.esp",
            b"ESP".as_slice(),
        )]));
        let be = StripPrefixBackend::new(inner, "Game Root");
        let st = be.getattr("Data/a.esp").unwrap().unwrap();
        assert_eq!(st.size, 3);
        let (h, _, _) = be.open("Data/a.esp", OPEN_READ).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(be.read(h, 0, &mut buf).unwrap(), 3);
        be.release(h).unwrap();
    }
}
