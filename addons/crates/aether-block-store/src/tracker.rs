//! Read generation tracking, so retired packs are deleted only after every read that
//! might still reference them has finished.

use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct ReadTracker {
    state: Mutex<TrackerState>,
}

#[derive(Default)]
struct TrackerState {
    generation: u64,
    /// generation -> number of reads in progress that entered during it
    active: BTreeMap<u64, usize>,
}

pub struct ReadGuard<'a> {
    tracker: &'a ReadTracker,
    generation: u64,
}

impl ReadTracker {
    /// Call before opening the index read transaction.
    pub fn enter(&self) -> ReadGuard<'_> {
        let mut s = self.state.lock().unwrap();
        let generation = s.generation;
        *s.active.entry(generation).or_insert(0) += 1;
        ReadGuard {
            tracker: self,
            generation,
        }
    }

    /// Starts a new generation and returns it. Call after the commit that retires a pack:
    /// reads entering from now on cannot see the retired pack.
    pub fn advance(&self) -> u64 {
        let mut s = self.state.lock().unwrap();
        s.generation += 1;
        s.generation
    }

    /// True once no read that entered before generation `generation` is still running.
    pub fn is_clear_before(&self, generation: u64) -> bool {
        let s = self.state.lock().unwrap();
        s.active
            .keys()
            .next()
            .is_none_or(|&oldest| oldest >= generation)
    }
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        let mut s = self.tracker.state.lock().unwrap();
        let n = s.active.get_mut(&self.generation).unwrap();
        *n -= 1;
        if *n == 0 {
            s.active.remove(&self.generation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_older_reads_only() {
        let t = ReadTracker::default();
        let old = t.enter();
        let g = t.advance();
        assert!(!t.is_clear_before(g));
        let new = t.enter();
        drop(old);
        assert!(t.is_clear_before(g));
        let g2 = t.advance();
        assert!(!t.is_clear_before(g2));
        drop(new);
        assert!(t.is_clear_before(g2));
    }
}
