//! Force a STATIC PE import of vproxy.dll by linking its import library.
fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    // OUT_DIR = <target_dir>/<profile>/build/<pkg>-<hash>/out → up 3 = profile dir
    let profile_dir = std::path::Path::new(&out)
        .ancestors()
        .nth(3)
        .expect("OUT_DIR ancestry")
        .to_path_buf();
    // Search profile dir and deps (import lib may land in either).
    println!("cargo:rustc-link-search=native={}", profile_dir.display());
    println!(
        "cargo:rustc-link-search=native={}",
        profile_dir.join("deps").display()
    );
    // MSVC names the cdylib import lib "vproxy.dll.lib". Passing "vproxy.dll"
    // makes rustc request "vproxy.dll.lib".
    println!("cargo:rustc-link-lib=dylib=vproxy.dll");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../vfs-fixture-vproxy/src/lib.rs");

    // Re-run when the import lib appears or vanishes, so the warning below
    // reflects the tree as it is now. Without this, cargo caches the script's
    // output and replays the warning long after the lib has been built, leaving
    // it insisting a file is missing while that file sits right there — and a
    // stale signal is the failure mode this fixture already suffered once.
    let candidates = ["vproxy.dll.lib", "libvproxy.dll.a"];
    for lib in candidates {
        println!("cargo:rerun-if-changed={}", profile_dir.join(lib).display());
        println!(
            "cargo:rerun-if-changed={}",
            profile_dir.join("deps").join(lib).display()
        );
    }

    // This crate has no Cargo dependency on vfs-fixture-vproxy (cargo#6313 — see
    // Cargo.toml), so nothing guarantees the import lib exists by the time rustc
    // links. When it does not, the link fails as
    //
    //     LINK : fatal error LNK1181: cannot open input file 'vproxy.dll.lib'
    //
    // which names the file and not the reason. Name the reason here. A warning
    // rather than a hard error on purpose: within a single cargo invocation the
    // cdylib may still be built between this script and the link, and failing
    // that case would trade a real bug for an invented one.
    if !candidates
        .iter()
        .any(|lib| profile_dir.join(lib).is_file() || profile_dir.join("deps").join(lib).is_file())
    {
        println!(
            "cargo:warning=vfs-fixture-staticimp: vproxy import lib not found under {}. \
             Build it first — `cargo build -p vfs-fixture-vproxy` — or this link fails LNK1181. \
             There is no Cargo dependency to order it for you (cargo#6313).",
            profile_dir.display()
        );
    }
}
