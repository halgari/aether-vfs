//! Plumbing shared by the integration test binaries.
//!
//! Every hook test installs process-wide state: the detours, the director's ring, env
//! switches such as `VFS_REGISTRY`, and `OnceLock` decisions such as `regclient::enabled`. Two
//! tests that need different state cannot share a process, so each `#[test]` in these binaries
//! starts with [`isolate!`], which re-executes the test binary on that one test in a fresh
//! process. The re-executed copy sees [`CHILD_ENV`] and runs the body.

use std::process::Command;
use std::sync::Mutex;

/// Set in the re-executed child so that it runs the test body instead of re-executing again.
const CHILD_ENV: &str = "AETHER_VFS_TEST_ISOLATED";

/// One isolated test at a time per binary: libtest runs tests on several threads, and the
/// registry tests (for one) create and delete the same real keys.
static SERIAL: Mutex<()> = Mutex::new(());

/// Put this first in a `#[test]`: in the parent it runs the test in a child process and
/// returns from the test, in the child it falls through to the body.
macro_rules! isolate {
    () => {{
        fn f() {}
        if !$crate::common::run_isolated(::std::any::type_name_of_val(&f)) {
            return;
        }
    }};
}

/// Whether the caller is the isolated child and should run its body. In the parent this runs
/// the test in a child and panics if the child fails, so returns `false` only on success.
/// `fn_path` is the type name of a function item nested in the test.
pub(crate) fn run_isolated(fn_path: &str) -> bool {
    if std::env::var_os(CHILD_ENV).is_some() {
        return true;
    }
    // `crate::module::test::f` -> libtest's `module::test`.
    let path = fn_path.strip_suffix("::f").expect("a nested fn named f");
    let name = path.split_once("::").map_or(path, |(_, rest)| rest);
    let _one_at_a_time = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let out = Command::new(std::env::current_exe().expect("current exe"))
        .args(["--exact", name, "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .output()
        .expect("spawn the isolated test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{name} failed in its own process ({}):\n{stdout}\n{stderr}",
        out.status
    );
    // A filter that matches nothing also exits 0.
    assert!(
        stdout.contains("1 passed"),
        "{name} did not run in its own process:\n{stdout}\n{stderr}"
    );
    false
}
