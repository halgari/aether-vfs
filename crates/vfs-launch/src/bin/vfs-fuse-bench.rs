//! Benchmark director FUSE control-ring round-trips (shipped Server + RingClient).
//!
//! Usage:
//!   cargo run -p vfs-launch --bin vfs-fuse-bench --release
//!   cargo run -p vfs-launch --bin vfs-fuse-bench --release -- --zip
//!
//! Measures OPEN/GETATTR/HEARTBEAT/READ latency and sequential throughput over
//! the real `vfs-server` + `vfs-ipc` path (SpinNotifier, in-process threads).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use vfs_core::{encode_zip_window, EntryKind, InputEntry, Layer, LayerId, SourceId};
use vfs_ipc::ring::init;
use vfs_ipc::{OwnedSeg, RingClient, RingServer, SpinNotifier};
use vfs_protocol::{
    decode_getattr_resp, decode_open_resp, decode_read_resp, decode_read_resp_into,
    encode_close_req, encode_open_req, encode_path_req, encode_read_req, OpenResp, ReadReq,
    OP_CLOSE, OP_GETATTR, OP_HEARTBEAT, OP_OPEN, OP_READ, OPEN_READ, ST_OK,
};
use vfs_server::{OpenTable, Server, DEFAULT_PAYLOAD_CAP};

struct Stats {
    name: String,
    samples: Vec<Duration>,
}

impl Stats {
    fn new(name: impl Into<String>) -> Self {
        Stats {
            name: name.into(),
            samples: Vec::new(),
        }
    }
    fn push(&mut self, d: Duration) {
        self.samples.push(d);
    }
    fn report(&self) -> String {
        if self.samples.is_empty() {
            return format!("{}: no samples", self.name);
        }
        let mut ns: Vec<u64> = self.samples.iter().map(|d| d.as_nanos() as u64).collect();
        ns.sort_unstable();
        let n = ns.len();
        let sum: u64 = ns.iter().sum();
        let mean = sum / n as u64;
        let p50 = ns[n / 2];
        let p95 = ns[n.saturating_mul(95) / 100];
        let p99 = ns[n.saturating_mul(99) / 100];
        let min = ns[0];
        let max = ns[n - 1];
        format!(
            "{:<28} n={:<5} min={:>8.2}µs  p50={:>8.2}µs  mean={:>8.2}µs  p95={:>8.2}µs  p99={:>8.2}µs  max={:>8.2}µs",
            self.name,
            n,
            min as f64 / 1000.0,
            p50 as f64 / 1000.0,
            mean as f64 / 1000.0,
            p95 as f64 / 1000.0,
            p99 as f64 / 1000.0,
            max as f64 / 1000.0,
        )
    }
}

fn percentile_us(samples: &[Duration], pct: usize) -> f64 {
    let mut ns: Vec<u64> = samples.iter().map(|d| d.as_nanos() as u64).collect();
    ns.sort_unstable();
    let i = ns.len().saturating_mul(pct) / 100;
    ns[i.min(ns.len() - 1)] as f64 / 1000.0
}

fn mean_us(samples: &[Duration]) -> f64 {
    let sum: u128 = samples.iter().map(|d| d.as_nanos()).sum();
    (sum as f64 / samples.len() as f64) / 1000.0
}

fn main() {
    let use_zip = std::env::args().any(|a| a == "--zip");
    let payload_cap = DEFAULT_PAYLOAD_CAP;
    let warmup = 50usize;
    let iters = 500usize;

    let dir = std::env::temp_dir().join(format!("vfs-fuse-bench-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let disk_path = dir.join("bench.bin");
    // 64 MiB of patterned data (avoids zero-page special cases).
    let file_size = 64 * 1024 * 1024usize;
    {
        let mut f = std::fs::File::create(&disk_path).expect("create bench file");
        let chunk = (0..65536u32).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let mut written = 0;
        while written < file_size {
            let n = (file_size - written).min(chunk.len());
            f.write_all(&chunk[..n]).unwrap();
            written += n;
        }
    }

    let (vpath, layers, label) = if use_zip {
        // Prefer real GameLayers Skyrim.esm zip window if present.
        let zip = std::path::Path::new(r"C:\GameLayers\1. Skyrim Special Edition.zip");
        if zip.is_file() {
            match find_zip_entry_window(zip, "Data/Skyrim.esm") {
                Some((off, size)) => {
                    let src = encode_zip_window(off, &zip.to_string_lossy());
                    (
                        "Data/Skyrim.esm".to_string(),
                        vec![Layer {
                            id: LayerId(0),
                            entries: vec![InputEntry {
                                vpath: "Data/Skyrim.esm".into(),
                                kind: EntryKind::File,
                                source: SourceId::new(src),
                                size,
                                mtime: 1,
                            }],
                        }],
                        format!("zip-window Skyrim.esm ({} bytes) from {}", size, zip.display()),
                    )
                }
                None => disk_layers(&disk_path, file_size as u64),
            }
        } else {
            disk_layers(&disk_path, file_size as u64)
        }
    } else {
        disk_layers(&disk_path, file_size as u64)
    };
    let _ = label;

    let server = Arc::new(
        Server::from_layers_with_cap(layers, payload_cap).expect("server"),
    );
    // Ring large enough for 256 KiB payloads × 8 slots.
    let stride = ((32 + payload_cap as usize) + 7) & !7;
    let ring_bytes = 40 + 8 * stride + 4096;
    let owned = OwnedSeg::new(ring_bytes);
    init(owned.seg(), 8, payload_cap).expect("ring init");
    let stop = Arc::new(AtomicBool::new(false));

    // `thread::scope` keeps `owned` alive for both server and client.
    let doc = thread::scope(|scope| {
        let stop2 = stop.clone();
        let srv = server.clone();
        let seg = owned.seg();
        scope.spawn(move || {
            let ring = RingServer::new(seg, SpinNotifier).unwrap();
            while !stop2.load(Ordering::Relaxed) {
                match srv.serve_one(&ring) {
                    Ok(true) => {}
                    Ok(false) => std::hint::spin_loop(),
                    Err(_) => break,
                }
            }
        });

        thread::sleep(Duration::from_millis(10));
        let client = RingClient::new(owned.seg(), SpinNotifier).unwrap();

        let mut lines: Vec<String> = Vec::new();
        lines.push("# VFS Director FUSE RPC Benchmark".into());
        lines.push(String::new());
        lines.push(format!("- Date: {}", chrono_like_now()));
        lines.push(format!(
            "- Host: {}",
            std::env::var("COMPUTERNAME").unwrap_or_else(|_| "?".into())
        ));
        lines.push(format!(
            "- Build: {}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        ));
        lines.push(format!("- payload_cap: {payload_cap} bytes"));
        lines.push(
            "- Notifier: SpinNotifier (same-process client/server threads)".into(),
        );
        lines.push(format!("- File: {label}"));
        lines.push(format!(
            "- Warmup: {warmup}, timed iters: {iters} (latency); throughput separate"
        ));
        lines.push(String::new());

        // --- Latency: HEARTBEAT ---
        let mut hb = Stats::new("HEARTBEAT RTT");
        for i in 0..(warmup + iters) {
            let t0 = Instant::now();
            let r = client.submit(OP_HEARTBEAT, 0, &[]).unwrap();
            let dt = t0.elapsed();
            assert_eq!(r.status, ST_OK);
            if i >= warmup {
                hb.push(dt);
            }
        }
        lines.push("## Latency (control plane)".into());
        lines.push(String::new());
        lines.push("```".into());
        lines.push(hb.report());

        // --- GETATTR ---
        let mut ga = Stats::new("GETATTR RTT");
        let path_pl = encode_path_req(&vpath);
        for i in 0..(warmup + iters) {
            let t0 = Instant::now();
            let r = client.submit(OP_GETATTR, 0, &path_pl).unwrap();
            let dt = t0.elapsed();
            assert_eq!(r.status, ST_OK);
            let _ = decode_getattr_resp(&r.payload).unwrap();
            if i >= warmup {
                ga.push(dt);
            }
        }
        lines.push(ga.report());

        // --- OPEN + CLOSE ---
        let mut open_s = Stats::new("OPEN RTT");
        let mut close_s = Stats::new("CLOSE RTT");
        let open_pl = encode_open_req(OPEN_READ, &vpath);
        for i in 0..(warmup + iters) {
            let t0 = Instant::now();
            let r = client.submit(OP_OPEN, 0, &open_pl).unwrap();
            let dt = t0.elapsed();
            assert_eq!(r.status, ST_OK);
            let OpenResp { fh, .. } = decode_open_resp(&r.payload).unwrap();
            if i >= warmup {
                open_s.push(dt);
            }
            let t1 = Instant::now();
            let c = client.submit(OP_CLOSE, 0, &encode_close_req(fh)).unwrap();
            let dt1 = t1.elapsed();
            assert_eq!(c.status, ST_OK);
            if i >= warmup {
                close_s.push(dt1);
            }
        }
        lines.push(open_s.report());
        lines.push(close_s.report());
        lines.push("```".into());

        // --- READ latencies ---
        let open = client.submit(OP_OPEN, 0, &open_pl).unwrap();
        let OpenResp { fh, size, .. } = decode_open_resp(&open.payload).unwrap();
        lines.push(String::new());
        lines.push("## READ RTT (single RPC, data fits in payload_cap)".into());
        lines.push(String::new());
        lines.push("```".into());

        for chunk in [64usize, 512, 4096, 16384, 65536, 262144 - 8] {
            let len = chunk
                .min((size as usize).max(1))
                .min(payload_cap as usize - 8) as u32;
            let mut st = Stats::new(format!("READ {len} B"));
            let req = encode_read_req(&ReadReq {
                fh,
                offset: 0,
                len,
            });
            for i in 0..(warmup + iters) {
                let t0 = Instant::now();
                let r = client.submit(OP_READ, 0, &req).unwrap();
                let dt = t0.elapsed();
                assert_eq!(r.status, ST_OK);
                let data = decode_read_resp(&r.payload).unwrap();
                assert_eq!(data.len(), len as usize);
                if i >= warmup {
                    st.push(dt);
                }
            }
            lines.push(st.report());
            let mean = mean_us(&st.samples);
            let mib_s = (len as f64) / (mean * 1e-6) / (1024.0 * 1024.0);
            lines.push(format!(
                "  └─ implied throughput @ mean: {mib_s:.1} MiB/s  (p50 RTT {:.2} µs)",
                percentile_us(&st.samples, 50)
            ));
        }
        lines.push("```".into());

        // --- Sequential throughput (A3 into scratch + A5 pipelined depth 4) ---
        let read_bytes = (size as usize).min(32 * 1024 * 1024);
        let max_chunk = (payload_cap as usize).saturating_sub(8);
        const PIPE_DEPTH: usize = 4;
        let mut sink = vec![0u8; max_chunk]; // A3: decode_read_resp_into (no second heap alloc)
        let t0 = Instant::now();
        let mut off = 0u64;
        let mut total = 0usize;
        let mut rpc_count = 0usize;
        'seq: while total < read_bytes {
            let mut reqs: Vec<(u32, u32, Vec<u8>)> = Vec::new();
            let mut plan_off = off;
            let mut wants: Vec<usize> = Vec::new();
            while reqs.len() < PIPE_DEPTH && total + wants.iter().sum::<usize>() < read_bytes {
                let already = wants.iter().sum::<usize>();
                let want = ((read_bytes - total - already) as u64).min(max_chunk as u64) as u32;
                if want == 0 {
                    break;
                }
                reqs.push((
                    OP_READ,
                    0,
                    encode_read_req(&ReadReq {
                        fh,
                        offset: plan_off,
                        len: want,
                    }),
                ));
                plan_off += want as u64;
                wants.push(want as usize);
            }
            if reqs.is_empty() {
                break;
            }
            rpc_count += reqs.len();
            let responses = client.submit_many(&reqs).unwrap();
            for (r, want) in responses.iter().zip(wants.iter()) {
                assert_eq!(r.status, ST_OK);
                let n = decode_read_resp_into(&r.payload, &mut sink[..*want]).unwrap();
                if n == 0 {
                    break 'seq;
                }
                total += n;
                off += n as u64;
                if n < *want {
                    break 'seq;
                }
            }
        }
        let elapsed = t0.elapsed();
        let _ = client.submit(OP_CLOSE, 0, &encode_close_req(fh));

        let secs = elapsed.as_secs_f64().max(1e-9);
        let mib = total as f64 / (1024.0 * 1024.0);
        let mib_s = mib / secs;

        lines.push(String::new());
        lines.push("## Sequential throughput (fragmented READ RPCs)".into());
        lines.push(String::new());
        lines.push(format!(
            "- Bytes read: {total} ({mib:.2} MiB) in {rpc_count} RPC(s)"
        ));
        lines.push(format!(
            "- Wall time: {elapsed:?} ({:.2} ms)",
            elapsed.as_secs_f64() * 1000.0
        ));
        lines.push(format!("- Throughput: **{mib_s:.1} MiB/s**"));
        lines.push(format!(
            "- Avg time per RPC: {:.2} µs",
            elapsed.as_secs_f64() * 1e6 / rpc_count as f64
        ));

        // --- Baseline: OpenTable direct ---
        let tree = server.tree();
        let table = OpenTable::new();
        let open = table.open(tree, &vpath, OPEN_READ).expect("direct open");
        let t0 = Instant::now();
        let mut off = 0u64;
        let mut total_d = 0usize;
        while total_d < read_bytes {
            let want = ((read_bytes - total_d) as u64)
                .min(max_chunk as u64)
                .min(1_048_576) as u32;
            let chunk = table
                .read(open.fh, off, want, want as usize)
                .expect("direct read");
            if chunk.is_empty() {
                break;
            }
            total_d += chunk.len();
            off += chunk.len() as u64;
        }
        let elapsed_d = t0.elapsed();
        let _ = table.close(open.fh);
        let mib_d = total_d as f64 / (1024.0 * 1024.0) / elapsed_d.as_secs_f64().max(1e-9);

        // std::fs baseline
        let t0 = Instant::now();
        let mut total_fs = 0usize;
        if let Ok(mut file) = std::fs::File::open(&disk_path) {
            use std::io::Read;
            let mut buf = vec![0u8; max_chunk.min(1_048_576)];
            while total_fs < read_bytes.min(file_size) {
                let n = file.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                total_fs += n;
            }
        }
        let elapsed_fs = t0.elapsed();
        let mib_fs = if total_fs > 0 {
            total_fs as f64 / (1024.0 * 1024.0) / elapsed_fs.as_secs_f64().max(1e-9)
        } else {
            0.0
        };

        lines.push(String::new());
        lines.push("## Baselines (same host, for context)".into());
        lines.push(String::new());
        lines.push(format!(
            "- **OpenTable::read direct** (no IPC, same Server tree): **{mib_d:.1} MiB/s** over {total_d} bytes"
        ));
        if total_fs > 0 {
            lines.push(format!(
                "- **std::fs::File sequential** on bench.bin: **{mib_fs:.1} MiB/s** over {total_fs} bytes"
            ));
        }
        lines.push(format!(
            "- **IPC overhead factor**: {:.1}× slower than OpenTable direct (throughput ratio)",
            mib_d / mib_s.max(0.001)
        ));

        lines.push(String::new());
        lines.push("## Notes".into());
        lines.push(String::new());
        lines.push(
            "- Latency numbers use **SpinNotifier** in one process (two threads). Production event wait will add OS wake latency (typically tens of µs) on idle rings."
                .into(),
        );
        lines.push(
            "- Each READ response is capped at `payload_cap - 8` bytes; large files are fragmented into multiple RPCs."
                .into(),
        );
        lines.push(
            "- Debug builds are substantially slower; prefer `--release` for published numbers."
                .into(),
        );
        lines.push(String::new());
        lines.push("## Summary table".into());
        lines.push(String::new());
        lines.push("| Metric | Value |".into());
        lines.push("|--------|-------|".into());
        lines.push(format!(
            "| HEARTBEAT p50 | {:.2} µs |",
            percentile_us(&hb.samples, 50)
        ));
        lines.push(format!(
            "| GETATTR p50 | {:.2} µs |",
            percentile_us(&ga.samples, 50)
        ));
        lines.push(format!(
            "| OPEN p50 | {:.2} µs |",
            percentile_us(&open_s.samples, 50)
        ));
        lines.push(format!(
            "| CLOSE p50 | {:.2} µs |",
            percentile_us(&close_s.samples, 50)
        ));
        lines.push(format!(
            "| Sequential RPC throughput | **{mib_s:.1} MiB/s** |"
        ));
        lines.push(format!(
            "| OpenTable direct throughput | **{mib_d:.1} MiB/s** |"
        ));
        if total_fs > 0 {
            lines.push(format!("| std::fs throughput | **{mib_fs:.1} MiB/s** |"));
        }

        stop.store(true, Ordering::Relaxed);
        lines.join("\n")
    });

    println!("{doc}");

    let out_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/benchmarks");
    let _ = std::fs::create_dir_all(&out_dir);
    let stamp = chrono_like_now()
        .replace(':', "-")
        .replace(' ', "_")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect::<String>();
    let out_path = out_dir.join(format!("fuse-rpc-{stamp}.md"));
    let latest = out_dir.join("fuse-rpc-latest.md");
    std::fs::write(&out_path, &doc).expect("write bench doc");
    std::fs::write(&latest, &doc).expect("write latest");
    eprintln!("\nWrote {}", out_path.display());
    eprintln!("Wrote {}", latest.display());

    let _ = std::fs::remove_dir_all(&dir);
}

fn disk_layers(path: &std::path::Path, size: u64) -> (String, Vec<Layer>, String) {
    let src = path.to_string_lossy().into_owned();
    (
        "data/bench.bin".into(),
        vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/bench.bin".into(),
                kind: EntryKind::File,
                source: src.as_str().into(),
                size,
                mtime: 1,
            }],
        }],
        format!("disk {} ({} bytes)", path.display(), size),
    )
}

/// Locate a Stored zip entry by basename suffix; returns (data_offset, size).
fn find_zip_entry_window(zip: &std::path::Path, want_vpath: &str) -> Option<(u64, u64)> {
    use vfs_zip::read_layer;
    let layer = read_layer(zip, LayerId(0)).ok()?;
    let want = want_vpath.replace('\\', "/").to_ascii_lowercase();
    for e in &layer.entries {
        if e.kind != EntryKind::File {
            continue;
        }
        let vp = e.vpath.replace('\\', "/").to_ascii_lowercase();
        if vp == want || vp.ends_with(&want) {
            match vfs_core::decode(&e.source.0) {
                vfs_core::Source::ZipWindow { offset, .. } => return Some((offset, e.size)),
                _ => {}
            }
        }
    }
    None
}

fn chrono_like_now() -> String {
    // Avoid chrono dep: local system time via Windows-friendly format.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Rough UTC stamp from epoch (good enough for bench filenames).
    // Prefer ISO-like via local time API if available.
    #[cfg(windows)]
    {
        use std::mem::zeroed;
        // fallback string
        let _ = now;
    }
    format!(
        "{}",
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "Get-Date -Format o"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| format!("unix-{now}"))
    )
}
