//! Per-file access timeline: when the process first opened, first read, last
//! read and last closed each file the director serves, and how much it read.
//!
//! The stats report cannot answer "when was the engine working on which
//! plugin". Its ordered trace stops at 4,000 operations, and most reads of a
//! plugin never reach the director at all: the read cache (`read_cache`)
//! answers them from 64 KiB units, so the director's own log sees a handful of
//! 1 MiB fetches per file. This records on the shim's side of the cache, for
//! every `NtReadFile` served through a synthetic handle: cache hits, cache
//! misses and reads sent over the ring alike.
//!
//! Off unless `VFS_SHIM_ACCESS_LOG` ([`vfs_env::SHIM_ACCESS_LOG`]) names a
//! file. Off, a read pays one relaxed load of [`ON`] and nothing else: the
//! handle's file id is never assigned, and no clock is read.
//!
//! **Cost when on.** The hook already takes `fuse_synth`'s table lock once per
//! read; the file id rides in that same lookup (`ReadView::access`), so the
//! recorder adds no lock and no map lookup per read. The id indexes a chunked
//! array of atomic entries, so a read is a clock read, a thread-id read and a
//! few relaxed atomic RMWs on the file's own entry. Only an *open* takes a
//! lock (the path → id index), and an open is already a ring round trip.
//!
//! **What it cannot see.** Only director-served file handles are recorded:
//! not directories, not files a redirect sent to the overlay on real disk, and
//! not demand-paged section reads (a page fault, not `NtReadFile`).
//!
//! Written like the stats report: periodically, via a temp file and a rename,
//! on the `VFS_SHIM_STATS_INTERVAL_MS` interval, and **never at exit** — see
//! `hookstats::banner` for why there is no exit flush. A trailing `#` line
//! says when the snapshot was taken.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Entries per lazily allocated chunk.
const CHUNK: usize = 4096;
/// Chunks: 64 × 4096 = 262,144 distinct files before the cap.
const CHUNKS: usize = 64;
/// Most distinct files the timeline holds. Past it, opens of new files are
/// counted as dropped and the output says so.
pub const MAX_FILES: usize = CHUNK * CHUNKS;
/// Reader threads tracked per file. A file read by more renders its thread
/// count as `16+`, and its main thread is the busiest of the first 16.
const TID_SLOTS: usize = 16;
/// "Never" for a `first_*` time.
const UNSET: u64 = u64::MAX;

/// The TSV header, the file's first line.
pub const HEADER: &str =
    "path\tfirst_open_us\tfirst_read_us\tlast_read_us\tlast_close_us\treads\tbytes\tmain_tid\tthreads";

/// One file's timeline. `first_*` hold microseconds or [`UNSET`]; `last_*`
/// hold microseconds **plus one**, so zero is "never" and `fetch_max` works
/// from the zeroed start.
struct Entry {
    name: OnceLock<Box<str>>,
    first_open: AtomicU64,
    last_close: AtomicU64,
    first_read: AtomicU64,
    last_read: AtomicU64,
    reads: AtomicU64,
    bytes: AtomicU64,
    /// Reader thread ids, claimed by compare-exchange from zero (no Windows
    /// user thread has id 0), with their read counts beside them.
    tids: [AtomicU32; TID_SLOTS],
    tid_reads: [AtomicU64; TID_SLOTS],
    /// More reader threads than `TID_SLOTS`.
    tid_overflow: AtomicBool,
}

impl Entry {
    fn new() -> Self {
        Entry {
            name: OnceLock::new(),
            first_open: AtomicU64::new(UNSET),
            last_close: AtomicU64::new(0),
            first_read: AtomicU64::new(UNSET),
            last_read: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            tids: [const { AtomicU32::new(0) }; TID_SLOTS],
            tid_reads: [const { AtomicU64::new(0) }; TID_SLOTS],
            tid_overflow: AtomicBool::new(false),
        }
    }

    fn read(&self, t: u64, bytes: u64, tid: u32) {
        // Load before the RMW: after the first read this is a plain load.
        if self.first_read.load(Ordering::Relaxed) == UNSET {
            self.first_read.fetch_min(t, Ordering::Relaxed);
        }
        self.last_read.fetch_max(t + 1, Ordering::Relaxed);
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        for i in 0..TID_SLOTS {
            let slot = &self.tids[i];
            let mut cur = slot.load(Ordering::Relaxed);
            if cur == 0 {
                // Claim the free slot; losing the race leaves the winner's id.
                cur = match slot.compare_exchange(0, tid, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => tid,
                    Err(other) => other,
                };
            }
            if cur == tid {
                self.tid_reads[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        self.tid_overflow.store(true, Ordering::Relaxed);
    }
}

/// The path → id index. Ids are `index + 1`; zero means "not recorded".
#[derive(Default)]
struct Index {
    ids: HashMap<Box<str>, u32>,
}

/// One timeline. The process has one ([`global`]); tests build their own.
pub struct Recorder {
    start: Instant,
    chunks: [OnceLock<Box<[Entry]>>; CHUNKS],
    index: Mutex<Index>,
    /// Ids handed out; an entry below this has its name set.
    len: AtomicU32,
    /// Opens of new files refused because the table was full.
    dropped: AtomicU64,
}

impl Recorder {
    pub fn new(start: Instant) -> Self {
        Recorder {
            start,
            chunks: [const { OnceLock::new() }; CHUNKS],
            index: Mutex::new(Index::default()),
            len: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    fn entry(&self, id: u32) -> Option<&Entry> {
        let i = (id as usize).checked_sub(1)?;
        self.chunks.get(i / CHUNK)?.get()?.get(i % CHUNK)
    }

    /// The id for `key`, assigning one on first sight; zero past the cap.
    pub fn id(&self, key: &str) -> u32 {
        let Ok(mut idx) = self.index.lock() else {
            return 0;
        };
        if let Some(&id) = idx.ids.get(key) {
            return id;
        }
        let i = self.len.load(Ordering::Relaxed) as usize;
        if i >= MAX_FILES {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        let chunk =
            self.chunks[i / CHUNK].get_or_init(|| (0..CHUNK).map(|_| Entry::new()).collect());
        let _ = chunk[i % CHUNK].name.set(key.into());
        let id = i as u32 + 1;
        idx.ids.insert(key.into(), id);
        // Release: a renderer that sees `len` sees the name it covers.
        self.len.store(id, Ordering::Release);
        id
    }

    /// A handle to file `key` was opened. Returns its id for the handle.
    pub fn open(&self, key: &str) -> u32 {
        let id = self.id(key);
        if let Some(e) = self.entry(id) {
            let t = self.now_us();
            if e.first_open.load(Ordering::Relaxed) == UNSET {
                e.first_open.fetch_min(t, Ordering::Relaxed);
            }
        }
        id
    }

    /// A handle to file `id` was closed.
    pub fn close(&self, id: u32) {
        if let Some(e) = self.entry(id) {
            e.last_close.fetch_max(self.now_us() + 1, Ordering::Relaxed);
        }
    }

    /// A read of `bytes` from file `id` completed on this thread.
    #[inline]
    pub fn read(&self, id: u32, bytes: u64) {
        if let Some(e) = self.entry(id) {
            e.read(self.now_us(), bytes, current_tid());
        }
    }

    /// Every recorded file, read back once.
    fn rows(&self) -> Vec<Row> {
        let n = self.len.load(Ordering::Acquire);
        (1..=n)
            .filter_map(|id| {
                let e = self.entry(id)?;
                let mut main = (0u32, 0u64);
                let mut threads = 0u32;
                for i in 0..TID_SLOTS {
                    let tid = e.tids[i].load(Ordering::Relaxed);
                    if tid == 0 {
                        continue;
                    }
                    threads += 1;
                    let c = e.tid_reads[i].load(Ordering::Relaxed);
                    if c > main.1 {
                        main = (tid, c);
                    }
                }
                let last = |a: &AtomicU64| a.load(Ordering::Relaxed).checked_sub(1);
                let first = |a: &AtomicU64| Some(a.load(Ordering::Relaxed)).filter(|&t| t != UNSET);
                Some(Row {
                    path: e.name.get()?.to_string(),
                    first_open: first(&e.first_open),
                    first_read: first(&e.first_read),
                    last_read: last(&e.last_read),
                    last_close: last(&e.last_close),
                    reads: e.reads.load(Ordering::Relaxed),
                    bytes: e.bytes.load(Ordering::Relaxed),
                    main_tid: main.0,
                    threads,
                    threads_overflow: e.tid_overflow.load(Ordering::Relaxed),
                })
            })
            .collect()
    }

    /// The whole timeline as TSV, as of now.
    pub fn render(&self) -> String {
        render_rows(
            self.rows(),
            self.dropped.load(Ordering::Relaxed),
            self.now_us(),
        )
    }
}

/// One file, read back for rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    path: String,
    first_open: Option<u64>,
    first_read: Option<u64>,
    last_read: Option<u64>,
    last_close: Option<u64>,
    reads: u64,
    bytes: u64,
    main_tid: u32,
    threads: u32,
    threads_overflow: bool,
}

/// Sorted by first read (files never read last, by first open), then path.
fn render_rows(mut rows: Vec<Row>, dropped: u64, now_us: u64) -> String {
    rows.sort_by(|a, b| {
        (
            a.first_read.unwrap_or(UNSET),
            a.first_open.unwrap_or(UNSET),
            &a.path,
        )
            .cmp(&(
                b.first_read.unwrap_or(UNSET),
                b.first_open.unwrap_or(UNSET),
                &b.path,
            ))
    });
    let t = |v: Option<u64>| v.map_or_else(|| "-".to_string(), |v| v.to_string());
    let mut s = String::with_capacity(64 + rows.len() * 96);
    s.push_str(HEADER);
    s.push('\n');
    for r in &rows {
        let _ = writeln!(
            s,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}{}",
            r.path,
            t(r.first_open),
            t(r.first_read),
            t(r.last_read),
            t(r.last_close),
            r.reads,
            r.bytes,
            if r.main_tid == 0 {
                "-".to_string()
            } else {
                r.main_tid.to_string()
            },
            r.threads,
            if r.threads_overflow { "+" } else { "" },
        );
    }
    if dropped > 0 {
        let _ = writeln!(
            s,
            "# CAP HIT: the timeline holds {MAX_FILES} files; {dropped} opens of further files \
             were not recorded, so rows are missing"
        );
    }
    let _ = writeln!(
        s,
        "# snapshot at t+{now_us}us, {} files; no exit write exists, so an absent row or a \
         missing last_close means \"not by then\"",
        rows.len()
    );
    s
}

#[allow(unsafe_code)]
#[inline]
fn current_tid() -> u32 {
    // SAFETY: no arguments; reads the calling thread's TEB.
    unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
}

/// Whether the timeline is on. Set once by [`init`]; the hot paths test it
/// with one relaxed load.
static ON: AtomicBool = AtomicBool::new(false);
static GLOBAL: OnceLock<Recorder> = OnceLock::new();
static WRITER: AtomicBool = AtomicBool::new(false);

/// Turn the timeline on if `VFS_SHIM_ACCESS_LOG` names a file, and start the
/// thread that writes it. Call at install, beside `hookstats::start_reporter`,
/// before the detours are live: time zero is this call.
pub fn init() {
    let Some(path) = vfs_env::raw(vfs_env::SHIM_ACCESS_LOG) else {
        return;
    };
    if WRITER.swap(true, Ordering::SeqCst) {
        return;
    }
    let rec = GLOBAL.get_or_init(|| Recorder::new(Instant::now()));
    ON.store(true, Ordering::Release);
    let interval = crate::hookstats::report_interval();
    let _ = std::thread::Builder::new()
        .name("vfs-shim-access".into())
        .spawn(move || {
            // Rendering a large timeline is not free; skip it when nothing
            // moved since the last write.
            let mut last = None;
            loop {
                std::thread::sleep(interval);
                let mark = (
                    rec.len.load(Ordering::Relaxed),
                    rec.dropped.load(Ordering::Relaxed),
                    activity(rec),
                );
                if last == Some(mark) {
                    continue;
                }
                last = Some(mark);
                crate::hookstats::write_report(&path, &rec.render());
            }
        });
}

/// A number that moves whenever any entry does: reads, opens or closes.
fn activity(rec: &Recorder) -> u64 {
    let n = rec.len.load(Ordering::Acquire);
    (1..=n)
        .filter_map(|id| rec.entry(id))
        .map(|e| {
            e.reads.load(Ordering::Relaxed)
                ^ e.last_close.load(Ordering::Relaxed).rotate_left(21)
                ^ e.first_open.load(Ordering::Relaxed).rotate_left(42)
        })
        .fold(0u64, |a, v| a.wrapping_add(v))
}

#[inline]
fn on() -> Option<&'static Recorder> {
    if !ON.load(Ordering::Relaxed) {
        return None;
    }
    GLOBAL.get()
}

/// A director file handle to `root:vpath` was opened. Returns the id to keep
/// on the handle, zero when the timeline is off (or full).
pub fn note_open(root: u32, vpath: &str) -> u32 {
    match on() {
        Some(rec) => rec.open(&format!("{root}:{vpath}")),
        None => 0,
    }
}

/// A handle carrying `id` was closed.
#[inline]
pub fn note_close(id: u32) {
    if let Some(rec) = on() {
        rec.close(id);
    }
}

/// A read through a handle carrying `id` returned `bytes`.
#[inline]
pub fn note_read(id: u32, bytes: u64) {
    if let Some(rec) = on() {
        rec.read(id, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(rows: &'a [Row], path: &str) -> &'a Row {
        rows.iter()
            .find(|r| r.path == path)
            .unwrap_or_else(|| panic!("no row {path}: {rows:?}"))
    }

    /// `VFS_SHIM_ACCESS_LOG` is unset under test and `init` never ran, so the
    /// process-wide recorder must not exist and every entry point is inert.
    #[test]
    fn disabled_records_nothing_and_assigns_no_id() {
        assert!(!ON.load(Ordering::Relaxed));
        assert_eq!(note_open(0, "data/skyrim.esm"), 0);
        note_read(1, 100);
        note_close(1);
        assert!(
            GLOBAL.get().is_none(),
            "a disabled timeline allocated its recorder"
        );
    }

    #[test]
    fn first_and_last_times_move_the_right_way() {
        let rec = Recorder::new(Instant::now());
        let id = rec.open("0:data/a.esp");
        assert_eq!(rec.open("0:data/a.esp"), id, "one path, one id");
        rec.read(id, 10);
        let r0 = rec.rows()[0].clone();
        assert_eq!(r0.reads, 1);
        assert_eq!(r0.bytes, 10);
        assert_eq!(r0.first_read, r0.last_read);
        assert!(r0.first_open.unwrap() <= r0.first_read.unwrap());
        assert_eq!(r0.last_close, None);
        std::thread::sleep(std::time::Duration::from_millis(3));
        rec.read(id, 20);
        rec.close(id);
        let r1 = rec.rows()[0].clone();
        assert_eq!((r1.reads, r1.bytes), (2, 30));
        assert_eq!(r1.first_read, r0.first_read, "first read must not move");
        assert_eq!(r1.first_open, r0.first_open, "first open must not move");
        assert!(
            r1.last_read.unwrap() >= r0.last_read.unwrap() + 2_000,
            "{r0:?} {r1:?}"
        );
        assert!(r1.last_close.unwrap() >= r1.last_read.unwrap());
        // A second open later leaves the first open where it was.
        rec.open("0:data/a.esp");
        assert_eq!(rec.rows()[0].first_open, r0.first_open);
        assert_eq!(r1.threads, 1);
        assert_eq!(r1.main_tid, current_tid());
    }

    #[test]
    fn many_threads_count_every_read_and_name_the_busiest_reader() {
        let rec = Recorder::new(Instant::now());
        let id = rec.open("0:data/skyrim.esm");
        let other = rec.open("0:data/update.esm");
        let busiest = std::thread::scope(|s| {
            let hs: Vec<_> = (0..4u64)
                .map(|k| {
                    let rec = &rec;
                    s.spawn(move || {
                        // Thread 2 reads the most.
                        let n = if k == 2 { 5_000 } else { 1_000 };
                        for _ in 0..n {
                            rec.read(id, 4);
                        }
                        current_tid()
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).nth(2).unwrap()
        });
        let rows = rec.rows();
        let r = row(&rows, "0:data/skyrim.esm");
        assert_eq!(r.reads, 8_000);
        assert_eq!(r.bytes, 32_000);
        assert_eq!(r.threads, 4);
        assert!(!r.threads_overflow);
        assert_eq!(r.main_tid, busiest);
        let u = row(&rows, "0:data/update.esm");
        assert_eq!((u.reads, u.first_read, u.main_tid), (0, None, 0));
        let _ = other;
    }

    #[test]
    fn more_reader_threads_than_slots_renders_a_plus() {
        let rec = Recorder::new(Instant::now());
        let id = rec.open("0:data/busy.bsa");
        std::thread::scope(|s| {
            for _ in 0..TID_SLOTS + 2 {
                s.spawn(|| rec.read(id, 1));
            }
        });
        let r = rec.rows()[0].clone();
        assert_eq!(
            r.reads,
            TID_SLOTS as u64 + 2,
            "overflowing threads still count reads"
        );
        assert!(r.threads_overflow);
        assert!(
            rec.render().contains(&format!("\t{TID_SLOTS}+\n")),
            "{}",
            rec.render()
        );
    }

    #[test]
    fn unknown_ids_are_ignored() {
        let rec = Recorder::new(Instant::now());
        rec.read(0, 1);
        rec.read(7, 1);
        rec.close(7);
        assert!(rec.rows().is_empty());
    }

    #[test]
    fn the_tsv_has_the_header_and_is_sorted_by_first_read() {
        let rec = Recorder::new(Instant::now());
        let never = rec.open("0:data/never-read.esp");
        let b = rec.open("0:data/b.esp");
        let a = rec.open("0:data/a.esp");
        rec.read(b, 1);
        std::thread::sleep(std::time::Duration::from_millis(2));
        rec.read(a, 1);
        let _ = never;
        let s = rec.render();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], HEADER);
        assert!(lines[1].starts_with("0:data/b.esp\t"), "{s}");
        assert!(lines[2].starts_with("0:data/a.esp\t"), "{s}");
        assert!(lines[3].starts_with("0:data/never-read.esp\t"), "{s}");
        // Never read: unset times are '-', and every row has nine columns.
        let cols: Vec<&str> = lines[3].split('\t').collect();
        assert_eq!(cols.len(), 9, "{s}");
        assert_eq!(&cols[2..5], ["-", "-", "-"], "{s}");
        assert_eq!(lines[1].split('\t').count(), 9);
        assert!(lines[4].starts_with("# snapshot at t+"), "{s}");
        assert!(!s.contains("CAP HIT"), "{s}");
    }

    #[test]
    fn a_full_table_says_rows_are_missing() {
        let rows = vec![Row {
            path: "0:data/x.esp".into(),
            first_open: Some(1),
            first_read: Some(2),
            last_read: Some(3),
            last_close: Some(4),
            reads: 5,
            bytes: 6,
            main_tid: 7,
            threads: 1,
            threads_overflow: false,
        }];
        let s = render_rows(rows, 12, 99);
        assert!(s.contains("0:data/x.esp\t1\t2\t3\t4\t5\t6\t7\t1\n"), "{s}");
        assert!(s.contains("CAP HIT") && s.contains("12 opens"), "{s}");
    }

    /// The cap is at least the 200k files a large load order can touch, and
    /// filling it does not lose the files already in it.
    #[test]
    fn the_table_holds_at_least_200k_files() {
        const { assert!(MAX_FILES >= 200_000) };
        let rec = Recorder::new(Instant::now());
        for i in 0..200_000u32 {
            assert_eq!(rec.open(&format!("0:data/f{i}.nif")), i + 1);
        }
        let id = rec.id("0:data/f199999.nif");
        rec.read(id, 9);
        assert_eq!(rec.entry(id).unwrap().bytes.load(Ordering::Relaxed), 9);
        assert_eq!(rec.dropped.load(Ordering::Relaxed), 0);
    }

    /// Cost of one recorded read, through the same `Recorder::read` the hook
    /// calls: clock, thread id and the entry's atomics. Prints ns/read; run
    /// with `--nocapture` (release for a representative figure).
    #[test]
    fn time_a_million_recorded_reads() {
        const N: u64 = 1_000_000;
        let rec = Recorder::new(Instant::now());
        let ids: Vec<u32> = (0..64)
            .map(|i| rec.open(&format!("0:data/p{i}.esp")))
            .collect();
        let t = Instant::now();
        for i in 0..N {
            rec.read(ids[(i as usize) & 63], 4096);
        }
        let one = t.elapsed();
        let t = Instant::now();
        for _ in 0..N {
            std::hint::black_box(Instant::now());
        }
        let clock = t.elapsed();
        // Four threads on one file: the contended case (the engine's loader
        // threads reading the same plugin).
        let t = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..N / 4 {
                        rec.read(ids[0], 4096);
                    }
                });
            }
        });
        let four = t.elapsed();
        let ns = |d: std::time::Duration| d.as_nanos() as f64 / N as f64;
        println!(
            "access_log: {:.1} ns/read single-thread ({:.1} ns of it Instant::now), \
             {:.1} ns/read wall across 4 threads on one file",
            ns(one),
            ns(clock),
            ns(four)
        );
        let total: u64 = rec.rows().iter().map(|r| r.reads).sum();
        assert_eq!(total, 2 * N);
    }
}
