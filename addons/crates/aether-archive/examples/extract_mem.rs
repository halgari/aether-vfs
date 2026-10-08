//! Peak memory and time of extracting every file of a 7z or zip archive
//! with `threads` decoder threads (one run per process: the peak is the
//! process's own).
//!
//! ```text
//! cargo run --release -p aether-archive --example extract_mem -- <archive> <threads>
//! ```

use std::time::Instant;

use aether_archive::FormatError;
use aether_archive::extract::extract_threads;
use aether_archive::range::FileRange;

fn peak_rss_mib() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .unwrap_or(0)
        >> 10
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = &args[0];
    let threads: u32 = args[1].parse().unwrap();
    let names: Vec<String> = std::process::Command::new("7z")
        .args(["l", "-slt", path])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.strip_prefix("Path = ").map(str::to_string))
                .skip(1)
                .collect()
        })
        .unwrap();
    let r = FileRange::open(path).unwrap();
    let t0 = Instant::now();
    let mut bytes = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    extract_threads::<_, _, FormatError>(r, &names, threads, |_, r| {
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                return Ok(());
            }
            bytes += n as u64;
        }
    })
    .ok();
    println!(
        "threads {threads}: {} files, {:.0} MiB out in {:.1} s, peak RSS {} MiB",
        names.len(),
        bytes as f64 / (1 << 20) as f64,
        t0.elapsed().as_secs_f64(),
        peak_rss_mib()
    );
}
