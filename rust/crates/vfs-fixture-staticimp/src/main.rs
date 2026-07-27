//! Pre-init injection fixture: statically imports vproxy.dll. Reaching main
//! means the loader bound that import during process init. Exit 0 iff the
//! backing export value (4242) is observed.

// Force a PE import of vproxy.dll (not a static link of the rlib). Combined
// with build.rs link-search for the import library.
#[link(name = "vproxy.dll", kind = "dylib")]
extern "C" {
    fn vproxy_value() -> i32;
}

// Keep a rustc dependency edge so Cargo builds vproxy first (rlib/cdylib).
// The lib crate name is `vproxy` (see vfs-fixture-vproxy Cargo.toml).
#[allow(unused_imports)]
use vproxy as _;

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
