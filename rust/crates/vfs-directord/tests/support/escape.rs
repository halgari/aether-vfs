//! Running `vfs-fixture-escape` under a session and reading what it reports;
//! shared by the escape matrix and the enumeration test.

use std::path::Path;

use super::launch::{drain_launch_events, LAUNCH_LOCK};

/// One parsed line of `vfs-fixture-escape`'s TSV output — see that crate's
/// module doc for the exact format this mirrors.
#[derive(Debug, Clone)]
pub struct EscapeLine {
    pub vector: String,
    pub spelling: String,
    pub outcome: String,
    pub note: String,
}

pub fn parse_escape_lines(text: &str) -> Vec<EscapeLine> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let mut parts = l.splitn(4, '\t');
            Some(EscapeLine {
                vector: parts.next()?.to_string(),
                spelling: parts.next()?.to_string(),
                outcome: parts.next()?.to_string(),
                note: parts.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// Launch `vfs-fixture-escape.exe` against `target` under `client`'s
/// `session_id`, with hook-stats logging enabled, and return its own parsed
/// TSV lines plus the shim's classified-paths set (see
/// `super::classified_paths`) built from the same run.
/// The parts of an escape-matrix fixture launch that stay constant across
/// every call in one test run — bundled so `run_escape_fixture` itself
/// stays under clippy's argument-count lint rather than growing a ninth
/// positional parameter for every future per-vector wrinkle.
#[derive(Clone, Copy)]
pub struct EscapeFixtureCtx<'a> {
    pub session_id: &'a str,
    pub fixture: &'a Path,
    pub stats_log: &'a Path,
    /// See `VFS_ESCAPE_VECTOR7_LINK_DIR`'s doc comment in `vfs-env`: a
    /// junction created by this test harness itself, before any fixture
    /// process is launched, so vector 7 never has to construct one from
    /// inside an already-injected process.
    pub vector7_link_dir: Option<&'a str>,
    /// `true` sets `VFS_ESCAPE_ACCESS=write`, so every vector writes through
    /// its spelling instead of reading through it. Unset (the default) the
    /// fixture runs the read matrix exactly as it always has.
    pub write_access: bool,
}

pub async fn run_escape_fixture(
    client: &mut vfs_control::pb::director_client::DirectorClient<tonic::transport::Channel>,
    ctx: &EscapeFixtureCtx<'_>,
    target: &Path,
    out_file: &Path,
    only_vector: Option<&str>,
) -> (
    i32,
    Vec<EscapeLine>,
    std::collections::BTreeSet<String>,
    bool,
) {
    use vfs_control::pb::LaunchReq;
    let EscapeFixtureCtx {
        session_id,
        fixture,
        stats_log,
        vector7_link_dir,
        write_access,
    } = *ctx;

    let _ = std::fs::remove_file(stats_log);
    let _ = std::fs::remove_file(out_file);

    let mut env = std::collections::HashMap::new();
    env.insert(
        "VFS_SHIM_STATS_LOG".to_string(),
        stats_log.to_string_lossy().into_owned(),
    );
    if let Some(dir) = vector7_link_dir {
        env.insert("VFS_ESCAPE_VECTOR7_LINK_DIR".to_string(), dir.to_string());
    }
    if write_access {
        env.insert("VFS_ESCAPE_ACCESS".to_string(), "write".to_string());
    }
    // Fast tick: this whole run (twenty-two lines plus a couple of helper
    // process spawns) finishes in well under the reporter's 250ms default,
    // so a short override is what makes the classification snapshot land at
    // all — same reasoning as the write-path e2e tests' identical override.
    //
    // The vectors-7/9 closeout found that this alone is not quite enough
    // margin for an *isolated* single-vector run specifically: with
    // `VFS_ESCAPE_ONLY_VECTOR` set, the selected vector's own decision is
    // the *only* real file activity in the whole process, so the process's
    // total lifetime can be short enough that `vfs-fixture-escape`'s own
    // end-of-run wait (`interval_ms * 2` = 10ms here) lands under Windows'
    // default ~15.6ms system timer resolution — a `Sleep(10)` on Windows is
    // not reliably "wakes at 10ms", only "wakes no earlier than 10ms, next
    // tick or later" — occasionally letting the process exit before the
    // reporter's first tick ever fires, an intermittent classification miss
    // unrelated to canonicalisation itself. Fixed at the source
    // (`vfs-fixture-escape::main`'s end-of-run wait is now floored at 20ms,
    // comfortably clearing that granularity) rather than by tuning this
    // interval further, since shrinking it below Windows' own timer
    // resolution floor would not have helped either.
    env.insert("VFS_SHIM_STATS_INTERVAL_MS".to_string(), "5".to_string());
    if let Some(v) = only_vector {
        env.insert("VFS_ESCAPE_ONLY_VECTOR".to_string(), v.to_string());
    }

    let mut stream = client
        .launch(LaunchReq {
            session_id: session_id.to_string(),
            exec: fixture.to_string_lossy().into_owned(),
            args: vec![
                target.to_string_lossy().into_owned(),
                out_file.to_string_lossy().into_owned(),
            ],
            wait: true,
            env,
        })
        .await
        .expect("Launch")
        .into_inner();

    let mut exit_code = None;
    drain_launch_events(&mut stream, "escape fixture log", &mut exit_code).await;

    let text = std::fs::read_to_string(out_file).unwrap_or_default();
    let lines = parse_escape_lines(&text);
    let (classified, truncated) = super::classified_paths(stats_log);
    (exit_code.unwrap_or(-1), lines, classified, truncated)
}

/// Every name directly inside `dir` on the **real** filesystem. Called only
/// from this test harness process, which is never injected, so `read_dir`
/// here answers about physical disk rather than about the provider graph.
pub fn real_dir_names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"))
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}
