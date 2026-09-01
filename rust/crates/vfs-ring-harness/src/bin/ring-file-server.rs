//! `ring-file-server`: the **native Linux** end of one file-backed ring.
//!
//! Pair with `ring-file-client`, which is the Windows end and runs under Wine.
//! They are separate binaries because they are separate platforms: this one
//! `mmap`s the ring file, the other maps the same path with
//! `CreateFileMappingW`, and the bytes in between are the ordinary ring.
//!
//! The served file lives in memory (see `CONTENT`) so a failure here is a
//! failure of the ring or of the mapping, never of a provider or of the
//! filesystem underneath one.
//!
//! Modelled on `crates/vfs-server/tests/fuse_e2e.rs`, which drives the same
//! `RingServer`/`RingClient` + `SpinNotifier` pair over an `OwnedSeg` inside one
//! process.

#[cfg(unix)]
mod imp {
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::process::exit;
    use std::time::{Duration, Instant};

    use vfs_ipc::{ring, RingServer, SpinNotifier};
    use vfs_protocol as P;
    use vfs_unix::FileMapping;

    /// The single file this server serves.
    const VPATH: &str = "data/hello.txt";
    /// Must stay byte-identical to `ring-file-client.rs`'s `CONTENT`: the client
    /// asserts the read bytes equal its own copy, so a drift fails loudly rather
    /// than passing on a coincidence.
    const CONTENT: &[u8] = b"the-ring-crossed-the-wine-boundary";

    /// Geometry. `ring::open` on the client reads these back out of the header,
    /// so the only value that must be repeated on both command lines is the
    /// segment length.
    const SLOTS: u32 = 4;
    const PAYLOAD_CAP: u32 = 4096;

    /// Wall-clock bound. `SpinNotifier` never blocks, so without this a client
    /// that failed to attach leaves the server spinning forever.
    const PATIENCE: Duration = Duration::from_secs(120);

    fn fail(msg: &str) -> ! {
        eprintln!("SERVER FAIL: {msg}");
        let _ = std::io::stderr().flush();
        exit(1);
    }

    fn attr() -> Vec<u8> {
        P::encode_getattr_resp(&P::AttrResp {
            found: true,
            is_dir: false,
            size: CONTENT.len() as u64,
            mtime: 1,
        })
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        if args.len() != 3 {
            fail("usage: ring-file-server <ring-path> <ring-bytes>");
        }
        let path = PathBuf::from(&args[1]);
        let bytes: usize = args[2]
            .parse()
            .unwrap_or_else(|_| fail("ring-bytes must be a positive integer"));

        let mapping = FileMapping::create(&path, bytes)
            .unwrap_or_else(|e| fail(&format!("create {}: {e}", path.display())));
        ring::init(mapping.seg(), SLOTS, PAYLOAD_CAP)
            .unwrap_or_else(|e| fail(&format!("ring init: {e:?}")));

        // Printed *after* `ring::init` has published the header, so a client
        // that starts on this line cannot observe a half-built ring. The
        // harness waits for this line rather than sleeping a fixed interval.
        println!(
            "SERVER: ready path={} bytes={bytes} slots={SLOTS} cap={PAYLOAD_CAP}",
            path.display()
        );
        let _ = std::io::stdout().flush();

        let ring = RingServer::new(mapping.seg(), SpinNotifier)
            .unwrap_or_else(|e| fail(&format!("ring server: {e:?}")));

        let mut getattrs = 0u32;
        let mut reads = 0u32;
        let deadline = Instant::now() + PATIENCE;
        while getattrs == 0 || reads == 0 {
            if Instant::now() > deadline {
                fail(&format!(
                    "no client after {}s (getattr={getattrs} read={reads})",
                    PATIENCE.as_secs()
                ));
            }
            let mut note = String::new();
            // Set only on a fully-served request of the kind we are counting, so
            // the loop's exit condition cannot be satisfied by an error reply.
            let mut served: Option<u32> = None;
            let handled = ring
                .serve_one(|req| match req.opcode {
                    P::OP_GETATTR => match P::decode_path_req(&req.payload) {
                        Some((root, vpath)) if vpath == VPATH => {
                            note = format!("GETATTR root={root} {vpath} -> ST_OK");
                            served = Some(P::OP_GETATTR);
                            (P::ST_OK, attr())
                        }
                        Some((root, vpath)) => {
                            note = format!("GETATTR root={root} {vpath} -> ST_NOT_FOUND");
                            (P::ST_NOT_FOUND, Vec::new())
                        }
                        None => {
                            note = "GETATTR undecodable payload".to_string();
                            (P::ST_BAD_REQUEST, Vec::new())
                        }
                    },
                    // One in-memory file, so the handle is not consulted; the
                    // offset and length are.
                    P::OP_READ => match P::decode_read_req(&req.payload) {
                        Some(r) => {
                            let start = (r.offset as usize).min(CONTENT.len());
                            let end = (start + r.len as usize).min(CONTENT.len());
                            note = format!(
                                "READ fh={} off={} len={} -> {} bytes",
                                r.fh,
                                r.offset,
                                r.len,
                                end - start
                            );
                            served = Some(P::OP_READ);
                            (P::ST_OK, P::encode_read_resp(&CONTENT[start..end]))
                        }
                        None => {
                            note = "READ undecodable payload".to_string();
                            (P::ST_BAD_REQUEST, Vec::new())
                        }
                    },
                    other => {
                        note = format!("opcode {other} unexpected");
                        (P::ST_NOT_SUPPORTED, Vec::new())
                    }
                })
                .unwrap_or_else(|e| fail(&format!("serve_one: {e:?}")));
            if !handled {
                continue;
            }
            println!("SERVER: handled {note}");
            let _ = std::io::stdout().flush();
            match served {
                Some(P::OP_GETATTR) => getattrs += 1,
                Some(P::OP_READ) => reads += 1,
                _ => {}
            }
        }
        println!("SERVER: done getattr={getattrs} read={reads}");
        let _ = std::io::stdout().flush();
        exit(0);
    }
}

#[cfg(unix)]
fn main() {
    imp::main();
}

// The Windows build has no `vfs-unix` in its graph at all (the dependency is in
// a `cfg(unix)` table), so the body above cannot compile there. This keeps
// `cargo build --workspace` and `cargo clippy --all-targets` green on Windows.
#[cfg(not(unix))]
fn main() {}
