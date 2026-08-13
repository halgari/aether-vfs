//! The provider contract. Everything past the read core defaults to
//! `ST_NOT_SUPPORTED`, so a read-only provider implements five methods.

use crate::caps::Capabilities;
use crate::model::{DirEntry, Handle, SetAttr, Stat};
use crate::path::VPath;
use crate::status::not_supported;

pub trait Provider: Send + Sync {
    /// Constant for the provider's lifetime; read once at construction.
    fn capabilities(&self) -> Capabilities;

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32>;
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32>;
    /// Returns `(handle, size, is_dir)`.
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32>;
    fn close(&self, h: Handle) -> Result<(), i32>;

    /// Positional read. Short reads are legal anywhere, not only at EOF.
    fn read_at(&self, _h: Handle, _offset: u64, _buf: &mut [u8]) -> Result<usize, i32> {
        Err(not_supported())
    }

    /// Forward-only read for `Access::SeqRead` providers.
    fn read_next(&self, _h: Handle, _buf: &mut [u8]) -> Result<usize, i32> {
        Err(not_supported())
    }

    fn write_at(&self, _h: Handle, _offset: u64, _buf: &[u8]) -> Result<usize, i32> {
        Err(not_supported())
    }
    fn set_len(&self, _h: Handle, _len: u64) -> Result<(), i32> {
        Err(not_supported())
    }
    fn flush(&self, _h: Handle) -> Result<(), i32> {
        Err(not_supported())
    }
    fn mkdir(&self, _p: VPath) -> Result<(), i32> {
        Err(not_supported())
    }
    fn remove(&self, _p: VPath) -> Result<(), i32> {
        Err(not_supported())
    }
    fn rename(&self, _from: VPath, _to: VPath) -> Result<(), i32> {
        Err(not_supported())
    }
    fn set_attr(&self, _p: VPath, _attr: SetAttr) -> Result<(), i32> {
        Err(not_supported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Capabilities;
    use crate::status::ST_NOT_SUPPORTED;

    /// The minimum a read-only provider must implement.
    struct Minimal;

    impl Provider for Minimal {
        fn capabilities(&self) -> Capabilities {
            Capabilities::read_only()
        }
        fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
            Ok(None)
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(Vec::new())
        }
        fn open(&self, _p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
            Err(crate::status::not_found())
        }
        fn close(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }
        fn read_at(&self, _h: Handle, _o: u64, _b: &mut [u8]) -> Result<usize, i32> {
            Ok(0)
        }
    }

    #[test]
    fn unimplemented_methods_report_not_supported() {
        let p = Minimal;
        assert_eq!(p.write_at(0, 0, b"x"), Err(ST_NOT_SUPPORTED));
        assert_eq!(p.mkdir(VPath::at_default("d")), Err(ST_NOT_SUPPORTED));
        assert_eq!(p.read_next(0, &mut [0u8; 4]), Err(ST_NOT_SUPPORTED));
        assert_eq!(p.set_attr(VPath::at_default("f"), SetAttr::default()), Err(ST_NOT_SUPPORTED));
    }

    #[test]
    fn a_minimal_provider_is_object_safe() {
        let p: std::sync::Arc<dyn Provider> = std::sync::Arc::new(Minimal);
        assert_eq!(p.capabilities().access, crate::caps::Access::Read);
    }
}
