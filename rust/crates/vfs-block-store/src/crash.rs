//! Crash injection for tests. With the `crash-points` feature, `point(name)` aborts the process
//! when the `BLOCK_STORE_CRASH_AT` environment variable equals `name`. Without it, `point` is a no-op.

#[cfg(feature = "crash-points")]
pub fn point(name: &str) {
    if std::env::var("BLOCK_STORE_CRASH_AT").is_ok_and(|v| v == name) {
        std::process::abort();
    }
}

#[cfg(not(feature = "crash-points"))]
#[inline(always)]
pub fn point(_name: &str) {}
