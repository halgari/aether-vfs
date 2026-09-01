//! Task 4b: launching an image that is **VFS content** — the top item on the
//! embeddable API's gap list.
//!
//! A managed root is deliberately empty on disk; the game lives in the
//! provider graph. `CreateProcess` cannot create a process from bytes, so the
//! image has to be written out with its PE import closure and the staging
//! directory mounted back into the graph. Until this task that sequence
//! existed once, in `vfs-directord`'s `SessionRegistry::launch`, and no other
//! host could reach it.
//!
//! Two things are proven here, and the **first one is the load-bearing one**:
//!
//! 1. Mounting staging back must not invert precedence. The daemon mounted it
//!    at `i32::MIN` inside a `stack_layers` stack, where the *first* entry is
//!    the bottom and loses. `Session`'s own composition is a `MountGraph`,
//!    whose lookup walks `.rev()`, so the *last* entry wins — a naive
//!    relocation (`mount_at(root, "/", staged)`) therefore lets a
//!    point-in-time staged copy silently shadow curated content, on exactly
//!    the paths staging touches.
//! 2. The capability itself: an empty managed root, an image only the graph
//!    holds, a process that actually runs.

use std::collections::HashMap;
use std::sync::Arc;

use vfs_embed::stage::ImageSource;
use vfs_embed::{InlineProvider, Provider, RootId, Session, StageOpts};
// Only the two `#[cfg(windows)]` launch tests below construct one.
#[cfg(windows)]
use vfs_embed::LaunchOpts;

/// Minimal PE: MZ header, e_lfanew, PE32+ optional header, no imports — the
/// same shape `vfs_director::stage`'s own tests and `vfs-directord`'s
/// `staging.rs` use. The trailing marker is what makes "which copy answered"
/// an observable fact rather than an inference.
fn bare_pe(marker: &[u8]) -> Vec<u8> {
    let mut pe = vec![0u8; 0x400];
    pe[0] = b'M';
    pe[1] = b'Z';
    pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    pe[0x80..0x84].copy_from_slice(b"PE\0\0");
    pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
    pe[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
    pe[0x98..0x9A].copy_from_slice(&0x20Bu16.to_le_bytes());
    pe.extend_from_slice(marker);
    pe
}

/// The trailing marker of a [`bare_pe`], so a failed assertion names the copy
/// that answered instead of printing a kilobyte of zeroes.
fn marker(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[0x400.min(bytes.len())..]).into_owned()
}

struct Fake(HashMap<String, Vec<u8>>);
impl ImageSource for Fake {
    fn read(&self, vpath: &str) -> Option<Vec<u8>> {
        self.0.get(vpath).cloned()
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("vfs-launch-content-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn inline(files: &[(&str, &[u8])]) -> Arc<dyn Provider> {
    Arc::new(InlineProvider::from_files(files.iter().copied()))
}

/// **The guard against the precedence trap.**
///
/// Staging writes a point-in-time copy of whatever the graph said at launch
/// time. It exists so the loader can find a real file before the shim is in
/// the child — it is not a content decision, and it must never outrank the
/// curated graph on a path both serve. Get this backwards and the failure is
/// invisible: reads keep working, they just serve a stale copy, and only on
/// the handful of paths staging touches.
///
/// The `helper.exe` assertions are not decoration. Without them a session
/// that mounted staging *nowhere* would pass the shadowing assertion
/// perfectly, which is the exact shape of a test that claims coverage it does
/// not have.
#[test]
fn a_staged_copy_must_not_shadow_curated_content_at_the_same_path() {
    let state = tmp("precedence-state");

    let mut s = Session::new();
    s.set_state_dir(&state);
    s.mount("", inline(&[("game.exe", &bare_pe(b"CURATED"))]))
        .unwrap();

    // The staged copy deliberately disagrees with the graph at `game.exe`,
    // and carries one image (`helper.exe`) nothing curated serves.
    let mut m = HashMap::new();
    m.insert("game.exe".to_string(), bare_pe(b"STAGED"));
    m.insert("helper.exe".to_string(), bare_pe(b"HELPER"));
    let source = Fake(m);

    let exe = s
        .stage_launch(
            &source,
            &StageOpts {
                exe_vpath: "game.exe",
                also: &["helper.exe"],
                fallback_dirs: &[],
            },
        )
        .expect("stage");
    assert!(exe.is_file(), "CreateProcess still needs a real on-disk image");

    assert_eq!(
        marker(&s.read_file("game.exe").unwrap()),
        "CURATED",
        "a staged copy must never shadow curated content at the same path — the \
         staging mount has to compose *below* everything the host mounted, not \
         above it"
    );

    // Non-vacuity: staging really is mounted, so the assertion above is about
    // precedence and not about an absent mount.
    assert_eq!(
        marker(&s.read_file("helper.exe").unwrap()),
        "HELPER",
        "a path only staging serves must resolve through the graph"
    );
    let listed = s.kernel().readdir(RootId::DEFAULT, "").unwrap();
    assert!(
        listed.iter().any(|e| e.name == "helper.exe"),
        "the staged image must enumerate too, not merely answer getattr/open: {:?}",
        listed.iter().map(|e| &e.name).collect::<Vec<_>>()
    );

    // A host that keeps its own source list and rebuilds a root wholesale
    // (`SessionRegistry::add_source` does this on every source) must not
    // knock staging out of the graph, and must not promote it either.
    s.set_root_mounts(
        RootId::DEFAULT,
        vec![(String::new(), inline(&[("game.exe", &bare_pe(b"CURATED-2"))]))],
    )
    .unwrap();
    assert_eq!(
        marker(&s.read_file("game.exe").unwrap()),
        "CURATED-2",
        "the rebuilt content must still outrank staging"
    );
    assert_eq!(
        marker(&s.read_file("helper.exe").unwrap()),
        "HELPER",
        "staging must survive a wholesale rebuild of the root's mounts"
    );

    let _ = std::fs::remove_dir_all(&state);
}

/// A launcher's spawn target must land **where the launcher will look for it**.
///
/// SKSE's `skse64_loader.exe` starts `SkyrimSE.exe` with an ordinary
/// `CreateProcess` that nothing of ours intercepts, so the game has to be a
/// real file where the loader resolves it before the loader runs — the reason
/// [`LaunchOpts::stage_also`] exists, and the reason `vfs-launch`'s default
/// invocation needs it.
///
/// Staging lands **inside the virtual root, at each image's vpath**. Two
/// consequences, both asserted here: a companion that is the launcher's
/// sibling in the graph is its sibling on disk — the SKSE shape, loader and
/// game together at the game root — and a nested companion keeps its
/// directory, which is what keeps a game resolving content relative to its own
/// module path inside the VFS. Staging flattened into `state_dir/stage` did
/// neither, and that is what stopped Cyberpunk 2077 and Stardew Valley.
///
/// The bytes are `bare_pe`s, so the launch itself cannot succeed. What
/// happened before that failure is the point, exactly as in `vfs-directord`'s
/// `production_launch_stages_a_relative_image_before_create_process` — the
/// staged files survive it because the session holds the `StagedDir`.
// Needs a live ring and a real `CreateProcess` + inject: Windows-only until the
// Proton path lands (increment 2 of the Wine-hosted-shim design).
#[cfg(windows)]
#[test]
fn launch_stages_companion_images_at_their_vpath_inside_the_root() {
    let state = tmp("companion-state");
    let root = tmp("companion-root");

    let mut s = Session::new();
    s.set_root(&root);
    s.set_state_dir(&state);
    s.set_overlay(tmp("companion-overlay"));
    s.mount(
        "",
        inline(&[
            ("loader.exe", &bare_pe(b"LOADER")),
            ("game.exe", &bare_pe(b"GAME")),
            ("Data/nested.exe", &bare_pe(b"NESTED")),
        ]),
    )
    .unwrap();
    s.serve().expect("serve");

    let err = s
        .launch(&LaunchOpts {
            image: "loader.exe".into(),
            stage_also: vec!["game.exe".into(), "Data/nested.exe".into()],
            wait: true,
            ..Default::default()
        })
        .expect_err("a bare_pe is not a runnable image");
    assert!(!err.is_empty());

    let listing = || {
        std::fs::read_dir(&root).map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>())
    };
    assert!(
        root.join("loader.exe").is_file(),
        "the launcher itself must be staged into the root: {:?}",
        listing()
    );
    assert!(
        root.join("game.exe").is_file(),
        "a companion the launcher will spawn by bare name must be its sibling          on disk — the child's own CreateProcess resolves it from there: {:?}",
        listing()
    );
    assert!(
        root.join("Data").join("nested.exe").is_file(),
        "a nested companion must keep its vpath directory rather than being          flattened beside the launcher: {:?}",
        listing()
    );

    s.stop_serve();
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&root);
}

// The refusals that remain — a relative image neither disk nor graph holds,
// and an empty one — stay asserted in `embed_api.rs`, beside the rest of the
// launch contract.

// ---------------------------------------------------------------------------
// The capability, end to end: a real process, from bytes only the graph held.
// ---------------------------------------------------------------------------

// The fixture-locating trio serves only the `#[cfg(windows)]` launch tests
// below: the artifacts they look for are Windows PEs and DLLs.
#[cfg(windows)]
fn profile_dir() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().unwrap();
    if dir.file_name().and_then(|s| s.to_str()) == Some("deps") {
        dir.parent().unwrap().to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

#[cfg(windows)]
fn locate_artifact(name: &str) -> std::path::PathBuf {
    let profile = profile_dir();
    for cand in [profile.join(name), profile.join("deps").join(name)] {
        if cand.is_file() {
            return cand;
        }
    }
    panic!("{name} not found near {profile:?} after ensure_fixtures()");
}

/// Build the shim DLL, the (separate-workspace) payload DLL and `vfs-probe`
/// once per test process and co-locate them beside the test binary, so
/// `Session::launch`'s own DLL search (near `current_exe()`) finds them.
///
/// Duplicated from `fuse_init_gate.rs` rather than shared: this is the
/// convention every launch-capable test harness in the workspace follows
/// (`vfs-inject`'s and `vfs-directord`'s do the same), and a `tests/support`
/// module for two callers would be the only shared build step in the crate.
#[cfg(windows)]
fn ensure_fixtures() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .canonicalize()
            .expect("workspace root");

        let status = std::process::Command::new(&cargo)
            .current_dir(&workspace)
            .args([
                "build", "-p", "vfs-shim-dll", "-p", "vfs-inject", "--bin", "vfs-probe", "--quiet",
            ])
            .status()
            .expect("spawn cargo to build shim + vfs-probe");
        assert!(status.success(), "shim/vfs-probe build failed: {status}");

        let target_dir = workspace.join("target");
        let status = std::process::Command::new(&cargo)
            .current_dir(&workspace)
            .env("CARGO_TARGET_DIR", &target_dir)
            .args([
                "build",
                "--manifest-path",
                "crates/vfs-payload/Cargo.toml",
                "--quiet",
            ])
            .status()
            .expect("spawn cargo to build vfs-payload");
        assert!(status.success(), "vfs-payload build failed: {status}");

        let profile = profile_dir();
        for name in ["vfs_shim_dll.dll", "vfs_payload.dll", "vfs-probe.exe"] {
            let dest = profile.join(name);
            if dest.is_file() {
                continue;
            }
            let src = profile.join("deps").join(name);
            if src.is_file() {
                let _ = std::fs::copy(&src, &dest);
            }
        }
    });
}

/// **The capability.** The managed root is empty on real disk, the executable
/// exists only as bytes in the provider graph, and `Session::launch` runs it.
///
/// Every assertion here is about something a host could not do before:
///
/// * the root is empty before the launch, and empty again once the session is
///   dropped — staging lands in the root at the image's vpath, so what is
///   promised is not that nothing is ever written there but that nothing
///   *outlives the session*;
/// * the child's own read of `hello.txt` — a path with no file behind it
///   anywhere — is served through the ring, which is what proves the process
///   ran virtualized rather than merely ran;
/// * the staged image answers at its vpath afterwards, so a later
///   hook-mediated open of the same relative name resolves through the graph
///   instead of falling through to disk.
// Needs a live ring and a real `CreateProcess` + inject: Windows-only until the
// Proton path lands (increment 2 of the Wine-hosted-shim design).
#[cfg(windows)]
#[test]
fn an_image_only_the_provider_graph_holds_launches_from_an_empty_managed_root() {
    ensure_fixtures();

    let probe = std::fs::read(locate_artifact("vfs-probe.exe")).expect("read vfs-probe.exe");
    let root = tmp("live-root");
    let state = tmp("live-state");
    let overlay = tmp("live-overlay");
    let out = tmp("live-out").join("probe-out.bin");

    let mut s = Session::new();
    s.set_root(&root);
    s.set_state_dir(&state);
    s.set_overlay(&overlay);
    s.mount(
        "",
        inline(&[
            ("probe.exe", &probe),
            ("hello.txt", b"served-from-the-graph"),
        ]),
    )
    .unwrap();
    s.serve().expect("serve");

    assert!(
        std::fs::read_dir(&root).unwrap().next().is_none(),
        "the managed root must be empty on disk — that is the whole case"
    );

    let code = s
        .launch(&LaunchOpts {
            image: "probe.exe".into(),
            args: vec![
                root.join("hello.txt").to_string_lossy().into_owned(),
                out.to_string_lossy().into_owned(),
            ],
            wait: true,
            ..Default::default()
        })
        .expect("an image only the provider graph holds must launch");
    assert_eq!(code, 0, "the child must exit cleanly");

    assert_eq!(
        std::fs::read(&out).expect("the child must have written its output"),
        b"served-from-the-graph",
        "the child's read went through the ring to the provider graph"
    );
    assert!(
        root.join("probe.exe").is_file(),
        "the image must be staged into the root at its own vpath — that is what          keeps everything the child resolves relative to its module path inside          the VFS: {:?}",
        std::fs::read_dir(&root)
            .map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>())
    );
    // The staged image must stay answerable at its vpath — and *staging* must
    // be what answers. Reading `probe.exe` while the host's own mount is still
    // there proves nothing: the graph served those bytes to begin with. So the
    // host's mounts are dropped wholesale first, leaving staging as the only
    // thing that can reply.
    s.set_root_mounts(RootId::DEFAULT, vec![]).unwrap();
    assert!(
        s.read_file("hello.txt").is_err(),
        "the host's own mount is gone, so its content must be gone with it — \
         otherwise the next assertion proves nothing"
    );
    assert_eq!(
        s.read_file("probe.exe").unwrap().len(),
        probe.len(),
        "the staged image must stay answerable at its vpath, and survive the host \
         rebuilding the root's mounts"
    );

    s.stop_serve();
    // `StagedDir`'s `Drop` is what removes the staged files, and the session is
    // what holds it alive — so dropping the session is what returns the managed
    // root to the state it was found in. This is the surviving half of "nothing
    // was extracted into it".
    drop(s);
    assert!(
        std::fs::read_dir(&root).unwrap().next().is_none(),
        "staging must not outlive the session: {:?}",
        std::fs::read_dir(&root)
            .map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>())
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&overlay);
}
