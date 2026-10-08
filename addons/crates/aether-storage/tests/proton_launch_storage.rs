//! **A named `aether-storage` layer as a Proton launch's write layer**, end to
//! end: the names test moved here from `vfs-embed`'s `proton_launch` with the
//! storage, because its write layer is a host's (`Storage::layer`), as
//! Haskill's is. See that file's module docs for what a Proton end-to-end test
//! needs and why the child's paths are literals; its support module (scratch
//! under `target/tmp`, artefacts, the throwaway home, `SKIP` on a missing
//! prerequisite) is shared, not copied. With this workspace's own `target/`,
//! point `VFS_WINDOWS_ARTIFACTS` at the directory `bin/build-windows` filled.
#![cfg(unix)]

#[path = "../../../../rust/crates/vfs-embed/tests/support/mod.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aether_storage::{Storage, StorageConfig};
use vfs_embed::{DiskProvider, LaunchOpts, Provider, Session};

/// The content of `Data/hello.txt`: one page of a single non-zero byte, as in
/// `vfs-embed`'s `proton_launch`.
const FILL: u8 = 0x5A;
const LEN: usize = 4096;

/// One Wine launch at a time: each test boots its own prefix.
static ONE_LAUNCH: Mutex<()> = Mutex::new(());

fn tmp(name: &str) -> PathBuf {
    support::scratch("vfs-proton-launch-storage", name)
}

/// Where root 0 is in the child for the names test: a location with capitals
/// in it, as a host's is, so "spelled as stored" is a claim about the root's
/// own path too.
const NAMES_ROOT: &str = r"C:\Haskill\TestList\game";

/// **A virtual directory has a final path, and it is the prefix of the final
/// path of every file under it.**
///
/// `std::filesystem::canonical` is `CreateFileW` with
/// `FILE_FLAG_BACKUP_SEMANTICS` then `GetFinalPathNameByHandleW`. On a
/// virtual handle that used to fail outright — the shim answered no name
/// query for a handle the director serves — so a plugin that checks a file is
/// inside its own directory by comparing canonical paths (Community Shaders,
/// for its fonts) rejected every one of them as a path traversal.
///
/// The fixture asks, for four kinds of directory and for files under them —
/// the root itself, a directory two providers both have, one only one
/// provider has, and one that exists only on the real disk under the root —
///
/// * `GetFinalPathNameByHandleW` in every volume-name and file-name form;
/// * `canonicalize`, `GetFileAttributesW`, `metadata`,
///   `GetFileInformationByHandle` and `…Ex` (attribute tag, name, id);
/// * all of it again with the path in the opposite letter case, which must
///   give the same final path and the same file id;
/// * that `canonical(dir)` is a byte prefix of `canonical(dir/file)`, with
///   the directory given in another case than the file;
/// * that a listing of a merged directory has both providers' entries.
///
/// The expected paths are spelled as the providers store them, which is not
/// how the fixture is told to open them.
#[test]
#[ignore = "needs a GE-Proton runtime, a bootable Wine prefix, and Windows-built artifacts \
            for this profile — see bin/build-windows"]
fn a_virtual_directory_has_a_final_path_that_prefixes_its_files_under_proton() {
    let _one = ONE_LAUNCH.lock().unwrap_or_else(|e| e.into_inner());
    let Some(rig) = support::rig(
        "proton_launch_storage::a_virtual_directory_has_a_final_path_that_prefixes_its_files_under_proton",
        "launch",
        &[vfs_proton::artifacts::FIXTURE_READ],
    ) else {
        return;
    };
    let art = &rig.art;

    let root = tmp("n-root");
    let state = tmp("n-state");
    let overlay = tmp("n-overlay");
    let upper = tmp("n-upper");
    let lower = tmp("n-lower");
    let write = |base: &Path, rel: &str, bytes: &[u8]| {
        let p = base.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    };
    // One provider has the font, the other a second file in the same
    // directory and a directory of its own; the real root has a third.
    write(
        &upper,
        "Data/Interface/CommunityShaders/Fonts/Jost/Jost-Regular.ttf",
        b"font",
    );
    write(&upper, "Data/hello.txt", &[FILL; LEN]);
    write(
        &lower,
        "Data/Interface/CommunityShaders/Fonts/Other.ttf",
        b"other",
    );
    write(&lower, "Data/OnlyInLower/b.txt", b"b");
    write(&root, "RealOnly/r.txt", b"r");
    let image = root.join("fixture.exe");
    std::fs::copy(art.path(vfs_proton::artifacts::FIXTURE_READ), &image)
        .expect("copy the fixture into the root");

    // A write layer like a host's: what the fixture creates lands here, and
    // the providers below stay read-only.
    let storage_dir = tmp("n-storage");
    let storage = Storage::open(&storage_dir, StorageConfig::default()).expect("open storage");

    let mut s = Session::new();
    s.set_home(&rig.home);
    s.set_root(&root);
    s.declare_root(0, NAMES_ROOT);
    s.set_state_dir(&state);
    s.set_overlay(&overlay);
    for dir in [&upper, &lower, &root] {
        s.mount("", Arc::new(DiskProvider::new(dir)) as Arc<dyn Provider>)
            .expect("mount a provider over root 0");
    }
    s.set_write_layer(storage.layer("write").expect("a write layer"))
        .expect("set the write layer");
    s.serve().expect("serve");

    let at = |rel: &str| format!(r"{NAMES_ROOT}\{rel}");
    let fonts = at(r"Data\Interface\CommunityShaders\Fonts");
    let font = at(r"Data\Interface\CommunityShaders\Fonts\Jost\Jost-Regular.ttf");
    // kind | how the fixture opens it | its final path. The fixture also
    // opens each in the opposite letter case of its own accord.
    let names = [
        ("d", NAMES_ROOT.to_string(), NAMES_ROOT.to_string()),
        ("d", at("Data"), at("Data")),
        ("d", fonts.clone(), fonts.clone()),
        (
            "d",
            at(r"DATA\interface\COMMUNITYSHADERS\fonts\"),
            fonts.clone(),
        ),
        ("d", at(r"Data\OnlyInLower"), at(r"Data\OnlyInLower")),
        ("d", at("RealOnly"), at("RealOnly")),
        ("f", font.clone(), font.clone()),
        (
            "f",
            at(r"data\INTERFACE\communityshaders\FONTS\other.TTF"),
            at(r"Data\Interface\CommunityShaders\Fonts\Other.ttf"),
        ),
        (
            "f",
            at(r"Data\OnlyInLower\b.txt"),
            at(r"Data\OnlyInLower\b.txt"),
        ),
        ("f", at(r"realonly\R.TXT"), at(r"RealOnly\r.txt")),
    ];
    let prefixes = [
        (fonts.clone(), font.clone()),
        (fonts.to_uppercase(), font.to_lowercase()),
        (NAMES_ROOT.to_lowercase(), at(r"Data\hello.txt")),
        (at("realonly"), at(r"RealOnly\r.txt")),
        // A directory named before a write under it, a file named after.
        (
            at("DATA"),
            at(r"data\interface\communityshaders\fonts\new font.ttf"),
        ),
        (fonts.to_lowercase(), font.clone()),
    ];
    // Written by the fixture after it has named the prefix directories:
    // under directories the providers have, spelled in lower case the way
    // nothing on disk is (that is how `Data` became `data`); a directory and
    // a file of the game's own; a long save name; and a rename of one to
    // another letter case and of another to a new name.
    const SAVE: &str = "Save12_ABCDEF01_0_4E6F726420486572_Tamriel_000123_20261002150000_1_1.ess";
    let creates = [
        at(r"data\interface\communityshaders\fonts\New Font.TTF"),
        at(r"data\SKSE\"),
        at(r"Data\SKSE\CommunityShaders.log"),
        at(r"Saves\"),
        at(&format!(r"Saves\{SAVE}")),
        at(r"Saves\quicksave.ESS"),
        at(r"Saves\Old Name.ess"),
    ];
    let renames = [
        (at(r"Saves\quicksave.ESS"), at(r"Saves\QuickSave.ess")),
        (at(r"Saves\Old Name.ess"), at(r"Saves\New Name.ESS")),
    ];
    let lists = [
        (fonts.clone(), "Jost,Other.ttf"),
        (NAMES_ROOT.to_string(), "Data,RealOnly,fixture.exe"),
        (at("DATA"), "Interface,OnlyInLower,hello.txt,SKSE"),
        (NAMES_ROOT.to_string(), "Data,RealOnly,fixture.exe,Saves"),
    ];

    let mut env = BTreeMap::new();
    env.insert("VFS_FIXTURE_PATH".to_string(), at(r"Data\hello.txt"));
    env.insert("VFS_FIXTURE_EXPECT".to_string(), LEN.to_string());
    env.insert("VFS_FIXTURE_FILL".to_string(), FILL.to_string());
    env.insert(
        "VFS_FIXTURE_NAMES".to_string(),
        names
            .iter()
            .map(|(k, o, w)| format!("{k}|{o}|{w}"))
            .collect::<Vec<_>>()
            .join(";"),
    );
    env.insert(
        "VFS_FIXTURE_NAME_PREFIXES".to_string(),
        prefixes
            .iter()
            .map(|(d, f)| format!("{d}|{f}"))
            .collect::<Vec<_>>()
            .join(";"),
    );
    env.insert("VFS_FIXTURE_NAME_CREATES".to_string(), creates.join(";"));
    env.insert(
        "VFS_FIXTURE_NAME_RENAMES".to_string(),
        renames
            .iter()
            .map(|(f, t)| format!("{f}|{t}"))
            .collect::<Vec<_>>()
            .join(";"),
    );
    env.insert(
        "VFS_FIXTURE_NAME_LISTS".to_string(),
        lists
            .iter()
            .map(|(d, c)| format!("{d}|{c}"))
            .collect::<Vec<_>>()
            .join(";"),
    );

    let code = s
        .launch(&LaunchOpts {
            image: "fixture.exe".into(),
            wait: true,
            shim_dll: Some(art.shim_dll()),
            env,
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("launch: {e}"));
    assert_eq!(
        code, 0,
        "the fixture exits 0 only if every name query on every path agreed; its own \
         `FIXTURE FAIL: names:` line above says which did not"
    );

    s.stop_serve();
    drop(s);
    drop(storage);
    for d in [&root, &state, &overlay, &upper, &lower, &storage_dir] {
        let _ = std::fs::remove_dir_all(d);
    }
}
