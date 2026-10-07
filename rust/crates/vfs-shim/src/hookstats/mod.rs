//! Per-hook call counts and time, for attributing shim overhead.
//!
//! `io_stats` in the director counts only what reaches it over the ring. The
//! shim detours *every* file operation the process makes, and the ones that
//! pass straight through to disk never become a request — so the director is
//! blind to them. Measured 2026-08-12: a launch reaching a window served ~800
//! director ops but took ~9.3 s longer than the same game with no VFS, which
//! those 800 ops cannot explain. This counts the population the director
//! cannot see.
//!
//! Off unless `VFS_SHIM_STATS_LOG` names a file: reading a clock on every
//! intercepted call is itself measurable, so it must not be on by default.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use self::tally::BoundedTally;

mod io;
mod open;
mod panics;
mod registry;
mod report;
mod tally;
#[cfg(test)]
mod tests;

pub use io::delete_on_close_refused_count;
pub use io::link_refused_count;
pub(crate) use io::*;
pub use open::*;
pub use panics::*;
pub use registry::*;
pub(crate) use report::*;

/// Generates [`Hook`], its count and its names from `detour_table!`: one `Variant = id` per row
/// that has a `stat` column.
///
/// The ids are written in the table, not counted, because they are the breadcrumb's hook ids
/// (a reader outside the process decodes them) and so must not move when a row is inserted or
/// reordered. The `const` block below fails the build if the ids are not exactly `0..N` with
/// no gaps or repeats.
macro_rules! hook_stats_from_table {
    ($(
        {
            export: $export:literal,
            stat: [$($variant:ident = $id:expr)?],
            $($rest:tt)*
        }
    )*) => {
        /// Hooks attributed separately: one entry per instrumented hook (there is no catch-all variant).
        #[derive(Copy, Clone, Debug, PartialEq, Eq)]
        #[repr(usize)]
        pub(crate) enum Hook {
            $($($variant = $id,)?)*
        }

        /// Number of [`Hook`]s.
        const N: usize = [$($(stringify!($variant),)?)*].len();

        /// The export name of each [`Hook`], indexed by its id.
        const NAMES: [&str; N] = {
            let mut names = [""; N];
            $($(names[$id] = $export;)?)*
            let mut i = 0;
            while i < N {
                assert!(!names[i].is_empty(), "hook ids in detour_table! are not exactly 0..N");
                i += 1;
            }
            names
        };
    };
}

detour_table!(hook_stats_from_table);

static CALLS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static NANOS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
/// Calls that resolved to VFS content (rather than passing through to disk).
/// Only meaningful for hooks that call `mark_rooted` — currently create/open.
static ROOTED: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
/// Slowest single call, and how many exceeded [`SLOW_NS`]. A mean cannot tell
/// "every call is slow" from "a few calls stall on a cold wake", and those want
/// opposite fixes — the first is per-call work, the second is wake latency.
static MAX_NANOS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static SLOW: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
/// Time above which a call is counted as stalled rather than served: well past
/// any ring RPC (20–209 µs measured) and into scheduler-quantum territory.
const SLOW_NS: u64 = 1_000_000;
static REPORTER: AtomicBool = AtomicBool::new(false);

/// Whether instrumentation is on, resolved once.
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| vfs_env::present(vfs_env::SHIM_STATS_LOG))
}

/// Times one hook invocation and records it on drop.
///
/// Constructing this when disabled reads no clock and touches no atomics, so an
/// un-instrumented run pays only a cached bool check.
pub(crate) struct Timed {
    hook: Hook,
    start: Option<Instant>,
    rooted: bool,
}

impl Timed {
    pub(crate) fn new(hook: Hook) -> Self {
        // Independent of `enabled()`: the breadcrumb is for a hang, where the
        // stats path is useless (its clock reads and reporter thread have been
        // measured to suppress the race). Two relaxed stores, or nothing at all
        // when the breadcrumb is off.
        crate::breadcrumb::enter(hook as u32);
        Timed {
            hook,
            start: if enabled() {
                Some(Instant::now())
            } else {
                None
            },
            rooted: false,
        }
    }

    /// Mark this call as having been served from the VFS.
    pub(crate) fn mark_rooted(&mut self) {
        self.rooted = true;
    }
}

impl Drop for Timed {
    fn drop(&mut self) {
        crate::breadcrumb::exit(self.hook as u32);
        let Some(start) = self.start else { return };
        let i = self.hook as usize;
        let ns = start.elapsed().as_nanos() as u64;
        CALLS[i].fetch_add(1, Ordering::Relaxed);
        NANOS[i].fetch_add(ns, Ordering::Relaxed);
        if ns > SLOW_NS {
            SLOW[i].fetch_add(1, Ordering::Relaxed);
        }
        MAX_NANOS[i].fetch_max(ns, Ordering::Relaxed);
        if self.rooted {
            ROOTED[i].fetch_add(1, Ordering::Relaxed);
        }
    }
}
