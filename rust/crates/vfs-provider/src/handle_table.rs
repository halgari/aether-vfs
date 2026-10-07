//! The table of open handles that a provider keeps.
//!
//! Most providers need the same three things: a counter that hands out
//! handles, a map from handle to per-open state, and lookups that answer
//! `ST_BAD_FH` for a handle that is not open and `ST_IO_ERROR` for a
//! poisoned lock. [`HandleTable`] is that, so a provider holds one field and
//! none of the locking idiom.
//!
//! Handles start at 1 and are never reused within a table.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Handle;
use crate::status::{ST_BAD_FH, lock_or_status};

/// Open-handle state of type `T`, keyed by the handle the provider issued.
///
/// A poisoned lock is `ST_IO_ERROR` from every method.
pub struct HandleTable<T> {
    next: AtomicU64,
    map: Mutex<HashMap<Handle, T>>,
}

impl<T> Default for HandleTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> HandleTable<T> {
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            map: Mutex::new(HashMap::new()),
        }
    }

    /// A handle that is not in the table, for an open that keeps no state
    /// (a directory, say). [`remove`](Self::remove) on it is `ST_BAD_FH`.
    pub fn fresh(&self) -> Handle {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Store `value` under a new handle.
    pub fn insert(&self, value: T) -> Result<Handle, i32> {
        let h = self.fresh();
        lock_or_status(&self.map)?.insert(h, value);
        Ok(h)
    }

    /// Run `f` on the state for `h`, holding the table's lock for its
    /// duration. Keep `f` short, and do not call back into the table.
    pub fn with<R>(&self, h: Handle, f: impl FnOnce(&mut T) -> R) -> Result<R, i32> {
        let mut g = lock_or_status(&self.map)?;
        g.get_mut(&h).map(f).ok_or(ST_BAD_FH)
    }

    /// Take the state for `h` out of the table.
    pub fn remove(&self, h: Handle) -> Result<T, i32> {
        lock_or_status(&self.map)?.remove(&h).ok_or(ST_BAD_FH)
    }

    /// Whether `h` is open. A poisoned table says no.
    pub fn contains(&self, h: Handle) -> bool {
        lock_or_status(&self.map).is_ok_and(|g| g.contains_key(&h))
    }

    /// Take every open entry out of the table.
    pub fn drain(&self) -> Result<Vec<(Handle, T)>, i32> {
        Ok(lock_or_status(&self.map)?.drain().collect())
    }
}

impl<T: Clone> HandleTable<T> {
    /// A copy of the state for `h`.
    pub fn get(&self, h: Handle) -> Result<T, i32> {
        self.with(h, |v| v.clone())
    }

    /// A copy of every open entry's state.
    pub fn values(&self) -> Result<Vec<T>, i32> {
        Ok(lock_or_status(&self.map)?.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::ST_IO_ERROR;

    #[test]
    fn insert_get_remove() {
        let t = HandleTable::new();
        let a = t.insert("a".to_string()).unwrap();
        let b = t.insert("b".to_string()).unwrap();
        assert_ne!(a, b);
        assert_eq!(t.get(a).unwrap(), "a");
        t.with(b, |s| s.push('!')).unwrap();
        assert_eq!(t.remove(b).unwrap(), "b!");
        assert_eq!(t.get(b), Err(ST_BAD_FH));
        assert_eq!(t.remove(b), Err(ST_BAD_FH));
        assert!(t.contains(a) && !t.contains(b));
        assert_eq!(t.values().unwrap(), vec!["a".to_string()]);
    }

    #[test]
    fn fresh_handles_are_not_stored_and_not_reused() {
        let t: HandleTable<u8> = HandleTable::new();
        let f = t.fresh();
        assert_eq!(t.remove(f), Err(ST_BAD_FH));
        assert_ne!(t.insert(1).unwrap(), f);
    }

    #[test]
    fn drain_empties_the_table() {
        let t = HandleTable::new();
        let a = t.insert(1u8).unwrap();
        assert_eq!(t.drain().unwrap(), vec![(a, 1)]);
        assert_eq!(t.get(a), Err(ST_BAD_FH));
    }

    #[test]
    fn poison_is_an_io_error() {
        let t = HandleTable::new();
        let a = t.insert(1u8).unwrap();
        let _ = std::panic::catch_unwind(|| {
            let _ = t.with(a, |_| panic!("poison"));
        });
        assert_eq!(t.get(a), Err(ST_IO_ERROR));
        assert_eq!(t.insert(2), Err(ST_IO_ERROR));
        assert!(!t.contains(a));
    }
}
