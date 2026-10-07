//! Cache sizing: the defaults and [`CacheConfig`].

use std::time::Duration;

/// Bytes per cached unit.
pub const DEFAULT_BLOCK: usize = 64 * 1024;
/// Most units one miss fetches (a sequential reader's run): 1 MiB.
pub const DEFAULT_MAX_RUN: usize = 16;
/// Reads shorter than this are served from the cache; others bypass it.
pub const DEFAULT_THRESHOLD: usize = 64 * 1024;
/// Units one file may hold at once: 16 MiB.
pub const DEFAULT_BLOCKS_PER_FILE: usize = 256;
/// Bytes the whole cache may hold, in-flight fetches included. The shim
/// takes it from `VFS_SHIM_READ_CACHE_MIB`.
pub const DEFAULT_MAX_BYTES: usize = 256 << 20;

/// A file goes cold after this many locality misses of evaluation…
pub(super) const COLD_AFTER_MISSES: u32 = 16;
/// …if it averaged fewer hits than [`CacheConfig::cold_hits_per_miss`] per
/// miss. Only re-fetches count (see [`CacheConfig::cold_counts_first_fetch`])
/// and not misses the process-wide cap caused (see
/// `State::pressure_evicted`), so this fires only on a file read at random
/// across more units than it may hold, where each re-fetched 64 KiB unit
/// (~80 µs with the provider) costs more than the ~5 uncached small reads
/// (~14 µs each) it would have to save to pay for itself. Replaying a real
/// launch's reads, it never fires.
pub const DEFAULT_COLD_HITS_PER_MISS: u32 = 8;
/// Reads a cold file is served uncached before it is tried again, doubling
/// each time it goes cold again, up to [`COLD_MAX_READS`].
pub(super) const COLD_READS: u32 = 4096;
pub(super) const COLD_MAX_READS: u32 = 1 << 16;
/// A file whose block fetches fail (or come back short) this many times
/// goes cold too, rather than paying a failed block fetch before every
/// uncached read.
pub(super) const COLD_AFTER_FAILED_FETCHES: u32 = 4;
/// Units of one file remembered as evicted by the process-wide cap.
pub(super) const PRESSURE_MEMORY: usize = 4096;
/// Files whose diagnostics are kept after nothing holds them any more.
pub(super) const RETIRED_DIAGS: usize = 4096;

/// How the cache is cut up. [`Default`] is what the shim uses.
#[derive(Debug, Clone, Copy)]
pub struct CacheConfig {
    /// Bytes per unit: what is stored, evicted and (at least) fetched.
    pub block: usize,
    /// Most units one fetch brings in, when a file is being read
    /// sequentially. `1` fetches exactly the missing unit every time.
    pub max_run: usize,
    pub threshold: usize,
    /// Most units one file holds at once.
    pub blocks_per_file: usize,
    pub max_bytes: usize,
    /// A file whose locality misses average fewer hits than this goes cold
    /// (see [`COLD_AFTER_MISSES`]); `0` never sends a file cold for that.
    pub cold_hits_per_miss: u32,
    /// Whether a file's first fetch of a unit counts against it. Off by
    /// default: a file being read into the cache for the first time misses
    /// on every unit it touches however good its locality, and judging it
    /// on those misses sent a 4 KiB reader of an 8 MiB region cold before
    /// it had warmed. Only a unit fetched **again** — after the file's own
    /// LRU dropped it — then counts.
    pub cold_counts_first_fetch: bool,
    /// How long a fetch is waited for, counted from when it started — the
    /// fetch's own deadline. A reader that finds another thread's fetch of
    /// its block waits at most what is left of this; a fetch older than it
    /// (its thread was killed, or its provider stopped answering) is given
    /// up on: the reader removes it and fetches the block itself.
    pub wait: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            block: DEFAULT_BLOCK,
            max_run: DEFAULT_MAX_RUN,
            threshold: DEFAULT_THRESHOLD,
            blocks_per_file: DEFAULT_BLOCKS_PER_FILE,
            max_bytes: DEFAULT_MAX_BYTES,
            cold_hits_per_miss: DEFAULT_COLD_HITS_PER_MISS,
            cold_counts_first_fetch: false,
            wait: crate::RESPONSE_DEADLINE,
        }
    }
}
