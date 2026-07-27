use std::env;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let target_dir = format!("{manifest_dir}/../target/debug");
    println!("cargo:rustc-link-search=native={target_dir}");
    // MSVC names the cdylib's import library "helper.dll.lib". Passing
    // "helper.dll" here makes rustc ask the linker for "helper.dll" + ".lib"
    // = "helper.dll.lib", matching that name exactly.
    println!("cargo:rustc-link-lib=dylib=helper.dll");
}
