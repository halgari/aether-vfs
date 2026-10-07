//! Per-file state: the file's name and version, its slots and in-flight fetches.

use std::collections::HashSet;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::lock;
use super::stats::FileDiag;

/// Which file: a root and its folded path under it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Name {
    pub(super) root: u32,
    pub(super) path: String,
}

/// Which content of that file: equal for every immutable open of it within
/// one mount generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Version {
    pub(super) size: u64,
    pub(super) mount_gen: u32,
}

/// What one handle holds of the cache: its file, and — if the handle may be
/// served from it — the version it saw. Cheap to clone.
#[derive(Clone)]
pub struct FileRef {
    pub(super) entry: Arc<Entry>,
    pub(super) version: Option<Version>,
}

impl FileRef {
    /// Whether reads through this handle may be served from the cache.
    #[cfg(test)]
    pub(crate) fn cacheable(&self) -> bool {
        self.version.is_some()
    }

    /// The file size this handle was opened with, if it is cacheable.
    pub fn size(&self) -> Option<u64> {
        self.version.map(|v| v.size)
    }
}

impl std::fmt::Debug for FileRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileRef")
            .field("root", &self.entry.name.root)
            .field("path", &self.entry.name.path)
            .field("version", &self.version)
            .finish()
    }
}

pub(super) struct Entry {
    pub(super) name: Name,
    pub(super) state: Mutex<State>,
}

#[derive(Default)]
pub(super) struct State {
    /// Changed, or might have: never cached again in this process.
    pub(super) poisoned: bool,
    /// The version the blocks below hold.
    pub(super) version: Option<Version>,
    pub(super) slots: Vec<Slot>,
    /// Since the file was last judged: reads that hit, and locality misses.
    pub(super) hits: u32,
    pub(super) misses: u32,
    /// Reads left to serve uncached while cold, and the next cold spell.
    pub(super) cold_left: u32,
    pub(super) cold_next: u32,
    /// Block fetches of this file that failed since it last went cold.
    pub(super) failed_fetches: u32,
    /// Where the last fetch ended, and how many units the next one takes if
    /// it starts exactly there.
    pub(super) next_seq: Option<u64>,
    pub(super) run: usize,
    /// Units the process-wide cap evicted from this file: a miss on one is
    /// a capacity miss, not evidence of poor locality.
    pub(super) pressure_evicted: HashSet<u64>,
    /// Units this file has fetched before (a bit each): a miss on one of
    /// them is a re-fetch, which says something about locality; a first
    /// fetch does not.
    pub(super) fetched_before: Vec<u64>,
    pub(super) diag: FileDiag,
}

impl State {
    /// Mark `idx` fetched; whether it had been before.
    pub(super) fn refetch(&mut self, idx: u64) -> bool {
        let (w, b) = ((idx / 64) as usize, idx % 64);
        if self.fetched_before.len() <= w {
            self.fetched_before.resize(w + 1, 0);
        }
        let was = self.fetched_before[w] & (1 << b) != 0;
        self.fetched_before[w] |= 1 << b;
        was
    }
}

pub(super) struct Slot {
    pub(super) idx: u64,
    pub(super) kind: SlotKind,
}

pub(super) enum SlotKind {
    Ready {
        data: Arc<[u8]>,
        /// Tick of the last read that used it.
        last: u64,
        /// Its key in the LRU index (the tick it was filed under).
        lru_key: u64,
    },
    Loading(Arc<Flight>),
}

/// What a finished fetch gives its waiters: the first unit's index and the
/// run's bytes, or `None` if it failed.
pub(super) type Fetched = Option<(u64, Arc<[u8]>)>;

/// One fetch of a run of units, which other readers of any of them can
/// wait for.
pub(super) struct Flight {
    pub(super) done: Mutex<Option<Fetched>>,
    pub(super) cv: Condvar,
    pub(super) started: Instant,
}

impl Flight {
    pub(super) fn new() -> Self {
        Flight {
            done: Mutex::new(None),
            cv: Condvar::new(),
            started: Instant::now(),
        }
    }

    /// Whether the fetch has outlived `deadline` without finishing.
    pub(super) fn overdue(&self, deadline: Duration) -> bool {
        self.started.elapsed() >= deadline
    }

    /// The first completion wins; later ones are ignored.
    pub(super) fn complete(&self, v: Fetched) {
        let mut g = lock(&self.done);
        if g.is_none() {
            *g = Some(v);
            self.cv.notify_all();
        }
    }

    /// Wait until the fetch finishes or `deadline` after it started.
    /// `Err(())` if it had not finished by then.
    pub(super) fn wait(&self, deadline: Duration) -> Result<Fetched, ()> {
        let left = deadline.saturating_sub(self.started.elapsed());
        let g = lock(&self.done);
        let (g, _) = self
            .cv
            .wait_timeout_while(g, left, |d| d.is_none())
            .unwrap_or_else(|e| e.into_inner());
        g.clone().ok_or(())
    }
}
