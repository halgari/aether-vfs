//! Locating, freshness-checking and building the Windows artefacts the launch
//! tests inject (shim DLL, payload DLL, fixture executables).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub use vfs_testkit::artifacts::profile_dir;

/// [`vfs_testkit::artifacts::locate`], panicking with this harness's build hint.
pub fn locate_artifact(name: &str) -> PathBuf {
    vfs_testkit::artifacts::locate_or_panic(
        name,
        "build -p vfs-fixture-read -p vfs-shim-dll and vfs-payload (--manifest-path crates/vfs-payload/Cargo.toml) first",
    )
}

/// Every crate directory reachable from `crate_dir` by following `path =
/// "..."` dependencies — normal, dev, build, and per-target — transitively,
/// including `crate_dir` itself. Parsed straight from each crate's
/// `Cargo.toml` rather than hand-maintained: a fixed list of "the crates that
/// feed vfs-shim-dll" silently stops covering the graph the moment a
/// dependency is added or changed, which is exactly how this function's
/// caller earned its history of testing against a stale DLL.
///
/// Some of these crates depend on each other in both directions across the
/// normal/dev split (`vfs-shim` depends on `vfs-inject`; `vfs-inject`
/// dev-depends on `vfs-shim`), so this canonicalizes each directory before
/// checking whether it has already been queued — without that, a `path =
/// "../x"` hop back into an already-visited crate would never match its
/// earlier, differently-`..`-laden spelling, and the walk would not
/// terminate.
fn transitive_crate_dirs(crate_dir: &Path) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(crate_dir.to_path_buf());
    while let Some(raw_dir) = queue.pop_front() {
        let dir = raw_dir.canonicalize().unwrap_or(raw_dir);
        if seen.contains(&dir) {
            continue;
        }
        seen.push(dir.clone());
        let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        let Ok(manifest) = text.parse::<toml::Value>() else {
            continue;
        };
        let mut dep_tables: Vec<&toml::Value> = Vec::new();
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(t) = manifest.get(key) {
                dep_tables.push(t);
            }
        }
        if let Some(targets) = manifest.get("target").and_then(|t| t.as_table()) {
            for platform in targets.values() {
                for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    if let Some(t) = platform.get(key) {
                        dep_tables.push(t);
                    }
                }
            }
        }
        for table in dep_tables {
            let Some(table) = table.as_table() else {
                continue;
            };
            for spec in table.values() {
                if let Some(rel) = spec.get("path").and_then(|p| p.as_str()) {
                    queue.push_back(dir.join(rel));
                }
            }
        }
    }
    seen
}

/// Latest modification time of any file under `dir`, recursively, skipping
/// `target` and `.git`. `None` if `dir` has no files (or does not exist).
fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if matches!(entry.file_name().to_str(), Some("target") | Some(".git")) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                newest = Some(newest.map_or(mtime, |n| n.max(mtime)));
            }
        }
    }
    newest
}

/// Whether `artifact` is missing, or older than any file feeding the crate at
/// `crate_dir` through its transitive local dependency graph.
///
/// This is the check the old `ensure_inject_artifacts` skipped: it rebuilt
/// only when an artifact file did not exist, so `cargo test -p vfs-embed`
/// would silently validate a change to `vfs-redirect` or `vfs-shim` against
/// whatever DLL a previous, unrelated build had left behind — no error, just
/// a passing test that measured the wrong binary. A needless rebuild costs
/// seconds; a stale one costs a false pass, so staleness (not mere absence)
/// is the bar, and every direction of that comparison is biased toward
/// rebuilding: `artifact_is_stale` treats an unreadable artifact as stale
/// (not "assume fresh"), and treats an unreadable dependency directory as
/// contributing no mtime (so it can never *suppress* a rebuild it should not).
fn artifact_is_stale(artifact: &Path, crate_dir: &Path) -> bool {
    let Ok(artifact_mtime) = std::fs::metadata(artifact).and_then(|m| m.modified()) else {
        return true;
    };
    transitive_crate_dirs(crate_dir)
        .iter()
        .filter_map(|dir| newest_mtime(dir))
        .any(|source_mtime| source_mtime > artifact_mtime)
}

/// Name the processes holding the shim DLL, for the one build failure that is
/// never about the code.
///
/// `cargo build` cannot replace `vfs_shim_dll.dll` while any process has it
/// mapped, and it reports only `Access is denied. (os error 5)` — which reads
/// like a permissions problem and sends you looking in the wrong place. Two
/// things routinely hold it: a leftover game launched by this project's own
/// injector, and orphaned fixtures from a previously killed test.
///
/// The orphan case is self-reinforcing and worth naming loudly: a wedged fixture
/// outlives its test, keeps the DLL mapped, and every later build on that
/// machine fails until someone notices. Measured 2026-09-02: one wedge left
/// three orphans, and with a stale `SkyrimSE` also holding it, 19 tests failed
/// across two runs for a reason none of them had anything to do with.
fn lock_holders_hint() -> String {
    let dll = profile_dir().join("vfs_shim_dll.dll");
    // Only bother if the DLL is genuinely unwritable; a build can fail for
    // ordinary reasons too and this hint would then be noise.
    if std::fs::OpenOptions::new().write(true).open(&dll).is_ok() {
        return String::new();
    }
    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            "Get-Process | ForEach-Object { $p=$_; try { if ($p.Modules |              Where-Object { $_.ModuleName -eq 'vfs_shim_dll.dll' }) {              \"$($p.ProcessName) (pid $($p.Id))\" } } catch {} }",
        ])
        .output();
    let holders = match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Err(_) => String::new(),
    };
    if holders.is_empty() {
        format!(
            "

NOTE: {} is not writable, so cargo could not replace it. This is not a              code failure. Could not enumerate the holders.",
            dll.display()
        )
    } else {
        format!(
            "

NOTE: this is NOT a code failure — cargo could not replace {} because these              processes have it mapped: {holders}. Close them (an orphaned fixture from a killed              test, or a game left running by the injector) and re-run.",
            dll.display()
        )
    }
}

/// Move a build artifact aside when a live process has it mapped.
///
/// Windows refuses to *delete or overwrite* a mapped file, which is what makes
/// `cargo build` fail with `Access is denied. (os error 5)` — but it permits
/// **renaming** one. Moving the old file out of the way frees the path so cargo
/// can write a fresh copy, while the process holding the old bytes keeps them.
///
/// This exists because the alternative is a wedged machine. A hung fixture
/// outlives its test and keeps `vfs_shim_dll.dll` mapped; if that fixture is
/// stuck in a non-alertable kernel wait it cannot even be terminated, so every
/// later build on that machine fails until someone reboots. Measured 2026-09-02:
/// three such fixtures survived repeated `Stop-Process` and `taskkill /F`, and
/// with them holding the DLL, 19 tests failed across two runs for a reason none
/// of them had anything to do with. Renaming recovered it without a reboot.
///
/// Best-effort by design: if the rename fails too, the build is attempted
/// anyway and [`lock_holders_hint`] explains what happened.
fn move_locked_artifacts_aside(names: &[&str]) {
    for name in names {
        let path = profile_dir().join(name);
        if !path.exists() {
            continue;
        }
        // Only disturb a file we cannot write; renaming a healthy artifact would
        // churn the build directory for nothing.
        if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
            continue;
        }
        let aside = path.with_extension(format!(
            "orphanlocked-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        match std::fs::rename(&path, &aside) {
            Ok(()) => eprintln!(
                "note: {} was mapped by a live process; moved aside to {} so cargo can                  replace it (see move_locked_artifacts_aside)",
                path.display(),
                aside.display()
            ),
            Err(e) => eprintln!(
                "note: {} is mapped and could not be moved aside ({e}); the build below                  will probably fail",
                path.display()
            ),
        }
    }
}

pub fn ensure_inject_artifacts() {
    // Session::launch locates the shim near the current exe (the test
    // binary). Co-locate them into the profile dir if cargo left them only in
    // deps/ or they were never built for this package.
    let profile = profile_dir();
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let needed = [
        "vfs_shim_dll.dll",
        "vfs-fixture-read.exe",
        "vfs-fixture-writepath.exe",
        "vfs-fixture-escape.exe",
        "vfs-fixture-prefs.exe",
    ];
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    // They build together as part of this workspace. Each is checked for staleness
    // against its own crate's transitive source, not merely for presence.
    let main_artifact_crates: [(&str, &str); 5] = [
        ("vfs_shim_dll.dll", "vfs-shim-dll"),
        ("vfs-fixture-read.exe", "vfs-fixture-read"),
        ("vfs-fixture-writepath.exe", "vfs-fixture-writepath"),
        ("vfs-fixture-escape.exe", "vfs-fixture-escape"),
        ("vfs-fixture-prefs.exe", "vfs-fixture-prefs"),
    ];
    // Free any artifact path a dead-but-unkillable process still holds, or the
    // build below fails with a bare `Access is denied`.
    move_locked_artifacts_aside(&needed);

    let main_stale = main_artifact_crates.iter().any(|(artifact, crate_name)| {
        artifact_is_stale(
            &profile.join(artifact),
            &workspace.join("crates").join(crate_name),
        )
    });
    if main_stale {
        let status = std::process::Command::new(&cargo)
            .current_dir(&workspace)
            .args([
                "build",
                "-p",
                "vfs-shim-dll",
                "-p",
                "vfs-fixture-read",
                "-p",
                "vfs-fixture-writepath",
                "-p",
                "vfs-fixture-escape",
                "-p",
                "vfs-fixture-prefs",
                "--quiet",
            ])
            .status()
            .expect("spawn cargo");
        assert!(
            status.success(),
            "fixture/artifact build failed: {status}{}",
            lock_holders_hint()
        );
    }

    // Copy into profile root so Session::launch's find_near works from the
    // test exe. Always overwrite: a rebuilt artifact must replace whatever
    // was co-located here before, not sit next to a stale copy that a
    // skip-if-exists check would otherwise leave in place untouched.
    for name in needed {
        let dest = profile.join(name);
        let src = locate_artifact(name);
        let same_file = match (src.canonicalize(), dest.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if same_file {
            continue;
        }
        let _ = std::fs::copy(&src, &dest);
    }
}
