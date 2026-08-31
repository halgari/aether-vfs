//! Pre-init injection fixture: statically imports vproxy.dll. Reaching main
//! means the loader bound that import during process init. Exit 0 iff the
//! backing export value (4242) is observed.

// Force a PE import of vproxy.dll (not a static link of the rlib). Combined
// with build.rs link-search for the import library.
#[link(name = "vproxy.dll", kind = "dylib")]
extern "C" {
    fn vproxy_value() -> i32;
}

// There is deliberately no `use vproxy` here: that would require the package to
// also emit an rlib, and a cdylib+rlib package builds as two units that both
// write `vproxy.dll` (cargo#6313) — the collision that made `--all-targets`
// unusable.
//
// The cost of dodging that collision is that **nothing orders this link**. With
// no Cargo edge to vfs-fixture-vproxy, cargo is free to link this crate before
// the cdylib that produces `vproxy.dll.lib` exists, and on a clean target dir it
// does: LNK1181. Whoever builds this crate has to build vfs-fixture-vproxy
// first, in an earlier cargo invocation — CI does it in the "Build daemon +
// inject artifacts" step, and `vfs-inject`'s `tests/common` does it at test
// runtime. build.rs warns if the lib is missing, because the linker's own error
// names a file and not a cause.
//
// This comment previously claimed the ordering "comes from the Cargo dependency
// on vfs-fixture-vproxy". There is no such dependency — Cargo.toml removed it on
// purpose. The claim survived because a developer tree always happens to have
// the import lib lying around, so the gap only ever showed up on clean CI.

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
