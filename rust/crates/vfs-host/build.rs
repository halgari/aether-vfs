//! Link the neutral hollow host at the layout `vfs-inject` needs.
//!
//! See `src/main.rs` for why each of these matters. They are link-time only —
//! nothing here can be expressed in source.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    // Load at the address a Bethesda x64 image prefers, and refuse ASLR so we
    // actually get it. Hollow can then write the game image in place instead of
    // allocating a second region, and zip-served DLLs cannot squat the range.
    println!("cargo:rustc-link-arg-bins=/BASE:0x140000000");
    println!("cargo:rustc-link-arg-bins=/DYNAMICBASE:NO");
    println!("cargo:rustc-link-arg-bins=/FIXED");
    // 16 MiB primary stack: matches what vfs-inject otherwise has to patch in
    // post-hoc (`expand_primary_stack` / the temp `vfs-host-stack-*.exe` copy).
    println!("cargo:rustc-link-arg-bins=/STACK:0x1000000,0x40000");
    // Large-address-aware is implied on x64, but be explicit.
    println!("cargo:rustc-link-arg-bins=/LARGEADDRESSAWARE");
}
