//! `ring-file-server`: the **native Linux** end of one file-backed ring.
//!
//! Pair with `ring-file-client`, which is the Windows end and runs under Wine.
//! They are separate binaries because they are separate platforms: this one
//! `mmap`s the ring file, the other maps the same path with
//! `CreateFileMappingW`, and the bytes in between are the ordinary ring.
//!
//! Two files are served, both held in memory (no provider, no filesystem under
//! the answer, so a failure here is a failure of the ring or the mapping):
//!
//! - `data/hello.txt`, small enough to come back **inline** in the ring payload.
//! - `data/big.bin`, larger than the ring payload capacity, so it can only come
//!   back through the **bulk arena** — the path a real asset read takes. Its
//!   bytes are a repeating counter rather than a constant, so a misaligned or
//!   truncated copy fails instead of coincidentally matching.
//!
//! Modelled on `crates/vfs-server/tests/fuse_e2e.rs`, which drives the same
//! `RingServer`/`RingClient` + `SpinNotifier` pair, and the same
//! `DataArena`/`fill_bank` bulk handling, over an `OwnedSeg` in one process.

#[cfg(unix)]
mod imp {
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::process::exit;
    use std::time::{Duration, Instant};

    use vfs_ipc::{ring, DataArena, RingServer, SpinNotifier};
    use vfs_protocol as P;
    use vfs_unix::FileMapping;

    /// The small file, read inline.
    const VPATH_INLINE: &str = "data/hello.txt";
    /// Must stay byte-identical to the client's `CONTENT`: the client asserts
    /// the read bytes equal its own copy, so a drift fails loudly rather than
    /// passing on a coincidence.
    const CONTENT: &[u8] = b"the-ring-crossed-the-wine-boundary";

    /// The large file, which cannot fit in a ring payload and so can only be
    /// answered through the arena.
    const VPATH_BULK: &str = "data/big.bin";
    /// 64 KiB — unambiguously past `PAYLOAD_CAP`.
    const BULK_LEN: usize = 64 * 1024;

    /// There is no `OP_OPEN` in this harness: two fixed handles stand in for it
    /// by convention, so the ring and the arena stay the only things under test.
    /// `ring-file-client.rs` sends these same two values.
    const FH_INLINE: u64 = 1;
    const FH_BULK: u64 = 2;

    /// Geometry. The client reads the ring half back out of the header via
    /// `ring::open`, and the arena offset travels in each bulk response, so the
    /// only value repeated on both command lines is the segment length.
    const SLOTS: u32 = 4;
    const PAYLOAD_CAP: u32 = 4096;

    /// Wall-clock bound. `SpinNotifier` never blocks, so without this a client
    /// that failed to attach leaves the server spinning forever.
    const PATIENCE: Duration = Duration::from_secs(120);

    /// What a served request was, for the tally. Set only on a fully served
    /// request, so the serve loop cannot be satisfied by an error reply.
    enum Served {
        Getattr,
        InlineRead,
        BulkRead,
    }

    fn fail(msg: &str) -> ! {
        eprintln!("SERVER FAIL: {msg}");
        let _ = std::io::stderr().flush();
        exit(1);
    }

    /// `data/big.bin` byte at absolute offset `i`: a repeating counter with a
    /// prime period, so any offset slip or truncation changes what the client
    /// sees. A constant fill would hide exactly that.
    fn bulk_byte(i: usize) -> u8 {
        (i % 251) as u8
    }

    fn file_len(fh: u64) -> Option<usize> {
        match fh {
            FH_INLINE => Some(CONTENT.len()),
            FH_BULK => Some(BULK_LEN),
            _ => None,
        }
    }

    fn bytes_of(fh: u64, start: usize, len: usize) -> Vec<u8> {
        if fh == FH_BULK {
            (start..start + len).map(bulk_byte).collect()
        } else {
            CONTENT[start..start + len].to_vec()
        }
    }

    fn attr_for(vpath: &str) -> Option<Vec<u8>> {
        let size = match vpath {
            VPATH_INLINE => CONTENT.len() as u64,
            VPATH_BULK => BULK_LEN as u64,
            _ => return None,
        };
        Some(P::encode_getattr_resp(&P::AttrResp {
            found: true,
            is_dir: false,
            size,
            mtime: 1,
        }))
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
        let geom = ring::init(mapping.seg(), SLOTS, PAYLOAD_CAP)
            .unwrap_or_else(|e| fail(&format!("ring init: {e:?}")));

        // The arena occupies whatever follows the control ring inside the same
        // mapping — the arrangement `fuse_e2e`'s bulk test uses, and the reason
        // bulk coherence across the boundary is a question about the very pages
        // the ring itself already travels over.
        let ring_bytes = geom.slot_off(geom.slot_count);
        let arena_bytes = bytes.checked_sub(ring_bytes).unwrap_or_else(|| {
            fail(&format!(
                "segment of {bytes} bytes is smaller than the control ring's {ring_bytes}"
            ))
        });
        let arena = DataArena::new(mapping.seg(), ring_bytes, arena_bytes, SLOTS as usize);
        if arena.bank_size < BULK_LEN {
            fail(&format!(
                "arena bank is {} bytes, need {BULK_LEN} for {VPATH_BULK}; pass a larger ring-bytes (the ring itself takes {ring_bytes})",
                arena.bank_size
            ));
        }

        // Printed *after* `ring::init` has published the header, so a client
        // that starts on this line cannot observe a half-built ring. The
        // harness waits for this line rather than sleeping a fixed interval.
        println!(
            "SERVER: ready path={} bytes={bytes} slots={SLOTS} cap={PAYLOAD_CAP} arena_off={ring_bytes} bank={}",
            path.display(),
            arena.bank_size
        );
        let _ = std::io::stdout().flush();

        let ring = RingServer::new(mapping.seg(), SpinNotifier)
            .unwrap_or_else(|e| fail(&format!("ring server: {e:?}")));

        let mut getattrs = 0u32;
        let mut inline_reads = 0u32;
        let mut bulk_reads = 0u32;
        let deadline = Instant::now() + PATIENCE;
        while getattrs == 0 || inline_reads == 0 || bulk_reads == 0 {
            if Instant::now() > deadline {
                fail(&format!(
                    "no client after {}s (getattr={getattrs} inline_read={inline_reads} bulk_read={bulk_reads})",
                    PATIENCE.as_secs()
                ));
            }
            let mut note = String::new();
            let mut served: Option<Served> = None;
            let handled = ring
                .serve_one(|req| match req.opcode {
                    P::OP_GETATTR => match P::decode_path_req(&req.payload) {
                        Some((root, vpath)) => match attr_for(&vpath) {
                            Some(resp) => {
                                note = format!("GETATTR root={root} {vpath} -> ST_OK");
                                served = Some(Served::Getattr);
                                (P::ST_OK, resp)
                            }
                            None => {
                                note = format!("GETATTR root={root} {vpath} -> ST_NOT_FOUND");
                                (P::ST_NOT_FOUND, Vec::new())
                            }
                        },
                        None => {
                            note = "GETATTR undecodable payload".to_string();
                            (P::ST_BAD_REQUEST, Vec::new())
                        }
                    },
                    P::OP_READ => {
                        let Some(r) = P::decode_read_req(&req.payload) else {
                            note = "READ undecodable payload".to_string();
                            return (P::ST_BAD_REQUEST, Vec::new());
                        };
                        let Some(len) = file_len(r.fh) else {
                            note = format!("READ unknown fh={}", r.fh);
                            return (P::ST_BAD_FH, Vec::new());
                        };
                        let start = (r.offset as usize).min(len);
                        let max = (r.len as usize).min(len - start);
                        if (req.flags & P::FLAG_READ_BULK) != 0 {
                            // Mirrors `vfs_server::handler::dispatch_full`: the
                            // bytes go straight into this slot's arena bank and
                            // the reply carries only (len, mapping offset).
                            match arena.fill_bank(req.slot, max, |buf| {
                                for (j, b) in buf.iter_mut().enumerate().take(max) {
                                    *b = bulk_byte(start + j);
                                }
                                Ok(max)
                            }) {
                                Ok((off, n)) => {
                                    note = format!(
                                        "READ(BULK) fh={} off={} len={} -> {n} bytes at arena_off={off}",
                                        r.fh, r.offset, r.len
                                    );
                                    served = Some(Served::BulkRead);
                                    (P::ST_OK, P::encode_read_resp_bulk(n as u32, off))
                                }
                                Err(st) => {
                                    note = format!("READ(BULK) fill_bank refused, status {st}");
                                    (st, Vec::new())
                                }
                            }
                        } else {
                            let data = bytes_of(r.fh, start, max);
                            note = format!(
                                "READ fh={} off={} len={} -> {} bytes",
                                r.fh,
                                r.offset,
                                r.len,
                                data.len()
                            );
                            served = Some(Served::InlineRead);
                            (P::ST_OK, P::encode_read_resp(&data))
                        }
                    }
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
                Some(Served::Getattr) => getattrs += 1,
                Some(Served::InlineRead) => inline_reads += 1,
                Some(Served::BulkRead) => bulk_reads += 1,
                None => {}
            }
        }
        println!(
            "SERVER: done getattr={getattrs} inline_read={inline_reads} bulk_read={bulk_reads}"
        );
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
