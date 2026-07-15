fn main() {
    let pe = std::fs::read(std::env::args().nth(1).unwrap()).unwrap();
    let (base, size) = vfs_inject::map_image_from_pe_bytes_local(&pe).expect("map");
    eprintln!("mapped base={base:p} size=0x{size:x}");
    // Read entry
    let e = u32::from_le_bytes(pe[0x3C..0x40].try_into().unwrap()) as usize;
    let entry_rva = u32::from_le_bytes(pe[e+24+16..e+24+20].try_into().unwrap()) as usize;
    let entry = (base as usize + entry_rva) as *const ();
    eprintln!("calling entry {entry:p}");
    unsafe {
        let f: extern "system" fn() -> i32 = std::mem::transmute(entry);
        let code = f();
        eprintln!("entry returned {code}");
    }
}
