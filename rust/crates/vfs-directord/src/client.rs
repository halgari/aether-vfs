//! Client side: connect to a running daemon, or auto-spawn one and wait for it.

use std::path::PathBuf;
use std::time::Duration;

use tonic::transport::Channel;

use vfs_control::pb::director_client::DirectorClient;

use crate::discovery::{default_discovery_path, read_discovery};

/// Connect to an already-running daemon at `endpoint` (`host:port`).
pub async fn connect(endpoint: &str) -> Result<DirectorClient<Channel>, String> {
    let uri = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    };
    DirectorClient::connect(uri)
        .await
        .map_err(|e| format!("connect {endpoint}: {e}"))
}

/// Try discovery file → Health; on failure auto-spawn `vfs daemon` and retry.
///
/// `endpoint_override` skips discovery (and auto-spawn) and connects directly.
pub async fn connect_or_spawn(
    endpoint_override: Option<&str>,
    discovery_path: Option<PathBuf>,
    daemon_exe: PathBuf,
) -> Result<DirectorClient<Channel>, String> {
    if let Some(ep) = endpoint_override {
        return connect(ep).await;
    }

    let path = discovery_path.unwrap_or_else(default_discovery_path);
    if let Ok(d) = read_discovery(&path) {
        if process_alive(d.pid) {
            if let Ok(mut c) = connect(&d.endpoint).await {
                if health_ok(&mut c).await {
                    return Ok(c);
                }
            }
        }
    }

    let mut child = spawn_daemon(&daemon_exe, &path)?;
    wait_for_spawned(&path, &mut child, Duration::from_secs(15)).await
}

/// Where an auto-spawned daemon's stderr goes: `<discovery>.daemon.log`. A
/// file rather than a pipe, because the detached daemon outlives the CLI
/// that spawned it.
pub(crate) fn daemon_log_path(discovery_path: &std::path::Path) -> PathBuf {
    let mut p = discovery_path.as_os_str().to_os_string();
    p.push(".daemon.log");
    PathBuf::from(p)
}

async fn health_ok(client: &mut DirectorClient<Channel>) -> bool {
    client.health(vfs_control::pb::HealthReq {}).await.is_ok()
}

fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        // Prefer OpenProcess over shelling out to tasklist (slow, locale-dependent).
        // SAFETY: OpenProcess is well-defined for any pid; we only check nullity.
        unsafe {
            #[allow(clippy::upper_case_acronyms)] // mirrors the Win32 name
            type HANDLE = *mut core::ffi::c_void;
            extern "system" {
                fn OpenProcess(access: u32, inherit: i32, pid: u32) -> HANDLE;
                fn CloseHandle(h: HANDLE) -> i32;
            }
            const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            CloseHandle(h);
            true
        }
    }
    #[cfg(not(windows))]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}

fn spawn_daemon(
    exe: &PathBuf,
    discovery_path: &std::path::Path,
) -> Result<std::process::Child, String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon");
    cmd.env("VFS_DISCOVERY_PATH", discovery_path);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    // Truncated on each spawn, so it describes this daemon rather than an
    // earlier one — unless the daemon the discovery file names is alive and
    // may still be writing it: then append, after a separator.
    let log = daemon_log_path(discovery_path);
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let live = read_discovery(discovery_path).is_ok_and(|d| process_alive(d.pid));
    match open_spawn_log(&log, live) {
        Ok(f) => cmd.stderr(f),
        Err(_) => cmd.stderr(std::process::Stdio::null()),
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const DETACHED_PROCESS: u32 = 0x00000008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    // Its own process group, so Ctrl-C in the terminal that ran the CLI does
    // not reach the daemon it spawned — the unix counterpart of
    // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS above.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
        .map_err(|e| format!("spawn daemon {}: {e}", exe.display()))
}

/// Opens a spawned daemon's log for its stderr: truncated first (unless a
/// live daemon may still be writing it: then a separator is appended), and
/// always opened for **append**. Two CLIs that spawn at once each truncate
/// it; with append, the first daemon's later writes go to the end of the
/// file rather than to its old offset, which would leave a run of NULs.
fn open_spawn_log(log: &std::path::Path, live: bool) -> std::io::Result<std::fs::File> {
    use std::io::Write;
    if !live {
        std::fs::File::create(log)?; // truncate
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    if live {
        writeln!(f, "--- vfs: spawning another daemon ---")?;
    }
    Ok(f)
}

/// [`wait_for_daemon`] for a daemon this process just spawned: if `child`
/// exits before it is ready (it could not open its storage, say), fail at
/// once with its exit status and its log, instead of waiting out `timeout`.
async fn wait_for_spawned(
    discovery_path: &std::path::Path,
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<DirectorClient<Channel>, String> {
    wait_for_spawned_with(
        discovery_path,
        || {
            child
                .try_wait()
                .map(|st| st.map(|st| st.to_string()))
                .map_err(|e| format!("waiting on the spawned daemon: {e}"))
        },
        timeout,
    )
    .await
}

/// How long a CLI whose spawned daemon exited still looks for another
/// daemon at the same discovery path (a concurrent spawn that won the race).
const SPAWN_RACE_GRACE: Duration = Duration::from_secs(2);

/// [`wait_for_spawned`] with the child's exit check injected (`Ok(Some(status))`
/// once it has exited), so the decision can be tested without a process.
async fn wait_for_spawned_with(
    discovery_path: &std::path::Path,
    mut exited: impl FnMut() -> Result<Option<String>, String>,
    timeout: Duration,
) -> Result<DirectorClient<Channel>, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = exited()? {
            // Two CLIs that auto-spawn at once each start a daemon; the one
            // that loses the storage lock exits while the winner comes up at
            // the same discovery path. Give the winner a short, bounded
            // chance before blaming our own daemon's exit.
            let grace = SPAWN_RACE_GRACE.min(
                deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .max(Duration::from_millis(200)),
            );
            if let Ok(c) = wait_for_daemon(discovery_path, grace).await {
                return Ok(c);
            }
            let log = daemon_log_path(discovery_path);
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            return Err(format!(
                "daemon exited ({status}) before becoming ready: {} [log: {}]",
                text.trim(),
                log.display()
            ));
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(format!(
                "daemon did not become ready (log: {})",
                daemon_log_path(discovery_path).display()
            ));
        }
        // One short readiness attempt per poll, so an exit is seen promptly.
        if let Ok(c) = wait_for_daemon(discovery_path, left.min(Duration::from_millis(200))).await {
            return Ok(c);
        }
    }
}

pub(crate) async fn wait_for_daemon(
    discovery_path: &std::path::Path,
    timeout: Duration,
) -> Result<DirectorClient<Channel>, String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last_err = "daemon did not become ready".to_string();
    while std::time::Instant::now() < deadline {
        if let Ok(d) = read_discovery(discovery_path) {
            match connect(&d.endpoint).await {
                Ok(mut c) => {
                    if health_ok(&mut c).await {
                        return Ok(c);
                    }
                    last_err = format!("health failed at {}", d.endpoint);
                }
                Err(e) => last_err = e,
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve_daemon_until;
    use crate::{DEFAULT_BIND, SessionRegistry};

    /// Two spawns that each truncate the log: the first daemon's later
    /// writes land at the end of the file, never past a hole of NULs.
    #[test]
    fn concurrent_spawn_logs_leave_no_holes() {
        use std::io::Write;
        let d = vfs_testkit::tempdir().unwrap();
        let log = d.path().join("d.json.daemon.log");
        let mut first = open_spawn_log(&log, false).unwrap();
        first.write_all(b"first daemon starting\n").unwrap();
        let mut second = open_spawn_log(&log, false).unwrap();
        second.write_all(b"second\n").unwrap();
        first.write_all(b"first daemon: warning\n").unwrap();
        let text = std::fs::read(&log).unwrap();
        assert!(!text.contains(&0), "{:?}", String::from_utf8_lossy(&text));
        assert_eq!(text, b"second\nfirst daemon: warning\n");
    }

    /// Two CLIs auto-spawn at once: ours loses the storage lock and exits,
    /// while the winner comes up at the same discovery path. The loser must
    /// connect to the winner, not report its own daemon's exit.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_spawned_daemon_that_lost_the_race_yields_to_the_winner() {
        let dir = vfs_testkit::tempdir().unwrap();
        let discovery = dir.path().join("discovery.json");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        // The winner: started a moment after "our" child has already exited.
        let winner = {
            let discovery = discovery.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                serve_daemon_until(
                    DEFAULT_BIND.parse().unwrap(),
                    discovery,
                    SessionRegistry::new(),
                    async {
                        let _ = stopped.await;
                    },
                )
                .await
            })
        };
        let start = std::time::Instant::now();
        let got = wait_for_spawned_with(
            &discovery,
            || Ok(Some("exit status: 1".to_string())),
            Duration::from_secs(15),
        )
        .await;
        assert!(got.is_ok(), "must connect to the winner: {:?}", got.err());
        assert!(start.elapsed() < Duration::from_secs(5));
        stop.send(()).unwrap();
        winner.await.unwrap().unwrap();
    }

    /// Our daemon exited and nobody else came up: the error, with the log,
    /// arrives after the short grace window, not the full timeout.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_spawned_daemon_that_exited_alone_reports_its_log() {
        let dir = vfs_testkit::tempdir().unwrap();
        let discovery = dir.path().join("discovery.json");
        std::fs::write(daemon_log_path(&discovery), "cannot open storage at X").unwrap();
        let start = std::time::Instant::now();
        let e = wait_for_spawned_with(
            &discovery,
            || Ok(Some("exit status: 1".to_string())),
            Duration::from_secs(15),
        )
        .await
        .expect_err("no daemon: an error");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        assert!(
            e.contains("daemon exited (exit status: 1)") && e.contains("cannot open storage"),
            "{e}"
        );
    }
}
