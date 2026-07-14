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
}
