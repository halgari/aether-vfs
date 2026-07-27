//! Strip the CRT so the payload is loadable pre-init (only ntdll mapped).
//! Zero-import is the goal: no KERNEL32, no VCRUNTIME, no api-ms-win-crt.
fn main() {
    println!("cargo:rustc-link-arg=/NODEFAULTLIB");
    println!("cargo:rustc-link-arg=/ENTRY:DllMain");
    println!("cargo:rerun-if-changed=build.rs");
}
