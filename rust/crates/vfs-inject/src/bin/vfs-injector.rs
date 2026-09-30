//! Generic injector (formalized from the M3 spike): a JVM-drivable wrapper
//! over `run_target_with_shim`. The JVM sets the ring env (VFS_RING_SECTION
//! etc.), spawns this bin; this bin injects the shim (dual-layer) into the
//! target, which inherits the env and connects its FuseClient back to the JVM
//! ring.
//!
//! Usage:
//!   vfs-injector <target_exe> <shim_dll> <payload_dll> <config_file> <ready_file> [-- target_args...]
//!
//! Environment: `VFS_READY_TIMEOUT_SECS` (default 180) bounds the wait for the
//! shim's ready report; `VFS_INJECT_CWD` is the target's working directory
//! (default: this process's). On an injection failure it exits 3 after
//! writing one line — `target-exited:<code>`, `ready-timeout:<secs>` or
//! `inject:<error>` — to `<ready_file>.injector-error`
//! (`vfs_env::INJECTOR_ERROR_SUFFIX`), so a caller that only sees the exit
//! code can still say why.
use std::time::Duration;
use vfs_inject::{parse_injector_args, run_target_with_shim, InjectError, RunConfig};

/// The ready wait when `VFS_READY_TIMEOUT_SECS` is unset: what a Windows
/// `Session::launch` has always defaulted to. A cold first launch in a fresh
/// Wine prefix can take well over the 20 s this used to be.
const DEFAULT_READY_TIMEOUT_SECS: u64 = 180;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let parsed = match parse_injector_args(&a) {
        Ok(parsed) => parsed,
        Err(usage) => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    };
    let vfs_inject::InjectorArgs {
        target,
        shim_dll: dll,
        payload_dll: payload,
        config,
        ready,
        target_args: args,
    } = parsed;

    let ready_timeout = Duration::from_secs(
        vfs_env::parsed_or(vfs_env::READY_TIMEOUT_SECS, DEFAULT_READY_TIMEOUT_SECS).max(1),
    );
    let current_dir = vfs_env::text(vfs_env::INJECT_CWD).filter(|d| !d.is_empty());
    let report = format!("{ready}{}", vfs_env::INJECTOR_ERROR_SUFFIX);
    let _ = std::fs::remove_file(&report);

    eprintln!("[vfs-injector] target={target} shim={dll} payload={payload} cwd={current_dir:?}");
    let exit = run_target_with_shim(RunConfig {
        target_exe: target,
        args,
        current_dir,
        dll_path: dll,
        config_path: config,
        ready_path: ready,
        ready_timeout,
        payload_path: payload,
        preinit_redirects: vec![],
        detach: false,
    })
    .unwrap_or_else(|e| {
        eprintln!("[vfs-injector] inject error: {e:?}");
        let line = match &e {
            InjectError::TargetExited(code) => {
                format!("{}{code:#x}", vfs_env::INJECTOR_TARGET_EXITED_PREFIX)
            }
            InjectError::Timeout => format!(
                "{}{}",
                vfs_env::INJECTOR_READY_TIMEOUT_PREFIX,
                ready_timeout.as_secs()
            ),
            other => format!("{}{other:?}", vfs_env::INJECTOR_FAILED_PREFIX),
        };
        let _ = std::fs::write(&report, line);
        std::process::exit(3);
    });
    eprintln!("[vfs-injector] target exited {exit}");
    std::process::exit(exit);
}
