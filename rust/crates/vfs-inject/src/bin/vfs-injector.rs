//! Generic injector (formalized from the M3 spike): a JVM-drivable wrapper
//! over `run_target_with_shim`. The JVM sets the ring env (VFS_RING_SECTION
//! etc.), spawns this bin; this bin injects the shim (dual-layer) into the
//! target, which inherits the env and connects its FuseClient back to the JVM
//! ring.
//!
//! Usage:
//!   vfs-injector <target_exe> <shim_dll> <payload_dll> <config_file> <ready_file> [-- target_args...]
use std::time::Duration;
use vfs_inject::{parse_injector_args, run_target_with_shim, RunConfig};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (target, dll, payload, config, ready, args) = match parse_injector_args(&a) {
        Ok(parsed) => parsed,
        Err(usage) => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    };

    eprintln!("[vfs-injector] target={target} shim={dll} payload={payload}");
    let exit = run_target_with_shim(RunConfig {
        target_exe: target,
        args,
        current_dir: None,
        dll_path: dll,
        config_path: config,
        ready_path: ready,
        ready_timeout: Duration::from_secs(20),
        payload_path: payload,
        preinit_redirects: vec![],
        detach: false,
    })
    .unwrap_or_else(|e| {
        eprintln!("[vfs-injector] inject error: {e:?}");
        std::process::exit(3);
    });
    eprintln!("[vfs-injector] target exited {exit}");
    std::process::exit(exit);
}
