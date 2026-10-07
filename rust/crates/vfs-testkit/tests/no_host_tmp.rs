//! No host-side test writes to the host's temp dir.
//!
//! On the owner's machine `/tmp` is a RAM tmpfs, and nothing may land there.
//! Tests take their scratch space from `vfs_testkit` (`scratch_dir`,
//! `scratch_path`, `tempdir`), which lives under `target/`. This is a source
//! scan, in the manner of vfs-env's `no_crate_reads_a_switch_that_is_not_registered`:
//! it walks every crate's test code and fails, naming file and line, on
//!
//! - `temp_dir()` (that is, `std::env::temp_dir()`),
//! - `tempdir()` without a `testkit::` prefix, and `TempDir::new(`,
//! - a string literal that starts with `/tmp`.
//!
//! "Test code" is everything under a `tests/`, `benches/` or `examples/`
//! directory, any file named `tests.rs`, `test_*.rs` or `*_tests.rs`, and the
//! part of any other `src/` file after its first `#[cfg(test)] mod`. Comment lines
//! are not scanned.

use std::path::{Path, PathBuf};

/// Crates that are Windows-only or run under Wine, where `temp_dir()` is the
/// Wine prefix's Windows temp and not the host's `/tmp`. Each: (crate, reason).
const EXEMPT_CRATES: &[(&str, &str)] = &[
    (
        "vfs-shim",
        "Windows DLL tests; run under Wine, where temp_dir() is the prefix's temp",
    ),
    ("vfs-shim-dll", "Windows-only shim DLL"),
    ("vfs-inject", "Windows injector tests; run under Wine"),
    ("vfs-payload", "Windows-only injected payload"),
    (
        "vfs-redirect",
        "Windows-only path redirection; tests use Win32 and run on Windows",
    ),
    (
        "vfs-win",
        "Windows-only crate (Win32 file mappings and volumes)",
    ),
    (
        "vfs-fixture-escape",
        "Windows fixture executable, runs under Wine",
    ),
    (
        "vfs-fixture-nvapi",
        "Windows fixture executable, runs under Wine",
    ),
    (
        "vfs-fixture-prefs",
        "Windows fixture executable, runs under Wine",
    ),
    (
        "vfs-fixture-read",
        "Windows fixture executable, runs under Wine",
    ),
    (
        "vfs-fixture-registry",
        "Windows fixture executable, runs under Wine",
    ),
    (
        "vfs-fixture-staticimp",
        "Windows fixture executable, runs under Wine",
    ),
    (
        "vfs-fixture-steam",
        "Windows fixture executable, runs under Wine",
    ),
    (
        "vfs-fixture-vproxy",
        "Windows fixture executable, runs under Wine",
    ),
    (
        "vfs-fixture-writepath",
        "Windows fixture executable, runs under Wine",
    ),
];

/// Real exceptions: (path relative to `crates/`, a distinctive substring of the
/// line, reason). A line is allowed when both match.
const ALLOW: &[(&str, &str, &str)] = &[];

#[derive(Clone, Copy)]
enum Scope {
    /// Every line is test code.
    Whole,
    /// Test code starts at the first `#[cfg(test)] mod`.
    AfterCfgTest,
}

fn scope_of(rel_in_crate: &Path) -> Scope {
    let comps: Vec<String> = rel_in_crate
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let file = comps.last().cloned().unwrap_or_default();
    let in_dir = comps[..comps.len() - 1]
        .iter()
        .any(|c| matches!(c.as_str(), "tests" | "benches" | "examples"));
    let stem = file.trim_end_matches(".rs");
    if in_dir || stem == "tests" || stem.starts_with("test_") || stem.ends_with("_tests") {
        Scope::Whole
    } else {
        Scope::AfterCfgTest
    }
}

/// The offending patterns on one (non-comment) line.
fn offences(line: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    if line.contains("temp_dir()") {
        out.push("temp_dir()");
    }
    let mut from = 0;
    while let Some(i) = line[from..].find("tempdir()") {
        let at = from + i;
        if !line[..at].ends_with("testkit::") {
            out.push("tempdir() outside vfs_testkit");
            break;
        }
        from = at + 1;
    }
    if line.contains("TempDir::new(") {
        out.push("TempDir::new(");
    }
    if line.contains("\"/tmp") || line.contains("r\"/tmp") {
        out.push("\"/tmp literal");
    }
    out
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Whether the next item after some attributes is a `mod`.
fn starts_a_module(rest: &[&str]) -> bool {
    rest.iter()
        .map(|l| l.trim_start())
        .find(|l| !l.starts_with("#[") && !l.starts_with("//"))
        .is_some_and(|l| {
            l.starts_with("mod ") || l.starts_with("pub mod ") || l.starts_with("pub(crate) mod ")
        })
}

/// Scan one file's text; `(line number, pattern, line)` for each offence.
fn scan(scope: Scope, text: &str) -> Vec<(usize, &'static str, String)> {
    let mut out = Vec::new();
    let mut in_test = matches!(scope, Scope::Whole);
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        // A `#[cfg(test)]` on a lone item is not the start of the test module.
        if !in_test && t.starts_with("#[cfg(test)]") && starts_a_module(&lines[i + 1..]) {
            in_test = true;
        }
        if !in_test || t.starts_with("//") {
            continue;
        }
        for pat in offences(line) {
            out.push((i + 1, pat, line.trim().to_string()));
        }
    }
    out
}

#[test]
fn no_host_side_test_uses_the_host_temp_dir() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir");
    let mut files = Vec::new();
    rust_files(crates, &mut files);
    let mut bad: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    let mut used_allow = vec![false; ALLOW.len()];
    for path in &files {
        let rel = path.strip_prefix(crates).unwrap();
        let mut comps = rel.components();
        let krate = comps
            .next()
            .unwrap()
            .as_os_str()
            .to_string_lossy()
            .into_owned();
        if EXEMPT_CRATES.iter().any(|(c, _)| *c == krate) {
            continue;
        }
        let in_crate: PathBuf = comps.collect();
        // This file spells the patterns out.
        if krate == "vfs-testkit" && in_crate.ends_with("no_host_tmp.rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        scanned += 1;
        for (line_no, pat, line) in scan(scope_of(&in_crate), &text) {
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            let allowed = ALLOW
                .iter()
                .enumerate()
                .find(|(_, (f, sub, _))| *f == rel_s && line.contains(sub));
            if let Some((idx, _)) = allowed {
                used_allow[idx] = true;
                continue;
            }
            bad.push(format!("{}:{line_no}: {pat}: {line}", path.display()));
        }
    }
    assert!(
        scanned > 100,
        "scanned only {scanned} files; did the walk break?"
    );
    assert!(
        bad.is_empty(),
        "host-side tests must not use the host's temp dir (a RAM tmpfs here). Use \
         vfs_testkit::{{scratch_dir, scratch_path, tempdir}}, or add a justified entry to ALLOW:\n  {}",
        bad.join("\n  ")
    );
    for (used, (f, sub, _)) in used_allow.iter().zip(ALLOW) {
        assert!(
            *used,
            "stale ALLOW entry ({f}, {sub}): nothing matches it any more"
        );
    }
}

#[test]
fn the_scan_recognises_what_it_forbids() {
    let hit = |s: &str| !scan(Scope::Whole, s).is_empty();
    assert!(hit("let d = std::env::temp_dir();"));
    assert!(hit("let d = tempfile::tempdir().unwrap();"));
    assert!(hit("let d = tempfile::TempDir::new().unwrap();"));
    assert!(hit("let p = Path::new(\"/tmp/x\");"));
    assert!(!hit("let d = vfs_testkit::tempdir().unwrap();"));
    assert!(!hit("let d = tempfile::tempdir_in(root).unwrap();"));
    assert!(!hit("// std::env::temp_dir()"));
    assert!(scan(Scope::AfterCfgTest, "fn f() { std::env::temp_dir(); }\n").is_empty());
    assert!(!scan(
        Scope::AfterCfgTest,
        "fn f() {}\n#[cfg(test)]\nmod t { fn g() { std::env::temp_dir(); } }\n"
    )
    .is_empty());
}
