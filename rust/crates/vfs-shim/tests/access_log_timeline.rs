//! `VFS_SHIM_ACCESS_LOG`, end to end: a file read through the hooked
//! `NtReadFile` and closed shows up in the TSV with its reads, bytes and
//! times.
//!
//! Its own binary: the timeline is switched on for the whole process at
//! `install`, which installs the process-global detours and `FuseClient`.

mod fakedirector;

use std::io::Read;

use fakedirector::{Fake, ReadStyle};
use vfs_shim::{install, Engine};

#[test]
fn a_file_read_through_the_hooks_appears_in_the_timeline() {
    let base = std::env::temp_dir().join(format!("vfs-access-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let overlay = base.join("overlay");
    let log = base.join("access.tsv");
    std::fs::create_dir_all(root.join("Data")).unwrap();
    std::fs::create_dir_all(&overlay).unwrap();

    std::env::set_var(vfs_env::SHIM_ACCESS_LOG, &log);
    std::env::set_var(vfs_env::SHIM_STATS_INTERVAL_MS, "10");

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "unrelated.txt".into(),
                kind: EntryKind::File,
                source: r"D:\nowhere\unrelated.txt".into(),
                size: 0,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let content = fakedirector::pattern(10_000);
    fakedirector::install(
        &root,
        Fake::new().with("data/plugin.esp", content.clone(), ReadStyle::Whole),
        0,
    );
    let hooks = install(
        Engine::with_overlay(root.to_str().unwrap(), overlay.to_str().unwrap(), snapshot).unwrap(),
    )
    .expect("install");

    // Ten 1,000-byte reads, then one at end of file.
    let mut f = std::fs::File::open(root.join("Data").join("Plugin.esp")).expect("open");
    let mut got = Vec::new();
    let mut buf = [0u8; 1000];
    loop {
        let n = f.read(&mut buf).expect("read");
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got, content);
    drop(f);
    drop(hooks);

    let mut body = String::new();
    let mut row = None;
    for _ in 0..500 {
        body = std::fs::read_to_string(&log).unwrap_or_default();
        row = body
            .lines()
            .find(|l| l.starts_with("0:data/plugin.esp\t"))
            .map(|l| l.split('\t').map(str::to_string).collect::<Vec<_>>())
            .filter(|c| c[4] != "-");
        if row.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        body.starts_with(
            "path\tfirst_open_us\tfirst_read_us\tlast_read_us\tlast_close_us\treads\tbytes\tmain_tid\tthreads\n"
        ),
        "{body}"
    );
    let c = row.unwrap_or_else(|| panic!("no closed row for data/plugin.esp:\n{body}"));
    let us = |i: usize| {
        c[i].parse::<u64>()
            .unwrap_or_else(|_| panic!("column {i}: {c:?}"))
    };
    assert!(us(1) <= us(2) && us(2) <= us(3) && us(3) <= us(4), "{c:?}");
    // Every NtReadFile the handle served, the end-of-file one included.
    assert_eq!(us(5), 11, "{c:?}");
    assert_eq!(us(6), 10_000, "{c:?}");
    assert_ne!(c[7], "-", "{c:?}");
    assert_eq!(c[8], "1", "{c:?}");

    let _ = std::fs::remove_dir_all(&base);
}
