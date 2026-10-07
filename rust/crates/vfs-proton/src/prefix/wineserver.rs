//! Controlling a prefix's `wineserver`.

use std::io;
use std::path::Path;

use super::Prefix;
use crate::process::{run_bounded, spawn_retrying_busy};

/// How long each step of [`Prefix::stop_wineserver`] may take.
pub const WINESERVER_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl Prefix {
    /// Stops this prefix's `wineserver` (and any Wine process still in it) and
    /// waits until it has exited, using `runtime`'s own `wineserver`.
    ///
    /// Needed before deleting a prefix: `wineserver` outlives the last Wine
    /// process by a few seconds and **writes the registry back into the prefix
    /// as it exits**, so a prefix removed while it lingers is recreated
    /// (`system.reg`, `user.reg`, `userdef.reg`) moments later. Absent
    /// server: returns promptly.
    ///
    /// **Bounded**: each step gets [`WINESERVER_STOP_TIMEOUT`] and is killed
    /// past it, so a wedged server cannot hang the caller — a `Session`'s
    /// `Drop`, or a daemon draining on shutdown. A step that had to be killed
    /// is reported as `TimedOut`, after both steps have been tried.
    pub fn stop_wineserver(&self, runtime: &Path) -> io::Result<()> {
        let server = runtime.join("files").join("bin").join("wineserver");
        let mut timed_out = None;
        for flag in ["-k", "-w"] {
            // `-k` exits non-zero when no server is running; that is fine.
            let mut cmd = std::process::Command::new(&server);
            cmd.arg(flag)
                .env("WINEPREFIX", &self.dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if !run_bounded(&mut cmd, WINESERVER_STOP_TIMEOUT)? {
                timed_out.get_or_insert(flag);
            }
        }
        match timed_out {
            None => Ok(()),
            Some(flag) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "wineserver {flag} for {} did not finish within {:?} and was killed",
                    self.dir.display(),
                    WINESERVER_STOP_TIMEOUT
                ),
            )),
        }
    }

    /// Starts `wineserver -w` for this prefix, with `runtime`'s own
    /// `wineserver`, and returns without waiting: the child exits once this
    /// prefix's server has — that is, once no Wine process is left in the
    /// prefix (plus the server's few seconds of persistence). With no server
    /// running it exits at once. Unbounded by design: it lasts as long as
    /// whatever runs in the prefix, so a caller polls it (`try_wait`) or
    /// waits on it where blocking for that long is the point.
    pub fn spawn_wineserver_wait(&self, runtime: &Path) -> io::Result<std::process::Child> {
        let mut cmd = std::process::Command::new(runtime.join("files").join("bin").join("wineserver"));
        cmd.arg("-w")
            .env("WINEPREFIX", &self.dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        spawn_retrying_busy(&mut cmd)
    }
}
