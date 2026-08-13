//! Pre-init injection fixture: statically imports vproxy.dll. Reaching main
//! means the loader bound that import during process init. Exit 0 iff the
//! backing export value (4242) is observed.

// Force a PE import of vproxy.dll (not a static link of the rlib). Combined
// with build.rs link-search for the import library.
#[link(name = "vproxy.dll", kind = "dylib")]
extern "C" {
    fn vproxy_value() -> i32;
}

// Ordering comes from the Cargo dependency on vfs-fixture-vproxy, which builds
// the cdylib and its import lib before this crate links. There is deliberately
// no `use vproxy` here: that would require the package to also emit an rlib,
// and a cdylib+rlib package builds as two units that both write `vproxy.dll`
// (cargo#6313) — the collision that made `--all-targets` unusable.

fn main() {
    let v = unsafe { vproxy_value() };
    let line = format!("vproxy_value={v}\n");
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = args.get(1) {
        let _ = std::fs::write(path, line.as_bytes());
    } else {
        print!("{line}");
    }
    std::process::exit(if v == 4242 { 0 } else { 3 });
}
