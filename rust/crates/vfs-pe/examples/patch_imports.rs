//! Spike: `patch_imports <in.exe> <out.exe> <dll> <func>`.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let raw = std::fs::read(&a[1]).unwrap();
    let mut out = vfs_pe::add_first_import(&raw, &a[3], &a[4]).unwrap_or_else(|e| panic!("{e}"));
    let grew = vfs_pe::raise_stack_reserve(&mut out, 16 * 1024 * 1024).unwrap();
    std::fs::write(&a[2], &out).unwrap();
    let names = vfs_pe::import_dll_names_of_pe(&out).unwrap();
    println!(
        "{} -> {} bytes, stack raised: {grew}, imports: {:?}",
        raw.len(),
        out.len(),
        &names[..4.min(names.len())]
    );
}
