//! Spike injector (M3 de-risk): a JVM-drivable wrapper over
//! `run_target_with_shim`. The JVM sets the ring env (VFS_RING_SECTION etc.),
//! spawns this bin; this bin injects the shim (dual-layer) into the target,
//! which inherits the env and connects its FuseClient back to the JVM ring.
//!
//! Usage:
//!   vfs-spike-inject <target_exe> <shim_dll> <payload_dll> <config_file> <ready_file> [-- target_args...]
use std::time::Duration;
use vfs_inject::{run_target_with_shim, RunConfig};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 6 {
        eprintln!(
            "usage: vfs-spike-inject <target> <shim_dll> <payload_dll> <config> <ready> [-- args...]"
        );
        std::process::exit(2);
    }
    let target = a[1].clone();
    let dll = a[2].clone();
    let payload = a[3].clone();
    let config = a[4].clone();
    let ready = a[5].clone();
    let args: Vec<String> = if a.len() > 6 && a[6] == "--" {
        a[7..].to_vec()
    } else {
        a[6..].to_vec()
    };

    eprintln!("[spike-inject] target={target} shim={dll} payload={payload}");
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
        target_pe_bytes: None,
    })
    .unwrap_or_else(|e| {
        eprintln!("[spike-inject] inject error: {e:?}");
        std::process::exit(3);
    });
    eprintln!("[spike-inject] target exited {exit}");
    std::process::exit(exit);
}
