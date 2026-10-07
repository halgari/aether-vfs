//! Spawn helpers shared by prefix setup and launch: bounded runs and
//! processes in a group of their own.

use std::io;

/// Runs `cmd` to completion or for at most `timeout`, whichever is first; a
/// child still running at the deadline is killed and reaped. `Ok(true)` when
/// it finished by itself (whatever its exit status), `Ok(false)` when it had
/// to be killed.
pub(crate) fn run_bounded(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> io::Result<bool> {
    Ok(run_bounded_status(cmd, timeout)?.is_some())
}

/// [`run_bounded`], keeping the exit status: `Some` when the child finished
/// by itself, `None` when it had to be killed.
pub(crate) fn run_bounded_status(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> io::Result<Option<std::process::ExitStatus>> {
    let mut child = spawn_retrying_busy(cmd)?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Put the child in a process group of its own.
///
/// A terminal delivers Ctrl-C (SIGINT) to its whole foreground process group,
/// and a child spawned plainly joins its parent's. The host decides what
/// Ctrl-C means — typically "stop the launch", through `wineserver -k`, which
/// is orderly — but Proton's prefix setup (a Python script), `wine` and the
/// `wineserver -w` watch would each get the raw signal first and die
/// mid-step: a prefix half-built by an interrupted `proton run` is left
/// behind, and a killed watch ends a launch that is still running.
pub(crate) fn own_process_group(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// `cmd.spawn()` in a process group of its own ([`own_process_group`]),
/// retried briefly on `ETXTBSY`.
///
/// A script written just before it is run can still be open for writing in a
/// child another thread of this process forked in the meantime (the write
/// descriptor is close-on-exec, so it lives only until that child execs), and
/// exec of a file open for writing fails with `ETXTBSY`. The window is
/// microseconds, so a few short retries close it.
pub(crate) fn spawn_retrying_busy(
    cmd: &mut std::process::Command,
) -> io::Result<std::process::Child> {
    own_process_group(cmd);
    let mut attempt = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && attempt < 10 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(10 * attempt));
            }
            r => return r,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn children_get_a_process_group_of_their_own() {
        // Field 5 of /proc/<pid>/stat is the process group; the command name
        // (field 2) is parenthesized and may hold spaces, so split after it.
        fn pgrp(stat: &str) -> i64 {
            let rest = &stat[stat.rfind(')').unwrap() + 2..];
            rest.split(' ').nth(2).unwrap().parse().unwrap()
        }
        let ours = pgrp(&std::fs::read_to_string("/proc/self/stat").unwrap());
        let mut cmd = std::process::Command::new("cat");
        cmd.arg("/proc/self/stat")
            .stdout(std::process::Stdio::piped());
        let child = spawn_retrying_busy(&mut cmd).unwrap();
        let pid = child.id() as i64;
        let out = child.wait_with_output().unwrap();
        let theirs = pgrp(&String::from_utf8(out.stdout).unwrap());
        assert_eq!(theirs, pid, "the child leads its own group");
        assert_ne!(
            theirs, ours,
            "a terminal's Ctrl-C to our group must not reach it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_bounded_kills_a_child_past_its_deadline() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let finished = run_bounded(
            std::process::Command::new("sleep").arg("30"),
            Duration::from_millis(200),
        )
        .unwrap();
        assert!(!finished, "a child past its deadline is reported as killed");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        assert!(run_bounded(
            &mut std::process::Command::new("true"),
            Duration::from_secs(10)
        )
        .unwrap());
        // A failing exit status still counts as finished.
        assert!(run_bounded(
            &mut std::process::Command::new("false"),
            Duration::from_secs(10)
        )
        .unwrap());
    }
}
