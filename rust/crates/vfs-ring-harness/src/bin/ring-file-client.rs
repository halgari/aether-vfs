//! `ring-file-client`: the **Windows** end of one file-backed ring, run under
//! Wine.
//!
//! Pair with `ring-file-server`, the native Linux end. This side maps the ring
//! file with `CreateFileMappingW` over a real file handle — not a named,
//! page-file-backed section, which has no identity a Linux process could open —
//! and coordinates with the server by **path** rather than by section name.
//!
//! Exit 0 with `CLIENT: OK` means the ring carried our protocol across the
//! boundary; any mismatch exits nonzero with a diagnostic on stderr.

#[cfg(windows)]
mod imp {
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::process::exit;

    use vfs_ipc::{RingClient, SpinNotifier};
    use vfs_protocol as P;
    use vfs_win::SharedMapping;

    /// Must match `ring-file-server.rs`'s `VPATH`.
    const VPATH: &str = "data/hello.txt";
    /// Must stay byte-identical to `ring-file-server.rs`'s `CONTENT`.
    const CONTENT: &[u8] = b"the-ring-crossed-the-wine-boundary";

    fn fail(msg: &str) -> ! {
        eprintln!("CLIENT FAIL: {msg}");
        let _ = std::io::stderr().flush();
        exit(1);
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        if args.len() != 3 {
            fail("usage: ring-file-client <ring-path> <ring-bytes>");
        }
        let path = PathBuf::from(&args[1]);
        let bytes: usize = args[2]
            .parse()
            .unwrap_or_else(|_| fail("ring-bytes must be a positive integer"));

        let mapping = SharedMapping::open_file_backed(&path, bytes)
            .unwrap_or_else(|e| fail(&format!("open_file_backed {}: {e}", path.display())));
        println!("CLIENT: mapped {} bytes={bytes}", path.display());
        let _ = std::io::stdout().flush();

        // `ring::open` (inside `RingClient::new`) validates magic, wire version
        // and geometry, so a mismatch is refused here rather than misparsed.
        let client = RingClient::new(mapping.seg(), SpinNotifier)
            .unwrap_or_else(|e| fail(&format!("ring open: {e:?} (segment length mismatch?)")));
        let geom = client.geom();
        println!(
            "CLIENT: ring slots={} stride={} cap={}",
            geom.slot_count, geom.slot_stride, geom.payload_cap
        );
        let _ = std::io::stdout().flush();

        let g = client
            .submit(P::OP_GETATTR, 0, &P::encode_path_req(0, VPATH))
            .unwrap_or_else(|e| fail(&format!("getattr submit: {e:?}")));
        if g.status != P::ST_OK {
            fail(&format!("getattr status {} (want ST_OK)", g.status));
        }
        let a = P::decode_getattr_resp(&g.payload).unwrap_or_else(|| fail("getattr decode"));
        if !a.found || a.is_dir || a.size != CONTENT.len() as u64 {
            fail(&format!(
                "getattr wrong: found={} is_dir={} size={} (want size {})",
                a.found,
                a.is_dir,
                a.size,
                CONTENT.len()
            ));
        }
        println!("CLIENT: getattr {VPATH} size={}", a.size);
        let _ = std::io::stdout().flush();

        // Inline READ: the bytes come back through the ring payload itself, so
        // this asserts the shared pages both ways in one hop. `fh` is 1 because
        // the server backs a single in-memory file and never consults it.
        let r = client
            .submit(
                P::OP_READ,
                0,
                &P::encode_read_req(&P::ReadReq {
                    fh: 1,
                    offset: 0,
                    len: a.size as u32,
                }),
            )
            .unwrap_or_else(|e| fail(&format!("read submit: {e:?}")));
        if r.status != P::ST_OK {
            fail(&format!("read status {} (want ST_OK)", r.status));
        }
        let data = P::decode_read_resp(&r.payload).unwrap_or_else(|| fail("read decode"));
        if data != CONTENT {
            fail(&format!(
                "read mismatch: got {:?}, want {:?}",
                String::from_utf8_lossy(&data),
                String::from_utf8_lossy(CONTENT)
            ));
        }
        println!("CLIENT: read {} bytes, content matches", data.len());
        println!("CLIENT: OK");
        let _ = std::io::stdout().flush();
        exit(0);
    }
}

#[cfg(windows)]
fn main() {
    imp::main();
}

// `vfs-win` is only in the Windows dependency table, so the body above cannot
// compile on Linux; this keeps a Linux `cargo build` of this crate green.
#[cfg(not(windows))]
fn main() {}
