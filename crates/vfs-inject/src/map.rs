//! Reflective PE mapping helpers: flatten a DLL image, apply base relocations,
//! and resolve exports by name. Used to place the zero-import payload into a
//! suspended target without LoadLibrary.
#![allow(unsafe_code)]

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Build a flat in-memory image (SizeOfImage bytes) from a PE file: copy
/// headers, place each section at its VirtualAddress. Returns
/// `(image, preferred_image_base, e_lfanew)`.
pub fn build_image(raw: &[u8]) -> Result<(Vec<u8>, u64, usize), &'static str> {
    if raw.len() < 0x40 {
        return Err("PE too small");
    }
    let e_lfanew = rd_u32(raw, 0x3C) as usize;
    if e_lfanew + 24 + 112 > raw.len() {
        return Err("bad e_lfanew");
    }
    if &raw[e_lfanew..e_lfanew + 4] != b"PE\0\0" {
        return Err("bad PE sig");
    }
    let opt = e_lfanew + 24;
    if rd_u16(raw, opt) != 0x20B {
        return Err("not PE32+");
    }
    let size_of_image = rd_u32(raw, opt + 56) as usize;
    let size_of_headers = rd_u32(raw, opt + 60) as usize;
    let image_base = rd_u64(raw, opt + 24);
    let num_sections = rd_u16(raw, e_lfanew + 6) as usize;
    let size_opt = rd_u16(raw, e_lfanew + 20) as usize;
    let sect_base = opt + size_opt;

    let mut img = vec![0u8; size_of_image];
    if size_of_headers > raw.len() || size_of_headers > size_of_image {
        return Err("bad SizeOfHeaders");
    }
    img[..size_of_headers].copy_from_slice(&raw[..size_of_headers]);
    for i in 0..num_sections {
        let s = sect_base + i * 40;
        if s + 40 > raw.len() {
            return Err("section table OOB");
        }
        let va = rd_u32(raw, s + 12) as usize;
        let raw_sz = rd_u32(raw, s + 16) as usize;
        let raw_ptr = rd_u32(raw, s + 20) as usize;
        if raw_sz == 0 {
            continue;
        }
        if raw_ptr + raw_sz > raw.len() || va + raw_sz > size_of_image {
            return Err("section data OOB");
        }
        img[va..va + raw_sz].copy_from_slice(&raw[raw_ptr..raw_ptr + raw_sz]);
    }
    Ok((img, image_base, e_lfanew))
}

/// Apply IMAGE_REL_BASED_DIR64 base relocations in-place for a new load base.
pub fn apply_relocs(img: &mut [u8], e_lfanew: usize, image_base: u64, new_base: u64) {
    let opt = e_lfanew + 24;
    let reloc_dir = opt + 112 + 5 * 8; // DataDirectory[IMAGE_DIRECTORY_ENTRY_BASERELOC]
    if reloc_dir + 8 > img.len() {
        return;
    }
    let reloc_rva = rd_u32(img, reloc_dir) as usize;
    let reloc_size = rd_u32(img, reloc_dir + 4) as usize;
    if reloc_rva == 0 || reloc_size == 0 {
        return;
    }
    let delta = new_base.wrapping_sub(image_base);
    let mut off = reloc_rva;
    let end = reloc_rva + reloc_size;
    while off + 8 <= end && off + 8 <= img.len() {
        let block_va = rd_u32(img, off) as usize;
        let block_sz = rd_u32(img, off + 4) as usize;
        if block_sz < 8 {
            break;
        }
        let count = (block_sz - 8) / 2;
        for i in 0..count {
            let eoff = off + 8 + i * 2;
            if eoff + 2 > img.len() {
                break;
            }
            let e = rd_u16(img, eoff);
            let typ = e >> 12;
            let o = (e & 0xFFF) as usize;
            if typ == 10 {
                let p = block_va + o;
                if p + 8 <= img.len() {
                    let v = rd_u64(img, p).wrapping_add(delta);
                    img[p..p + 8].copy_from_slice(&v.to_le_bytes());
                }
            }
        }
        off += block_sz;
    }
}

/// Resolve the PE import table in-place using **this process's** module bases
/// (system DLLs share session-wide bases, which is enough for typical loader
/// EXEs like skse64_loader). Call after `build_image` / before writing into a
/// hollowed target.
pub fn resolve_imports(img: &mut [u8], e_lfanew: usize) -> Result<(), &'static str> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

    let opt = e_lfanew + 24;
    // DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT = 1]
    let imp_dir = opt + 112 + 8;
    if imp_dir + 8 > img.len() {
        return Ok(());
    }
    let mut desc = rd_u32(img, imp_dir) as usize;
    let imp_size = rd_u32(img, imp_dir + 4) as usize;
    if desc == 0 || imp_size == 0 {
        return Ok(());
    }
    let desc_end = desc + imp_size;

    while desc + 20 <= img.len() && desc < desc_end {
        let oft = rd_u32(img, desc) as usize; // OriginalFirstThunk
        let name_rva = rd_u32(img, desc + 12) as usize;
        let ft = rd_u32(img, desc + 16) as usize; // FirstThunk (IAT)
        if name_rva == 0 {
            break;
        }
        // DLL name
        if name_rva >= img.len() {
            break;
        }
        let mut end = name_rva;
        while end < img.len() && img[end] != 0 {
            end += 1;
        }
        let mut dll_name = img[name_rva..end].to_vec();
        dll_name.push(0);
        // SAFETY: LoadLibraryA of a normal system/import DLL name from the PE.
        let module = unsafe { LoadLibraryA(dll_name.as_ptr()) };
        if module.is_null() {
            return Err("LoadLibraryA for import failed");
        }
        let mut thunk_rva = if oft != 0 { oft } else { ft };
        let mut iat_rva = ft;
        loop {
            if thunk_rva + 8 > img.len() || iat_rva + 8 > img.len() {
                break;
            }
            let entry = rd_u64(img, thunk_rva);
            if entry == 0 {
                break;
            }
            let addr = if entry & (1u64 << 63) != 0 {
                // Import by ordinal
                let ord = (entry & 0xFFFF) as u16;
                unsafe { GetProcAddress(module, ord as *const u8) }
            } else {
                // IMAGE_IMPORT_BY_NAME: Hint (2) + Name
                let name_ptr = (entry as usize) + 2;
                if name_ptr >= img.len() {
                    break;
                }
                let mut ne = name_ptr;
                while ne < img.len() && img[ne] != 0 {
                    ne += 1;
                }
                let mut nm = img[name_ptr..ne].to_vec();
                nm.push(0);
                unsafe { GetProcAddress(module, nm.as_ptr()) }
            };
            let Some(func) = addr else {
                return Err("GetProcAddress for import failed");
            };
            let fa = func as u64;
            img[iat_rva..iat_rva + 8].copy_from_slice(&fa.to_le_bytes());
            thunk_rva += 8;
            iat_rva += 8;
        }
        desc += 20;
    }
    Ok(())
}

/// Find an export's RVA by name (RVAs index into the flat image).
pub fn export_rva(img: &[u8], e_lfanew: usize, name: &[u8]) -> Result<u32, &'static str> {
    let opt = e_lfanew + 24;
    let exp_dir = opt + 112; // DataDirectory[IMAGE_DIRECTORY_ENTRY_EXPORT]
    if exp_dir + 8 > img.len() {
        return Err("no export dir");
    }
    let exp_rva = rd_u32(img, exp_dir) as usize;
    if exp_rva == 0 || exp_rva + 40 > img.len() {
        return Err("empty export dir");
    }
    let num_names = rd_u32(img, exp_rva + 24) as usize;
    let addr_funcs = rd_u32(img, exp_rva + 28) as usize;
    let addr_names = rd_u32(img, exp_rva + 32) as usize;
    let addr_ords = rd_u32(img, exp_rva + 36) as usize;
    for i in 0..num_names {
        let name_rva = rd_u32(img, addr_names + i * 4) as usize;
        if name_rva >= img.len() {
            continue;
        }
        let mut e = name_rva;
        while e < img.len() && img[e] != 0 {
            e += 1;
        }
        if &img[name_rva..e] == name {
            let ord = rd_u16(img, addr_ords + i * 2) as usize;
            return Ok(rd_u32(img, addr_funcs + ord * 4));
        }
    }
    Err("export not found")
}
