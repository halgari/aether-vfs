//! `ring-file-client`: the **Windows** end of one file-backed ring, run under
//! Wine.
//!
//! Pair with `ring-file-server`, the native Linux end. This side maps the ring
//! file with `CreateFileMappingW` over a real file handle — not a named,
//! page-file-backed section, which has no identity a Linux process could open —
//! and coordinates with the server by **path** rather than by section name.
//!
//! Three assertions, in order: a `GETATTR`, an **inline** `READ` whose bytes
//! travel in the ring payload, and a **bulk** `READ` whose bytes travel through
//! the shared arena instead. The bulk case is the one large asset reads take, so
//! an inline-only proof would leave the transport's real workload untested.
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

    /// Must match the server's `VPATH_INLINE`.
    const VPATH_INLINE: &str = "data/hello.txt";
    /// Must stay byte-identical to the server's `CONTENT`.
    const CONTENT: &[u8] = b"the-ring-crossed-the-wine-boundary";
    /// Must match the server's `BULK_LEN` — 64 KiB, well past the ring payload
    /// capacity, so this read cannot be answered inline.
    const BULK_LEN: usize = 64 * 1024;

    /// The server's fixed stand-ins for `OP_OPEN`; see its `FH_INLINE`/`FH_BULK`.
    const FH_INLINE: u64 = 1;
    const FH_BULK: u64 = 2;

    fn fail(msg: &str) -> ! {
        eprintln!("CLIENT FAIL: {msg}");
        let _ = std::io::stderr().flush();
        exit(1);
    }

    /// Must match the server's `bulk_byte`. A repeating counter, so a copy that
    /// is offset or truncated mismatches instead of coincidentally agreeing.
    fn bulk_byte(i: usize) -> u8 {
        (i % 251) as u8
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
            .submit(P::OP_GETATTR, 0, &P::encode_path_req(0, VPATH_INLINE))
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
        println!("CLIENT: getattr {VPATH_INLINE} size={}", a.size);
        let _ = std::io::stdout().flush();

        // Inline READ: the bytes come back through the ring payload itself, so
        // this asserts the shared pages both ways in one hop.
        let r = client
            .submit(
                P::OP_READ,
                0,
                &P::encode_read_req(&P::ReadReq {
                    fh: FH_INLINE,
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
        println!("CLIENT: read {} bytes inline, content matches", data.len());
        let _ = std::io::stdout().flush();

        // Bulk READ: 64 KiB cannot fit in a 4 KiB ring payload, so the reply
        // carries only (len, arena offset) and the bytes are read straight out
        // of the shared arena — the same handling as `fuse_e2e`'s bulk test.
        let b = client
            .submit(
                P::OP_READ,
                P::FLAG_READ_BULK,
                &P::encode_read_req(&P::ReadReq {
                    fh: FH_BULK,
                    offset: 0,
                    len: BULK_LEN as u32,
                }),
            )
            .unwrap_or_else(|e| fail(&format!("bulk read submit: {e:?}")));
        if b.status != P::ST_OK {
            fail(&format!("bulk read status {} (want ST_OK)", b.status));
        }
        // Asserted, not assumed: an inline reply here would prove nothing about
        // the arena, so it must fail rather than quietly pass.
        if !P::is_read_resp_bulk(&b.payload) {
            fail(&format!(
                "bulk read came back INLINE ({} payload bytes) — the arena path was not exercised",
                b.payload.len()
            ));
        }
        let (n, aoff) =
            P::decode_read_bulk_resp(&b.payload).unwrap_or_else(|| fail("bulk read decode"));
        if n as usize != BULK_LEN {
            fail(&format!("bulk read returned {n} bytes, want {BULK_LEN}"));
        }
        // Safe to read the bank after `submit` released the slot: this is the
        // only client, single-threaded, so nothing can reclaim the slot and
        // overwrite the bank in between (`submit_many_held` exists for the
        // concurrent case).
        let mut buf = vec![0u8; n as usize];
        mapping
            .seg()
            .copy_to(aoff as usize, &mut buf)
            .unwrap_or_else(|| {
                fail(&format!(
                    "arena offset {aoff} + {n} bytes is outside the {bytes}-byte mapping"
                ))
            });
        if let Some(i) = (0..buf.len()).find(|&i| buf[i] != bulk_byte(i)) {
            fail(&format!(
                "bulk pattern mismatch at byte {i} of {n}: got {}, want {}",
                buf[i],
                bulk_byte(i)
            ));
        }
        println!("CLIENT: bulk read {n} bytes at arena_off={aoff}, pattern matches");
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
