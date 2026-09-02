//! `shim-ring-client`: a **Windows** process with the real `vfs-shim` hooks
//! installed, reading a file that exists only inside a native Linux Director.
//!
//! Pair with `vfs-director`'s `vfs-serve-fb`, which serves the other end of the
//! ring file natively on Linux. This process runs under Proton's Wine.
//!
//! How this differs from `ring-file-client`, which it does not replace:
//! that binary speaks the wire protocol *itself*, so it proves the transport
//! and nothing above it. Here the reader is `std::fs::read` on an ordinary
//! Windows path, and every hop after it — `NtCreateFile`/`NtReadFile`, the
//! hook's under-root classifier, `FuseClient`'s ring client, the synthetic
//! handle table — is the shipped shim's own code. The path it reads exists on
//! no filesystem this process can see.
//!
//! **The shim is installed in-process, not injected**, which is deliberate:
//! injection under Wine is proven separately, and using it here would put two
//! unproven things in one experiment. This binary isolates the ring.
//!
//! Configuration comes from the environment, exactly as a real launch does —
//! `fuse_client::try_init_from_env` reads `VFS_RING_PATH`, `VFS_RING_BYTES`,
//! `VFS_RING_PAYLOAD_CAP`, `VFS_ARENA_LEN` and `VFS_VIRTUAL_DIR`, and this
//! binary reads none of them itself except to echo them. `VFS_RING_BYTES` is
//! the one that must match the server's: too small and the ring still *opens*
//! (the header is at offset 0) while the bulk arena falls outside the mapping,
//! so it is echoed here to be checked against the server's own log.
//!
//! Two reads, deliberately: a 16-byte one that travels inline in the ring
//! payload, and the whole file (past the 64 KiB bulk threshold) whose bytes
//! travel through the shared arena instead. The arena is the read path large
//! assets take, and it is the only one an under-sized `VFS_RING_BYTES` breaks.
//!
//! Exit 0 with `CLIENT: OK` means a Wine-hosted shim read bytes out of a
//! native Linux Director. Any other exit prints why on stderr.

#[cfg(windows)]
mod imp {
    use std::io::{Read as _, Write as _};
    use std::process::exit;

    use vfs_shim::{fuse_client, install, Engine};

    /// Bytes of the first, deliberately small read. Under
    /// `FuseClient`'s 64 KiB bulk threshold, so it travels inline in the ring
    /// slot's own payload.
    const INLINE_PROBE: usize = 16;

    fn fail(msg: &str) -> ! {
        eprintln!("CLIENT FAIL: {msg}");
        let _ = std::io::stderr().flush();
        exit(1);
    }

    fn say(msg: &str) {
        println!("CLIENT: {msg}");
        let _ = std::io::stdout().flush();
    }

    /// A valid, **empty** snapshot: one root directory with no children.
    ///
    /// The engine's local snapshot is what a standalone shim composes from, and
    /// it must contribute nothing here — every byte has to come across the
    /// ring. An empty snapshot is the strongest available statement of that: if
    /// the read somehow resolved locally, there is nothing for it to resolve
    /// *to*. `Engine::new` validates the snapshot, so it cannot be a bare
    /// `Vec::new()`.
    fn empty_snapshot() -> Vec<u8> {
        let mut b = vfs_shared::SnapshotBuilder::new();
        let root = b.add_dir("", &[]);
        b.set_root(root);
        b.finish()
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        if args.len() != 4 {
            fail("usage: shim-ring-client <file-path> <pattern> <total-len>");
        }
        let path = args[1].clone();
        let pattern = args[2].as_bytes().to_vec();
        let total: usize = args[3]
            .parse()
            .unwrap_or_else(|_| fail("total-len must be a positive integer"));
        if pattern.len() < INLINE_PROBE {
            fail("pattern must be at least as long as the inline probe");
        }

        // Echoed, not chosen: the geometry has to be the server's, and this is
        // the record that it was.
        for name in [
            vfs_env::RING_PATH,
            vfs_env::RING_BYTES,
            vfs_env::RING_PAYLOAD_CAP,
            vfs_env::ARENA_LEN,
            vfs_env::VIRTUAL_DIR,
        ] {
            say(&format!(
                "env {name}={}",
                vfs_env::text(name).unwrap_or_else(|| "<unset>".to_string())
            ));
        }

        // The managed root comes from the same variable the client reads, so
        // the engine and the ring client cannot disagree about it — a
        // disagreement would be invisible, classifying the path as outside any
        // root and letting it fall through to real disk.
        let root = vfs_env::text(vfs_env::VIRTUAL_DIR)
            .unwrap_or_else(|| fail("VFS_VIRTUAL_DIR unset: the managed root has no default"));

        // Before the hooks exist, so this failure is reported rather than
        // hidden behind a redirected open.
        if let Err(e) = fuse_client::try_init_from_env() {
            fail(&format!("fuse init: {e:?}"));
        }
        say("fuse client attached (heartbeat answered)");

        let engine = Engine::new(&root, empty_snapshot())
            .unwrap_or_else(|e| fail(&format!("engine: {e:?}")));
        let _guard = install(engine).unwrap_or_else(|e| fail(&format!("install: {e:?}")));
        say(&format!("hooks installed, root={root}"));

        // A by-name attribute query first (`NtQueryFullAttributesFile` →
        // GETATTR). An ordinary std call; nothing here knows a ring exists.
        let md = std::fs::metadata(&path)
            .unwrap_or_else(|e| fail(&format!("metadata {path}: {e}")));
        say(&format!("metadata {path} len={}", md.len()));
        if md.len() != total as u64 {
            fail(&format!("metadata len {} != expected {total}", md.len()));
        }

        // A small read first, and separately from the big one, because the two
        // take **different transports** inside `FuseClient::read_fragmented`:
        // under 64 KiB the bytes ride inline in the ring slot's payload, at or
        // above it they ride in the shared bulk arena and the ring carries only
        // (len, arena offset). An inline-only proof would leave the arena — and
        // with it the mapping-size hazard below — untested.
        let mut f =
            std::fs::File::open(&path).unwrap_or_else(|e| fail(&format!("open {path}: {e}")));
        let mut head = [0u8; INLINE_PROBE];
        f.read_exact(&mut head)
            .unwrap_or_else(|e| fail(&format!("inline read {path}: {e}")));
        if head[..] != pattern[..INLINE_PROBE] {
            fail(&format!(
                "inline read mismatch: got {:?}, want {:?}",
                String::from_utf8_lossy(&head),
                String::from_utf8_lossy(&pattern[..INLINE_PROBE])
            ));
        }
        drop(f);
        say(&format!(
            "inline read {INLINE_PROBE} bytes: {:?}",
            String::from_utf8_lossy(&head)
        ));

        // The whole file: past the bulk threshold, so the bytes cross through
        // the arena. **This is what makes a wrong `VFS_RING_BYTES` fatal**
        // rather than invisible — an under-sized mapping still opens the ring
        // (the header is at offset 0) and only fails once an arena bank lands
        // outside the view, which an inline read never touches.
        let got = std::fs::read(&path).unwrap_or_else(|e| fail(&format!("read {path}: {e}")));
        if got.len() != total {
            fail(&format!("read {} bytes, want {total}", got.len()));
        }
        // Position-sensitive on purpose: a copy that is truncated, short, or
        // offset by an arena bank mismatches instead of coincidentally
        // agreeing.
        if let Some(i) = (0..got.len()).find(|&i| got[i] != pattern[i % pattern.len()]) {
            fail(&format!(
                "content mismatch at byte {i} of {total}: got {}, want {}",
                got[i],
                pattern[i % pattern.len()]
            ));
        }
        say(&format!(
            "bulk read {} bytes through the shim, pattern matches; head={:?}",
            got.len(),
            String::from_utf8_lossy(&got[..INLINE_PROBE])
        ));
        say("OK");
        exit(0);
    }
}

#[cfg(windows)]
fn main() {
    imp::main();
}

// `vfs-shim` is Windows-only (NT detours), so the body above cannot compile on
// Linux; this keeps a Linux build of this crate green, the same shape
// `ring-file-client` uses.
#[cfg(not(windows))]
fn main() {}
