//! The `vfs` CLI end to end under GE-Proton: `vfs up` brings a named session
//! up, `vfs exec` launches a Windows fixture into it, and the fixture reads
//! content only the Linux Director serves and writes a save into a write
//! layer. Then a second `vfs exec`, by the absolute path, launches into the
//! same live session — its roots, links and prefix reused, not rebuilt. (The
//! first exec staged `fixture.exe` into root 0, so the second finds a real file
//! there and launches it as is: it does not exercise staging again.)
//!
//! Needs GE-Proton under `$VFS_HOME` and the `bin/build-windows` artifacts
//! beside the `vfs` binary, so it is `#[ignore]`d; the `proton-linux` CI job
//! runs it.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(300);

/// Run `cmd` with output captured to files, bounded by [`WAIT`]. On timeout
/// the child is killed and the test fails with what it printed.
fn run(label: &str, scratch: &Path, mut cmd: Command) -> std::process::Output {
    let out_path = scratch.join(format!("{label}.stdout"));
    let err_path = scratch.join(format!("{label}.stderr"));
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&out_path).unwrap())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {label}: {e}"));
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break Some(s);
        }
        if start.elapsed() > WAIT {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stdout = std::fs::read(&out_path).unwrap_or_default();
    let stderr = std::fs::read(&err_path).unwrap_or_default();
    println!(
        "--- {label} stdout ---\n{}\n--- {label} stderr ---\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    match status {
        Some(status) => std::process::Output {
            status,
            stdout,
            stderr,
        },
        None => panic!("{label} did not finish within {WAIT:?} and was killed"),
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// A one-entry Stored zip.
fn write_stored_zip(path: &Path, entry: &str, content: &[u8]) {
    let mut buf = Vec::new();
    let crc = crc32(content);
    let n = entry.len() as u16;
    let len = content.len() as u32;
    buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(entry.as_bytes());
    buf.extend_from_slice(content);
    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 6]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&[0u8; 8]);
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(entry.as_bytes());
    let cd_size = buf.len() as u32 - cd_start;
    buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&cd_size.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    std::fs::write(path, &buf).unwrap();
}

/// Tears the session down even when the test panics: `vfs down` against the
/// daemon the discovery file names (never an auto-spawned replacement), a TERM
/// to every daemon this run knows of, a bounded wait for them to exit, a stop
/// of the named prefix's `wineserver`, and only then removal of this run's own
/// session directory (never any other).
struct Cleanup {
    vfs: &'static str,
    vfs_home: PathBuf,
    discovery: PathBuf,
    name: String,
    daemon_pid: Option<u32>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        // `--endpoint`, so a dead daemon is not silently replaced by a fresh
        // auto-spawned one whose pid nothing here would know. No discovery
        // file: no daemon to talk to.
        if let Some(endpoint) = read_discovery_field(&self.discovery, "endpoint")
            .and_then(|v| v.as_str().map(str::to_string))
        {
            let mut down = Command::new(self.vfs);
            down.args(["--endpoint", &endpoint, "down", "--session", &self.name])
                .env("VFS_HOME", &self.vfs_home)
                .env("VFS_DISCOVERY_PATH", &self.discovery)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Ok(child) = down.spawn() {
                wait_bounded(child, Duration::from_secs(60));
            }
        }
        // The pid recorded at `up`, and whatever the discovery file names now,
        // in case they differ.
        let mut pids: Vec<u32> = self.daemon_pid.into_iter().collect();
        pids.extend(read_daemon_pid(&self.discovery));
        pids.sort_unstable();
        pids.dedup();
        for pid in &pids {
            let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        while pids.iter().any(|p| Path::new(&format!("/proc/{p}")).exists())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        // Only ever this run's own directory, by its unique name.
        if self.name.starts_with("e2e-") {
            let session_dir = self.vfs_home.join("sessions").join(&self.name);
            // A named prefix outlives its session by design, and its
            // `wineserver` lingers a few seconds after the last Wine process,
            // writing the registry back into the prefix as it exits. Stop it
            // first, or it recreates what is removed below.
            stop_wineservers(&self.vfs_home, &session_dir.join("prefix"));
            let _ = std::fs::remove_dir_all(&session_dir);
        }
    }
}

/// `wineserver -k` then `-w` for `prefix`, with every installed runtime's
/// `wineserver`, each bounded.
fn stop_wineservers(vfs_home: &Path, prefix: &Path) {
    if !prefix.exists() {
        return;
    }
    let Ok(runtimes) = std::fs::read_dir(vfs_home.join("runtimes")) else {
        return;
    };
    for rt in runtimes.flatten() {
        let server = rt.path().join("files").join("bin").join("wineserver");
        if !server.is_file() {
            continue;
        }
        for flag in ["-k", "-w"] {
            if let Ok(child) = Command::new(&server)
                .arg(flag)
                .env("WINEPREFIX", prefix)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                wait_bounded(child, Duration::from_secs(30));
            }
        }
    }
}

/// Waits for `child` at most `timeout`, then kills and reaps it.
fn wait_bounded(mut child: std::process::Child, timeout: Duration) {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn read_discovery_field(discovery: &Path, field: &str) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(discovery).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get(field).cloned()
}

fn read_daemon_pid(discovery: &Path) -> Option<u32> {
    read_discovery_field(discovery, "pid")?.as_u64().map(|p| p as u32)
}

#[test]
#[ignore = "needs GE-Proton under $VFS_HOME and bin/build-windows artifacts beside the vfs binary"]
fn vfs_up_then_exec_runs_a_windows_fixture_under_proton() {
    let vfs: &'static str = env!("CARGO_BIN_EXE_vfs");
    let vfs_home = PathBuf::from(std::env::var("VFS_HOME").expect("set VFS_HOME to the GE-Proton home"));

    let t = tempfile::tempdir().unwrap();
    let t = t.path();
    let bin_dir = Path::new(vfs).parent().unwrap();
    let fixture_src = bin_dir.join("vfs-fixture-read.exe");
    assert!(
        fixture_src.is_file(),
        "{} is missing; run bin/build-windows first",
        fixture_src.display()
    );

    std::fs::create_dir_all(t.join("game")).unwrap();
    std::fs::copy(&fixture_src, t.join("game").join("fixture.exe")).unwrap();
    write_stored_zip(&t.join("data.zip"), "data/hello.txt", &[b'A'; 4096]);
    std::fs::create_dir_all(t.join("saves-layer")).unwrap();

    let name = format!("e2e-{}", std::process::id());
    // This run's own name. A directory already there is a previous run's
    // leftover under a recycled pid — a stale persistent prefix this run
    // would otherwise reuse (and whose root links it would find foreign).
    let _ = std::fs::remove_dir_all(vfs_home.join("sessions").join(&name));
    let discovery = t.join("discovery.json");
    let config = format!(
        r#"[session]
name = "{name}"

[[root]]
id = 0
name = "Games"
path = 'C:\Games\Fixture'

[[root]]
id = 1
name = "Saves"
path = 'C:\users\steamuser\vfs-e2e-save'

[[source]]
type = "disk"
path = "{t}/game"
root = 0

[[source]]
type = "zip"
path = "{t}/data.zip"
root = 0

[[source]]
type = "disk"
path = "{t}/saves-layer"
root = 1
write_layer = true
"#,
        t = t.display()
    );
    let config_path = t.join("session.toml");
    std::fs::write(&config_path, config).unwrap();

    let mut guard = Cleanup {
        vfs,
        vfs_home: vfs_home.clone(),
        discovery: discovery.clone(),
        name: name.clone(),
        daemon_pid: None,
    };

    let vfs_cmd = |args: &[&str]| {
        let mut c = Command::new(vfs);
        c.args(args)
            .env("VFS_HOME", &vfs_home)
            .env("VFS_DISCOVERY_PATH", &discovery);
        c
    };

    let out = run(
        "up",
        t,
        vfs_cmd(&["up", "--config", config_path.to_str().unwrap()]),
    );
    guard.daemon_pid = read_daemon_pid(&discovery);
    assert!(out.status.success(), "vfs up failed: {:?}", out.status);

    let read_env = [
        "VFS_FIXTURE_PATH=C:\\Games\\Fixture\\data\\hello.txt",
        "VFS_FIXTURE_EXPECT=4096",
        "VFS_FIXTURE_FILL=65",
    ];
    let mut args: Vec<&str> = vec!["exec", "--session", &name, "{Games}\\fixture.exe"];
    for e in read_env {
        args.extend(["--env", e]);
    }
    for e in [
        "VFS_FIXTURE_WRITE_PATH=C:\\users\\steamuser\\vfs-e2e-save\\save.txt",
        "VFS_FIXTURE_WRITE_DATA=saved",
    ] {
        args.extend(["--env", e]);
    }
    let out = run("exec1", t, vfs_cmd(&args));
    assert!(out.status.success(), "first vfs exec failed: {:?}", out.status);

    assert_eq!(
        std::fs::read(t.join("saves-layer").join("save.txt")).expect("save.txt in the write layer"),
        b"saved"
    );

    let prefix = vfs_home.join("sessions").join(&name).join("prefix");
    for rel in ["drive_c/users/steamuser/vfs-e2e-save", "drive_c/Games/Fixture"] {
        let p = prefix.join(rel);
        let md = std::fs::symlink_metadata(&p)
            .unwrap_or_else(|e| panic!("{} missing: {e}", p.display()));
        assert!(
            md.file_type().is_symlink(),
            "{} should be a symlink (the prefix holds links, not content)",
            p.display()
        );
    }

    // The absolute form into the same live session. The first exec's staged
    // `fixture.exe` is still in root 0, so this takes the real-file branch —
    // what it proves is that the session's mappings and prefix are reused.
    let mut args: Vec<&str> = vec!["exec", "--session", &name, "C:\\Games\\Fixture\\fixture.exe"];
    for e in read_env {
        args.extend(["--env", e]);
    }
    let out = run("exec2", t, vfs_cmd(&args));
    assert!(out.status.success(), "second vfs exec failed: {:?}", out.status);

    drop(guard);
    assert!(
        !vfs_home.join("sessions").join(&name).exists(),
        "cleanup left the session directory behind"
    );
}
