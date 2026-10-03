//! Write classes: whether a write is on someone's critical path or part of a bulk job, which
//! decides how its new blocks are compressed (see [`crate::BulkCompression`]).

use std::cell::Cell;

/// Who is waiting for a write.
///
/// Foreground writes are compressed with CPU zstd at [`crate::StoreConfig::zstd_level`], one
/// block per thread, so they never wait for anything else. Bulk writes are compressed as
/// [`crate::StoreConfig::bulk`] says: possibly on the GPU, in batches shared with every other
/// bulk writer, which is faster in total but may make one write wait a few milliseconds for its
/// batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum WriteClass {
    /// A reader or an application is waiting (the default).
    #[default]
    Foreground,
    /// Throughput matters, latency does not: preparing, warming, imports.
    Bulk,
}

thread_local! {
    static CLASS: Cell<WriteClass> = const { Cell::new(WriteClass::Foreground) };
}

impl WriteClass {
    /// The class of writes made on this thread: [`WriteClass::Foreground`] unless inside
    /// [`with_write_class`].
    pub fn current() -> WriteClass {
        CLASS.with(Cell::get)
    }
}

/// Runs `f` with this thread's write class set to `class`; the previous class is restored after,
/// also on a panic. [`crate::BlockStore::write_blocks`] reads it, so a layer or cache that writes
/// on the caller's thread passes the caller's class through without an argument.
pub fn with_write_class<T>(class: WriteClass, f: impl FnOnce() -> T) -> T {
    struct Restore(WriteClass);
    impl Drop for Restore {
        fn drop(&mut self) {
            CLASS.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(CLASS.with(|c| c.replace(class)));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_class_is_per_thread_nested_and_restored() {
        assert_eq!(WriteClass::current(), WriteClass::Foreground);
        with_write_class(WriteClass::Bulk, || {
            assert_eq!(WriteClass::current(), WriteClass::Bulk);
            std::thread::spawn(|| assert_eq!(WriteClass::current(), WriteClass::Foreground))
                .join()
                .unwrap();
            with_write_class(WriteClass::Foreground, || {
                assert_eq!(WriteClass::current(), WriteClass::Foreground);
            });
            assert_eq!(WriteClass::current(), WriteClass::Bulk);
        });
        assert_eq!(WriteClass::current(), WriteClass::Foreground);
        let r = std::panic::catch_unwind(|| {
            with_write_class(WriteClass::Bulk, || panic!("boom"));
        });
        assert!(r.is_err());
        assert_eq!(WriteClass::current(), WriteClass::Foreground);
    }
}
