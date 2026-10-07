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

    /// The spelling this provider stores for the last component of `p` — the
    /// name a listing of its parent shows — or `None` if it has no such
    /// entry. `p` may be in any case; the answer is the stored one.
    ///
    /// What a final-path query needs: one name, without listing the
    /// directory it is in. A provider that can answer from an index should;
    /// the default says it cannot, and callers then find the name in a
    /// listing instead (`vfs_compose::stored_name`), which costs the whole
    /// directory every time.
    fn stored_name(&self, _p: VPath) -> Result<Option<String>, i32> {
        Err(not_supported())
    }

    /// Whether the bytes behind the open handle `h` can never change while
    /// it is open — the per-handle form of [`Capabilities::immutable`].
    ///
    /// A provider's own `immutable` is the answer for every handle it opens,
    /// which is the default. A composition answers for the child that holds
    /// the handle: an overlay's handle on a base file of an immutable base
    /// is immutable even though the overlay as a whole (it can be written)
    /// is not. That is what lets a client cache what it read through such a
    /// handle — the director reports it in its open reply, and the shim's
    /// read cache serves only handles it is true for. Answering `true` for a
    /// handle whose content can change is a stale-read bug in that cache;
    /// `false` only costs it a cache miss.
    ///
    /// [`Capabilities::immutable`]: crate::caps::Capabilities::immutable
    fn is_immutable(&self, _h: Handle) -> bool {
        self.capabilities().immutable
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
        assert_eq!(
            p.set_attr(VPath::at_default("f"), SetAttr::default()),
            Err(ST_NOT_SUPPORTED)
        );
    }

    /// The default answer for a handle is the provider's own declaration.
    #[test]
    fn a_handle_is_as_immutable_as_its_provider_by_default() {
        assert!(!Minimal.is_immutable(1), "read_only() is mutable");
        struct Frozen;
        impl Provider for Frozen {
            fn capabilities(&self) -> Capabilities {
                Capabilities {
                    immutable: true,
                    ..Capabilities::read_only()
                }
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
        }
        assert!(Frozen.is_immutable(1));
    }

    #[test]
    fn a_minimal_provider_is_object_safe() {
        let p: std::sync::Arc<dyn Provider> = std::sync::Arc::new(Minimal);
        assert_eq!(p.capabilities().access, crate::caps::Access::Read);
    }
}
