//! Questions about processes that std cannot answer: who this process runs
//! as, and whether another one is alive and what it is called.
//!
//! Each was answered elsewhere by reading `/proc`, which Linux has and macOS
//! does not; asking the kernel directly works on both.

/// The effective uid this process runs as.
pub fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes nothing, cannot fail, and touches no memory of
    // ours.
    #[allow(unsafe_code)]
    unsafe {
        libc::geteuid()
    }
}

/// Whether a process `pid` exists — running, stopped or a zombie not yet
/// reaped, as `/proc/<pid>` would say. A process of another user's counts:
/// it exists, this process is only not allowed to signal it.
pub fn process_alive(pid: u32) -> bool {
    // 0 is "my process group" and anything above `i32::MAX` is a negative
    // pid, "that process group": neither names one process.
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 is the documented existence check: it delivers
    // nothing and touches no memory of ours.
    #[allow(unsafe_code)]
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The short command name of process `pid` — what `/proc/<pid>/comm` holds on
/// Linux, and `proc_name` on macOS — or `None` when there is no such process.
#[cfg(target_os = "linux")]
pub fn process_name(pid: u32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim_end_matches('\n').to_owned())
}

/// The short command name of process `pid` — what `/proc/<pid>/comm` holds on
/// Linux, and `proc_name` on macOS — or `None` when there is no such process.
#[cfg(target_os = "macos")]
pub fn process_name(pid: u32) -> Option<String> {
    let pid = libc::c_int::try_from(pid).ok().filter(|&p| p > 0)?;
    // `proc_name` fills at most 2 * MAXCOMLEN (32) bytes and NUL-terminates.
    let mut buf = [0u8; 64];
    // SAFETY: `buf` is writable for the length passed, and outlives the call.
    #[allow(unsafe_code)]
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let n = usize::try_from(n).ok().filter(|&n| n > 0)?;
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// A signal [`signal`] sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// `SIGTERM`: asks the process to end.
    Term,
    /// `SIGKILL`: ends it.
    Kill,
}

/// Sends `signal` to process `pid`. `false` when there is no such process or
/// it may not be signalled.
pub fn signal(pid: u32, signal: Signal) -> bool {
    let signal = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` touches no memory of ours; `pid` is one process (> 0).
    #[allow(unsafe_code)]
    let r = unsafe { libc::kill(pid, signal) };
    r == 0
}

/// Every process id this process can see.
#[cfg(target_os = "linux")]
pub fn all_pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Every process id this process can see.
#[cfg(target_os = "macos")]
pub fn all_pids() -> Vec<u32> {
    // Asked twice: once for the count, then with room to spare for
    // processes started in between.
    // SAFETY: a null buffer of size 0 asks only for the count.
    #[allow(unsafe_code)]
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(n) = usize::try_from(n) else {
        return Vec::new();
    };
    let mut pids = vec![0 as libc::pid_t; n + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: `pids` is writable for `bytes` bytes and outlives the call.
    #[allow(unsafe_code)]
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    let got = usize::try_from(got).unwrap_or(0).min(pids.len());
    pids[..got]
        .iter()
        .filter_map(|&p| u32::try_from(p).ok().filter(|&p| p > 0))
        .collect()
}

/// Process `pid`'s first argument as it now reads — which a program may
/// have rewritten: Wine puts the Windows image path there
/// (`C:\Games\X\game.exe`). `None` when it cannot be read.
#[cfg(target_os = "linux")]
pub fn process_argv0(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let first = raw.split(|&b| b == 0).next()?;
    Some(String::from_utf8_lossy(first).trim().to_owned()).filter(|s| !s.is_empty())
}

/// Process `pid`'s first argument as it now reads — which a program may
/// have rewritten: Wine puts the Windows image path there
/// (`C:\Games\X\game.exe`). `None` when it cannot be read.
#[cfg(target_os = "macos")]
pub fn process_argv0(pid: u32) -> Option<String> {
    let pid = libc::c_int::try_from(pid).ok().filter(|&p| p > 0)?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut buf = vec![0u8; 64 * 1024];
    let mut len = buf.len();
    // SAFETY: `mib` names three levels, `buf` is writable for `len` bytes,
    // and `len` is updated to what was written.
    #[allow(unsafe_code)]
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return None;
    }
    // `int argc`, the executable's path, NUL padding, then argv[0]...
    let rest = buf.get(std::mem::size_of::<libc::c_int>()..len)?;
    let after_exec = rest.iter().position(|&b| b == 0)?;
    let rest = &rest[after_exec..];
    let start = rest.iter().position(|&b| b != 0)?;
    let rest = &rest[start..];
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    Some(String::from_utf8_lossy(&rest[..end]).trim().to_owned()).filter(|s| !s.is_empty())
}

/// Process `pid`'s current directory. `None` when it cannot be read.
#[cfg(target_os = "linux")]
pub fn process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Process `pid`'s current directory. `None` when it cannot be read.
#[cfg(target_os = "macos")]
pub fn process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let pid = libc::c_int::try_from(pid).ok().filter(|&p| p > 0)?;
    // SAFETY: an all-zero `proc_vnodepathinfo` is a valid value of it (plain
    // integers and byte arrays).
    #[allow(unsafe_code)]
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: `info` is writable for `size` bytes and outlives the call.
    #[allow(unsafe_code)]
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    let path = &info.pvi_cdir.vip_path;
    // SAFETY: `c_char` and `u8` have the same size and alignment, and the
    // slice stays within `path`.
    #[allow(unsafe_code)]
    let bytes: &[u8] = unsafe {
        // `size_of_val`, not `len`: libc spells the `MAXPATHLEN` array as
        // `[[c_char; 32]; 32]`, whose `len` is 32.
        std::slice::from_raw_parts(path.as_ptr().cast(), std::mem::size_of_val(path))
    };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Some(std::ffi::OsStr::from_bytes(&bytes[..end]).into())
        .filter(|p: &std::path::PathBuf| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive_and_a_reaped_child_is_not() {
        assert!(process_alive(std::process::id()));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!process_alive(pid));
        assert!(!process_alive(0), "0 is a process group, not a process");
        assert!(
            !process_alive(u32::MAX),
            "a negative pid is a process group"
        );
    }

    #[test]
    fn the_effective_uid_owns_what_this_process_creates() {
        use std::os::unix::fs::MetadataExt;
        let dir = vfs_testkit::scratch_dir("vfs-unix-euid");
        assert_eq!(std::fs::metadata(&*dir).unwrap().uid(), effective_uid());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_process_is_named_after_its_executable() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let name = process_name(child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(name.as_deref(), Some("sleep"));
        assert_eq!(process_name(child.id()), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_process_is_listed_with_its_first_argument_and_directory() {
        let dir = vfs_testkit::scratch_dir("vfs-unix-procs");
        let dir = std::fs::canonicalize(&*dir).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&dir)
            .spawn()
            .unwrap();
        let pid = child.id();
        // `sleep` may not have exec'd yet the first time it is looked at.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while process_argv0(pid).as_deref() != Some("sleep") {
            assert!(
                std::time::Instant::now() < deadline,
                "{:?}",
                process_argv0(pid)
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(all_pids().contains(&pid));
        assert!(all_pids().contains(&std::process::id()));
        assert_eq!(process_cwd(pid), Some(dir));
        assert!(signal(pid, Signal::Term));
        child.wait().unwrap();
        assert!(!signal(pid, Signal::Term), "it is gone");
        assert_eq!(process_argv0(pid), None);
    }
}
