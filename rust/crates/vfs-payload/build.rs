//! Strip the CRT so the payload is loadable pre-init (only ntdll mapped).
//! Zero-import is the goal: no KERNEL32, no VCRUNTIME, no api-ms-win-crt.
//!
//! These must be `rustc-cdylib-link-arg`, not `rustc-link-arg`: the latter is
//! applied to *every* target in the package, including the unit-test binary,
//! which then fails to link (`/NODEFAULTLIB` with an `/ENTRY` of `DllMain` is
//! meaningless for a test executable). Scoping them to the cdylib is what lets
//! this crate have tests at all.
fn main() {
    println!("cargo:rustc-cdylib-link-arg=/NODEFAULTLIB");
    println!("cargo:rustc-cdylib-link-arg=/ENTRY:DllMain");
    println!("cargo:rerun-if-changed=build.rs");
}
