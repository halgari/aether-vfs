//! `BoundedTally`: a process-wide, thread-safe count of how often each key was seen, whose
//! number of distinct keys is capped.
//!
//! Past the cap new keys are dropped but known keys keep counting, so a loop that started early
//! still shows its true rate while a session that runs for hours cannot grow the table without
//! limit. A poisoned lock and a never-used tally read alike: as empty.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;

pub(super) struct BoundedTally<K> {
    map: Mutex<Option<HashMap<K, u64>>>,
    max: usize,
}

impl<K> BoundedTally<K> {
    /// A tally that learns at most `max` distinct keys.
    pub(super) const fn new(max: usize) -> Self {
        Self {
            map: Mutex::new(None),
            max,
        }
    }

    /// A tally with no cap, for keys drawn from a small fixed set.
    pub(super) const fn unbounded() -> Self {
        Self::new(usize::MAX)
    }
}

impl<K: Eq + Hash> BoundedTally<K> {
    /// Count one more sighting of `key`.
    pub(super) fn add(&self, key: K) {
        let Ok(mut g) = self.map.lock() else { return };
        let map = g.get_or_insert_with(HashMap::new);
        if let Some(c) = map.get_mut(&key) {
            *c += 1;
        } else if map.len() < self.max {
            map.insert(key, 1);
        }
    }

    /// How many times `key` was seen; 0 for one never seen (or dropped past the cap).
    pub(super) fn count<Q>(&self, key: &Q) -> u64
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.map
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|m| m.get(key).copied()))
            .unwrap_or(0)
    }
}

impl<K: Clone> BoundedTally<K> {
    /// A copy of every key and its count.
    pub(super) fn snapshot(&self) -> HashMap<K, u64> {
        self.map
            .lock()
            .ok()
            .and_then(|g| g.as_ref().cloned())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_tally_is_empty() {
        let t = BoundedTally::<String>::new(2);
        assert!(t.snapshot().is_empty());
        assert_eq!(t.count("x"), 0);
    }

    #[test]
    fn repeats_are_counted() {
        let t = BoundedTally::new(4);
        t.add("a".to_string());
        t.add("a".to_string());
        t.add("b".to_string());
        assert_eq!(t.count("a"), 2);
        assert_eq!(t.count("b"), 1);
        assert_eq!(t.snapshot().len(), 2);
    }

    /// Past the cap new keys are dropped, but a known key still counts: an early loop keeps its
    /// true rate.
    #[test]
    fn past_the_cap_new_keys_are_dropped_and_known_keys_keep_counting() {
        let t = BoundedTally::new(2);
        t.add(1u32);
        t.add(2);
        t.add(3);
        t.add(1);
        t.add(1);
        assert_eq!(t.count(&3), 0);
        assert_eq!(t.count(&1), 3);
        assert_eq!(t.count(&2), 1);
        assert_eq!(t.snapshot().len(), 2);
    }

    #[test]
    fn an_unbounded_tally_learns_every_key() {
        let t = BoundedTally::unbounded();
        for i in 0..10_000u32 {
            t.add(i);
        }
        assert_eq!(t.snapshot().len(), 10_000);
    }

    #[test]
    fn a_borrowed_key_finds_an_owned_one() {
        let t = BoundedTally::<&'static str>::unbounded();
        t.add("NtCreateFile");
        assert_eq!(t.count("NtCreateFile"), 1);
    }
}
