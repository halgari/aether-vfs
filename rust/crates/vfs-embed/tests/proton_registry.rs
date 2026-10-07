//! **The registry overlay, end to end** (registry overlay spec §8.3): a Windows program
//! under GE-Proton, with the shim injected, cannot tell whether its registry writes reach
//! the real registry or the session's registry layer, and with the layer on, none of them
//! reaches the real registry.
//!
//! The program is `vfs-fixture-registry.exe`, a scripted Win32 registry sequence under
//! `Software\AetherVfsRegistryTest` in HKCU and HKLM (see its module docs for what it prints
//! and what it leaves out). The test runs, all in one throwaway Wine prefix:
//!
//! 1. `cleanup` and `prepare` without a layer: a known real starting point, `Base`, which the
//!    script's copy-on-write steps write through. `probe` records it (P0).
//! 2. `run` without a layer: transcript A. `probe` records what it left in the real registry
//!    (PA). Then `cleanup` and `prepare` again, and `probe` must give P0.
//! 3. `run` with a layer (an in-memory provider): transcript B, which must equal A line for
//!    line.
//! 4. `probe` with the layer, as a second process of the same session: PA, so the overlay
//!    holds everything run A wrote to the real registry, and serves it to another process.
//! 5. Detach the layer and `probe`: P0. The real registry, `Base` included, is unchanged.
//! 6. Re-attach the same provider (in this session, then in a new one, which loads the
//!    `overlay.reg` the detach flushed into it) and `probe`: PA each time.
//!
//! Needs, and cannot provide for itself: a verified GE-Proton runtime and the Windows
//! artifacts from `bin/build-windows` for this test's profile:
//! `bin/build-windows --release`, then
//! `cargo test --release -p vfs-embed --test proton_registry -- --ignored`. Without either it
//! prints `SKIP ...` with the command to run and passes (see `tests/support/mod.rs` for the
//! policy and for how the runtime and artifacts are found).
//!
//! Nothing it creates is outside this workspace's target directory: the test gets its own
//! aether-vfs home there (`support::throwaway_home`), so the Wine prefix
//! (`sessions/registry-e2e`) is a throwaway one. The real registry it changes is that
//! prefix's, and it removes its scratch keys again at the end.
#![cfg(unix)]

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use vfs_embed::{DiskProvider, LaunchOpts, MemoryProvider, Provider, Session, VPath};

const FIXTURE: &str = vfs_proton::artifacts::FIXTURE_REGISTRY;
/// The scratch key's leaf, the same in both runs.
const RUN_ID: &str = "e2e";
const PREFIX: &str = "registry-e2e";

fn fresh(name: &str) -> PathBuf {
    support::scratch("vfs-proton-registry", name)
}

struct Rig {
    home: PathBuf,
    art: support::Artifacts,
    logs: PathBuf,
    launches: usize,
}

impl Rig {
    /// A served session over a root holding the fixture, in the test's named prefix.
    fn session(&self, tag: &str) -> Session {
        let root = fresh(&format!("{tag}-root"));
        std::fs::copy(self.art.path(FIXTURE), root.join("probe.exe")).unwrap();
        let mut s = Session::new();
        s.set_home(&self.home);
        s.set_root(&root);
        s.set_state_dir(fresh(&format!("{tag}-state")));
        s.set_overlay(fresh(&format!("{tag}-overlay")));
        s.set_prefix_name(PREFIX).unwrap();
        s.mount("", Arc::new(DiskProvider::new(&root)) as Arc<dyn Provider>)
            .unwrap();
        s.serve().unwrap();
        s
    }

    /// Launch the fixture in `mode` and return its `reg: ` lines.
    fn fixture(&mut self, s: &Session, mode: &str) -> Vec<String> {
        self.launches += 1;
        let log = self.logs.join(format!("{:02}-{mode}.log", self.launches));
        let code = s
            .launch(&LaunchOpts {
                image: "probe.exe".into(),
                args: vec![mode.to_string(), RUN_ID.to_string()],
                wait: true,
                shim_dll: Some(self.art.shim_dll()),
                payload_dll: Some(self.art.payload_dll()),
                log_file: Some(log.clone()),
                // A Wine debug channel list for the fixture's launches, when one is wanted.
                env: std::env::var("VFS_TEST_WINEDEBUG")
                    .map(|v| BTreeMap::from([("WINEDEBUG".to_string(), v)]))
                    .unwrap_or_default(),
                ..Default::default()
            })
            .unwrap_or_else(|e| panic!("launch {mode}: {e}"));
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<String> = text
            .lines()
            .filter_map(|l| l.trim_end().strip_prefix("reg: "))
            .map(str::to_string)
            .collect();
        std::fs::write(
            self.logs
                .join(format!("{:02}-{mode}.transcript", self.launches)),
            lines.join("\n") + "\n",
        )
        .unwrap();
        assert_eq!(
            code,
            0,
            "fixture {mode} exited {code}; log {}:\n{text}",
            log.display()
        );
        assert_eq!(
            lines.last().map(String::as_str),
            Some("end"),
            "fixture {mode} did not finish; log {}:\n{text}",
            log.display()
        );
        lines
    }
}

/// Fail with the first differing lines of two transcripts, if they differ.
fn assert_same(what: &str, want: &[String], got: &[String]) {
    if want == got {
        return;
    }
    let mut diff = Vec::new();
    for i in 0..want.len().max(got.len()) {
        let (a, b) = (want.get(i), got.get(i));
        if a != b {
            diff.push(format!(
                "  line {i}:\n    want {}\n    got  {}",
                a.map_or("<none>", String::as_str),
                b.map_or("<none>", String::as_str)
            ));
        }
        if diff.len() == 40 {
            break;
        }
    }
    panic!(
        "{what}: {} lines wanted, {} got; first differences:\n{}",
        want.len(),
        got.len(),
        diff.join("\n")
    );
}

fn has(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

#[test]
#[ignore = "needs a GE-Proton runtime and the Windows artifacts from bin/build-windows for this \
            profile (including vfs-fixture-registry.exe): bin/build-windows --release, then \
            cargo test --release"]
fn registry_writes_look_the_same_through_the_overlay_and_never_reach_the_real_registry() {
    let Some(support::Rig { home, art }) = support::rig("proton_registry", "registry", &[FIXTURE])
    else {
        return;
    };
    let mut rig = Rig {
        home,
        art,
        logs: fresh("logs"),
        launches: 0,
    };
    eprintln!("logs and transcripts: {}", rig.logs.display());

    // 1. A known real starting point.
    let s = rig.session("one");
    rig.fixture(&s, "cleanup");
    rig.fixture(&s, "prepare");
    let p0 = rig.fixture(&s, "probe");
    assert!(
        has(&p0, r#"EnumKey [.] "Base""#),
        "prepare made Base:\n{p0:#?}"
    );
    assert!(!has(&p0, RUN_ID), "no scratch key yet:\n{p0:#?}");

    // 2. Run A, on the real registry.
    let a = rig.fixture(&s, "run");
    let pa = rig.fixture(&s, "probe");
    assert!(
        has(&pa, &format!(r#"EnumKey [.] "{RUN_ID}""#)),
        "run A left its key:\n{pa:#?}"
    );
    rig.fixture(&s, "cleanup");
    rig.fixture(&s, "prepare");
    let again = rig.fixture(&s, "probe");
    assert_same("the real registry after cleanup and prepare", &p0, &again);

    // 3. Run B, through the overlay.
    let layer = Arc::new(MemoryProvider::new());
    // The layer's durable point: counted, and called when the layer is detached.
    let syncs = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&syncs);
    let sync: vfs_embed::RegistrySync = Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    s.set_registry_layer(Some(Arc::clone(&layer) as Arc<dyn Provider>), Some(sync))
        .expect("attach the registry layer");
    let b = rig.fixture(&s, "run");
    eprintln!("transcript: {} lines", a.len());
    assert_same("run B (overlay) against run A (real registry)", &a, &b);

    // 4. Another process of the same session sees what run B wrote.
    let pb = rig.fixture(&s, "probe");
    assert_same(
        "probe with the layer against the real registry after run A",
        &pa,
        &pb,
    );

    // 5. Nothing reached the real registry.
    s.set_registry_layer(None, None)
        .expect("detach the registry layer");
    assert!(
        syncs.load(Ordering::SeqCst) >= 1,
        "detaching reaches the layer's durable point"
    );
    assert!(
        layer
            .getattr(VPath::at_default("overlay.reg"))
            .unwrap()
            .is_some(),
        "detaching flushes overlay.reg into the layer"
    );
    let real = rig.fixture(&s, "probe");
    assert_same("the real registry after run B", &p0, &real);

    // 6. The layer persists: re-attached here, and loaded by a new session.
    s.set_registry_layer(Some(Arc::clone(&layer) as Arc<dyn Provider>), None)
        .unwrap();
    let reattached = rig.fixture(&s, "probe");
    assert_same("probe after re-attaching the layer", &pa, &reattached);
    drop(s);

    let s2 = rig.session("two");
    s2.set_registry_layer(Some(Arc::clone(&layer) as Arc<dyn Provider>), None)
        .unwrap();
    let loaded = rig.fixture(&s2, "probe");
    assert_same("probe in a new session with the same layer", &pa, &loaded);
    s2.set_registry_layer(None, None).unwrap();
    // Leave the prefix's real registry as it was found.
    rig.fixture(&s2, "cleanup");
    let gone = rig.fixture(&s2, "probe");
    assert!(has(&gone, "probe: no scratch root"), "{gone:#?}");
}
