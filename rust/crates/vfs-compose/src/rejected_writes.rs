//! Process-wide record of writes refused because no provider on the path is
//! writable, keyed by path.
//!
//! This is the discovery instrument spec §7 describes: a host that mounts a
//! vanilla install read-only reads it back to learn which paths the game tried
//! to write. `MountGraph` records here when it refuses a write, and
//! `vfs-director`'s `io_stats` records here for the refusals `Director::open`
//! makes, so one list covers both. It lives in this crate, not in
//! `vfs-director`, so that `MountGraph` does not depend on the kernel.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

static REJECTED: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

fn table() -> &'static Mutex<HashMap<String, u64>> {
    REJECTED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The path as the report keys it: no leading separator, `/` between
/// components, and `/` for the root.
fn norm_path(p: &str) -> String {
    let p = p.trim().trim_start_matches('/').trim_start_matches('\\');
    if p.is_empty() || p == "." {
        return "/".into();
    }
    p.replace('\\', "/")
}

/// Record that `open(..., OPEN_WRITE)` was refused because the resolved
/// mount's provider has no `ReadWrite` access. Keyed by path so a caller can
/// tell "no provider here is writable" apart from a one-off mistake.
pub fn record_rejected_write(path: &str) {
    let path = norm_path(path);
    if let Ok(mut t) = table().lock() {
        *t.entry(path).or_insert(0) += 1;
    }
}

/// Snapshot of `(path, count)` for every rejected write seen so far.
pub fn rejected_writes() -> Vec<(String, u64)> {
    let Ok(t) = table().lock() else {
        return Vec::new();
    };
    t.iter().map(|(path, count)| (path.clone(), *count)).collect()
}

/// Clear rejected-write tracking (tests; also useful before a fresh probe).
pub fn reset_rejected_writes() {
    if let Ok(mut t) = table().lock() {
        t.clear();
    }
}
