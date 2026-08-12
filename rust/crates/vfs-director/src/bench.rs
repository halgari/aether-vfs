//! Load benchmark for a real game launch.
//!
//! `vfs-fuse-bench` measures synthetic ring round-trips; this measures the thing
//! a player feels — wall clock from process start to a window on screen, and how
//! much VFS traffic it took to get there.
//!
//! The end condition is **window visible with a non-zero client rect**. It is
//! objective, cheap to detect, and captures every cost on the path (staging,
//! injection, hollow, content streaming) rather than one layer of it.
//!
//! Emits a markdown row so runs land in `docs/benchmarks/` and stay comparable
//! across builds — the immediate use being debug vs release.

// Window/process enumeration is Win32; the crate is otherwise unsafe-free.
#![allow(unsafe_code)]

use std::time::{Duration, Instant};

/// One named point on the launch timeline.
#[derive(Debug, Clone)]
pub struct Phase {
    pub name: String,
    pub at: Duration,
}

/// Records when each stage of a launch completed, relative to a start instant.
#[derive(Debug)]
pub struct Timeline {
    start: Instant,
    phases: Vec<Phase>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Timeline {
    pub fn new() -> Self {
        Timeline {
            start: Instant::now(),
            phases: Vec::new(),
        }
    }

    /// Mark `name` as reached now.
    pub fn mark(&mut self, name: &str) {
        let at = self.start.elapsed();
        self.phases.push(Phase {
            name: name.to_string(),
            at,
        });
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    pub fn phases(&self) -> &[Phase] {
        &self.phases
    }

    /// Time from the previous mark to each mark, so a slow stage stands out
    /// rather than being hidden in a cumulative total.
    pub fn deltas(&self) -> Vec<(String, Duration, Duration)> {
        let mut out = Vec::with_capacity(self.phases.len());
        let mut prev = Duration::ZERO;
        for p in &self.phases {
            out.push((p.name.clone(), p.at.saturating_sub(prev), p.at));
            prev = p.at;
        }
        out
    }

    /// Cumulative time of the named phase, if it was reached.
    pub fn at(&self, name: &str) -> Option<Duration> {
        self.phases.iter().find(|p| p.name == name).map(|p| p.at)
    }
}

/// Human-readable timeline plus counters.
pub fn report(tl: &Timeline, totals: &crate::io_stats::Totals, label: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("\n=== load benchmark: {label} ===\n"));
    s.push_str("  phase                        delta      cumulative\n");
    for (name, delta, cum) in tl.deltas() {
        s.push_str(&format!(
            "  {:<26} {:>7.2}s   {:>8.2}s\n",
            name,
            delta.as_secs_f64(),
            cum.as_secs_f64()
        ));
    }
    s.push_str(&format!(
        "\n  VFS: {} reads, {:.1} MiB, {:.1} KiB/read, {:.0} reads/MiB\n",
        totals.reads,
        totals.bytes as f64 / (1024.0 * 1024.0),
        totals.bytes_per_read() / 1024.0,
        totals.reads_per_mib()
    ));
    s.push_str(&format!(
        "  ops: getattr={} readdir={} open={} close={} err={} paths={}\n",
        totals.getattrs,
        totals.readdirs,
        totals.opens,
        totals.closes,
        totals.errors,
        totals.paths
    ));
    s
}

/// One markdown table row, for appending to a benchmark doc.
pub fn markdown_row(tl: &Timeline, totals: &crate::io_stats::Totals, label: &str) -> String {
    let secs = |n: &str| {
        tl.at(n)
            .map(|d| format!("{:.2}", d.as_secs_f64()))
            .unwrap_or_else(|| "—".into())
    };
    format!(
        "| {} | {} | {} | {} | {} | {} | {:.1} | {:.1} | {:.0} |",
        label,
        secs("zip index"),
        secs("staged"),
        secs("serving"),
        secs("launched"),
        secs("window visible"),
        totals.bytes as f64 / (1024.0 * 1024.0),
        totals.bytes_per_read() / 1024.0,
        totals.reads_per_mib()
    )
}

/// Header matching [`markdown_row`].
pub fn markdown_header() -> &'static str {
    "| run | zip idx | staged | serving | launched | **window** | MiB | KiB/read | reads/MiB |\n\
     |-----|--------:|-------:|--------:|---------:|-----------:|----:|---------:|----------:|"
}

/// Wait until `pid` shows a visible window with a non-zero client area.
///
/// Returns its size, or `None` on timeout. A game can hold an invisible or
/// zero-sized window well before it renders, so both conditions matter —
/// an error dialog also has a client rect, hence the caller should sanity-check
/// the dimensions against what it asked for.
#[cfg(windows)]
pub fn wait_for_window(pid: u32, timeout: Duration) -> Option<(u32, u32)> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(sz) = find_window(pid) {
            return Some(sz);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

#[cfg(not(windows))]
pub fn wait_for_window(_pid: u32, _timeout: Duration) -> Option<(u32, u32)> {
    None
}

#[cfg(windows)]
fn find_window(pid: u32) -> Option<(u32, u32)> {
    use std::cell::RefCell;
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowThreadProcessId, IsWindowVisible,
    };

    thread_local! {
        static FOUND: RefCell<Option<(u32, u32)>> = const { RefCell::new(None) };
        static WANT_PID: RefCell<u32> = const { RefCell::new(0) };
    }

    unsafe extern "system" fn cb(hwnd: HWND, _l: LPARAM) -> BOOL {
        let mut wp: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut wp);
        let want = WANT_PID.with(|w| *w.borrow());
        if wp != want || IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let mut r: RECT = core::mem::zeroed();
        if GetClientRect(hwnd, &mut r) == 0 {
            return 1;
        }
        let (w, h) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
        if w == 0 || h == 0 {
            return 1;
        }
        FOUND.with(|f| *f.borrow_mut() = Some((w, h)));
        0 // stop enumerating
    }

    WANT_PID.with(|w| *w.borrow_mut() = pid);
    FOUND.with(|f| *f.borrow_mut() = None);
    // SAFETY: callback only reads thread-locals and Win32 window state.
    unsafe {
        EnumWindows(Some(cb), 0);
    }
    FOUND.with(|f| *f.borrow())
}

/// Find a running process by image name, returning its pid.
#[cfg(windows)]
pub fn find_pid(image_name: &str) -> Option<u32> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: standard snapshot walk; handle closed on every path.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() {
            return None;
        }
        let mut e: PROCESSENTRY32W = core::mem::zeroed();
        e.dwSize = core::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e);
        let mut found = None;
        while ok != 0 {
            let n: String = {
                let raw = &e.szExeFile;
                let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
                String::from_utf16_lossy(&raw[..len])
            };
            if n.eq_ignore_ascii_case(image_name) {
                found = Some(e.th32ProcessID);
                break;
            }
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
        found
    }
}

#[cfg(not(windows))]
pub fn find_pid(_image_name: &str) -> Option<u32> {
    None
}

/// Poll for `image_name` to appear, returning its pid.
pub fn wait_for_pid(image_name: &str, timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(p) = find_pid(image_name) {
            return Some(p);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_split_cumulative_time_into_stages() {
        let mut tl = Timeline::new();
        tl.phases.push(Phase {
            name: "a".into(),
            at: Duration::from_millis(100),
        });
        tl.phases.push(Phase {
            name: "b".into(),
            at: Duration::from_millis(250),
        });
        let d = tl.deltas();
        assert_eq!(d[0].0, "a");
        assert_eq!(d[0].1, Duration::from_millis(100));
        assert_eq!(d[1].0, "b");
        // b's delta is the gap from a, not from zero — a slow stage must stand out.
        assert_eq!(d[1].1, Duration::from_millis(150));
        assert_eq!(d[1].2, Duration::from_millis(250));
    }

    #[test]
    fn at_reports_only_reached_phases() {
        let mut tl = Timeline::new();
        tl.mark("only");
        assert!(tl.at("only").is_some());
        assert!(tl.at("never").is_none());
    }

    #[test]
    fn read_amplification_metrics() {
        let t = crate::io_stats::Totals {
            reads: 12432,
            bytes: 64 * 1024 * 1024,
            ..Default::default()
        };
        // The shaders.bsa signature: many tiny reads.
        assert!((t.bytes_per_read() / 1024.0 - 5.27).abs() < 0.1, "{}", t.bytes_per_read());
        assert!((t.reads_per_mib() - 194.25).abs() < 1.0, "{}", t.reads_per_mib());
    }

    #[test]
    fn empty_totals_do_not_divide_by_zero() {
        let t = crate::io_stats::Totals::default();
        assert_eq!(t.bytes_per_read(), 0.0);
        assert_eq!(t.reads_per_mib(), 0.0);
    }

    #[test]
    fn markdown_row_marks_unreached_phases() {
        let mut tl = Timeline::new();
        tl.phases.push(Phase {
            name: "zip index".into(),
            at: Duration::from_millis(500),
        });
        let row = markdown_row(&tl, &crate::io_stats::Totals::default(), "debug");
        assert!(row.contains("| debug |"), "{row}");
        assert!(row.contains("0.50"), "{row}");
        // A run that never reached the window must not read as instant.
        assert!(row.contains("—"), "{row}");
    }
}
