//! This crate does its own Steam login and never reads or reuses the native
//! Steam client's credentials or configuration. steamroom-client has helpers
//! that do exactly that, so it must stay out of the dependency graph, and
//! nothing in this crate may name the Steam client's files.
use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "steamroom-client",
    "steamroom_client",
    "steam_creds",
    "local.vdf",
    "config.vdf",
    "loginusers.vdf",
    "share/Steam",
    ".steam/",
    "Valve\\Steam",
    ".depotdownloader",
];

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs" || x == "toml") {
            out.push(p);
        }
    }
}

#[test]
fn crate_never_touches_native_steam_credentials() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut all = vec![root.join("Cargo.toml")];
    for d in ["src", "tests", "examples"] {
        if root.join(d).is_dir() {
            files(&root.join(d), &mut all);
        }
    }
    let me = root.join("tests/no_native_credentials.rs");
    let mut hits = Vec::new();
    for f in all.iter().filter(|f| **f != me) {
        let text = std::fs::read_to_string(f).unwrap();
        for word in FORBIDDEN {
            if text.contains(word) {
                hits.push(format!("{}: {word}", f.display()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "native Steam credential access:\n{}",
        hits.join("\n")
    );
}

#[test]
fn steamroom_client_is_not_in_the_lockfile() {
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let text = std::fs::read_to_string(&lock).unwrap();
    assert!(
        !text.contains("name = \"steamroom-client\""),
        "steamroom-client (which can read the Steam client's saved login) is in {}",
        lock.display()
    );
}
