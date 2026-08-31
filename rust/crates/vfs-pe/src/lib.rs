//! Pure PE byte parsing.

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// True for a PE32+ (64-bit) optional header, false for PE32 (32-bit).
///
/// Both appear in a .NET application. Native and ReadyToRun images are PE32+,
/// but a **pure-IL assembly is emitted as PE32 even on x64** — the CLR consumes
/// it as metadata, not as native code, so the 32-bit header is not a statement
/// about the machine. `System.Runtime.dll` and the other framework facades are
/// exactly this.
pub fn is_pe32_plus(img: &[u8], e_lfanew: usize) -> bool {
    let opt = e_lfanew + 24;
    opt + 2 <= img.len() && rd_u16(img, opt) == 0x20B
}

/// Offset of the optional header's `DataDirectory` array.
///
/// The two formats differ only in the fixed part that precedes it: 112 bytes
/// for PE32+, 96 for PE32. Assuming 112 on a PE32 image reads a neighbouring
/// field as a directory RVA and quietly produces nonsense.
pub fn dd_base(img: &[u8], e_lfanew: usize) -> usize {
    let opt = e_lfanew + 24;
    if is_pe32_plus(img, e_lfanew) {
        opt + 112
    } else {
        opt + 96
    }
}

/// Build a flat in-memory image (SizeOfImage bytes) from a PE file: copy
/// headers, place each section at its VirtualAddress. Returns
/// `(image, preferred_image_base, e_lfanew)`.
pub fn build_image(raw: &[u8]) -> Result<(Vec<u8>, u64, usize), &'static str> {
    if raw.len() < 0x40 {
        return Err("PE too small");
    }
    let e_lfanew = rd_u32(raw, 0x3C) as usize;
    // 96, not 112: a PE32 optional header is shorter, and demanding the PE32+
    // length here rejected small IL assemblies before the magic was even read.
    if e_lfanew + 24 + 96 > raw.len() {
        return Err("bad e_lfanew");
    }
    if &raw[e_lfanew..e_lfanew + 4] != b"PE\0\0" {
        return Err("bad PE sig");
    }
    let opt = e_lfanew + 24;
    let magic = rd_u16(raw, opt);
    if magic != 0x20B && magic != 0x10B {
        return Err("not a PE32 or PE32+ optional header");
    }
    let pe32_plus = magic == 0x20B;
    if pe32_plus && e_lfanew + 24 + 112 > raw.len() {
        return Err("bad e_lfanew");
    }
    // SizeOfImage and SizeOfHeaders sit at the same offsets in both formats;
    // ImageBase does not — PE32+ has 8 bytes at 24, PE32 has 4 at 28, because
    // PE32 spends 24..28 on BaseOfData.
    let size_of_image = rd_u32(raw, opt + 56) as usize;
    let size_of_headers = rd_u32(raw, opt + 60) as usize;
    let image_base = if pe32_plus {
        rd_u64(raw, opt + 24)
    } else {
        rd_u32(raw, opt + 28) as u64
    };
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

/// Apply `IMAGE_REL_BASED_DIR64` base relocations in-place for a new load base.
///
/// **PE32 images are left alone, deliberately.** Their fixups are
/// `IMAGE_REL_BASED_HIGHLOW`, a 32-bit field, and the only PE32 images that
/// reach this mapper are pure-IL assemblies that get placed wherever
/// `VirtualAlloc` finds room — far above 4 GiB. The delta does not fit, and
/// applying it truncated corrupts the image: doing exactly that turned a clean
/// load failure into an access violation inside the CLR. Nothing executes a
/// PE32 image here — the runtime reads it as metadata by RVA — so leaving it
/// unrelocated is correct, and is what Windows does when mapping an image for
/// data rather than for execution.
pub fn apply_relocs(img: &mut [u8], e_lfanew: usize, image_base: u64, new_base: u64) {
    if !is_pe32_plus(img, e_lfanew) {
        return;
    }
    let reloc_dir = dd_base(img, e_lfanew) + 5 * 8; // DataDirectory[BASERELOC]
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

/// Collect import DLL names from a flat PE image (ANSI, no path).
pub fn import_dll_names(img: &[u8], e_lfanew: usize) -> Vec<String> {
    let imp_dir = dd_base(img, e_lfanew) + 8;
    let mut out = Vec::new();
    if imp_dir + 8 > img.len() {
        return out;
    }
    let mut desc = rd_u32(img, imp_dir) as usize;
    let imp_size = rd_u32(img, imp_dir + 4) as usize;
    if desc == 0 || imp_size == 0 {
        return out;
    }
    let desc_end = desc + imp_size;
    while desc + 20 <= img.len() && desc < desc_end {
        let name_rva = rd_u32(img, desc + 12) as usize;
        if name_rva == 0 {
            break;
        }
        if name_rva >= img.len() {
            break;
        }
        let mut end = name_rva;
        while end < img.len() && img[end] != 0 {
            end += 1;
        }
        out.push(String::from_utf8_lossy(&img[name_rva..end]).into_owned());
        desc += 20;
    }
    out
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

pub fn is_system_import_dll(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    // Not `Path::file_name()`: that treats `\` as a separator only on Windows,
    // so the same import table would classify differently per host. A PE names
    // its imports with Windows conventions no matter who reads the file, so
    // both separators are split here explicitly.
    let base = n.rsplit(['/', '\\']).next().unwrap_or(&n);
    base.starts_with("api-ms-")
        || base.starts_with("ext-ms-")
        || matches!(
            base,
            "kernel32.dll"
                | "kernelbase.dll"
                | "ntdll.dll"
                | "user32.dll"
                | "gdi32.dll"
                | "gdi32full.dll"
                | "advapi32.dll"
                | "shell32.dll"
                | "ole32.dll"
                | "oleaut32.dll"
                | "ws2_32.dll"
                | "winhttp.dll"
                | "winmm.dll"
                | "setupapi.dll"
                | "hid.dll"
                | "d3d11.dll"
                | "dxgi.dll"
                | "dinput8.dll"
                | "xinput1_3.dll"
                | "xinput1_4.dll"
                | "x3daudio1_7.dll"
                | "msvcp140.dll"
                | "vcruntime140.dll"
                | "vcruntime140_1.dll"
                | "ucrtbase.dll"
                | "sechost.dll"
                | "rpcrt4.dll"
                | "combase.dll"
                | "shlwapi.dll"
                | "version.dll"
                | "imm32.dll"
                | "dwmapi.dll"
                | "uxtheme.dll"
                | "bcrypt.dll"
                | "bcryptprimitives.dll"
                | "crypt32.dll"
                | "wintrust.dll"
                | "psapi.dll"
                | "userenv.dll"
                | "dbghelp.dll"
        )
}

pub fn pe_looks_like_image(pe: &[u8]) -> bool {
    pe.len() >= 0x40 && pe[0] == b'M' && pe[1] == b'Z'
}

/// Import DLL names of a raw PE, flattening the image first.
pub fn import_dll_names_of_pe(raw: &[u8]) -> Option<Vec<String>> {
    let (img, _base, e_lfanew) = build_image(raw).ok()?;
    Some(import_dll_names(&img, e_lfanew))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-byte buffer starting "MZ" is the minimum this predicate accepts.
    /// It is a cheap gate, not validation: `build_image` does the real checks.
    #[test]
    fn mz_magic_and_minimum_length_gate_the_image() {
        let mut buf = vec![0u8; 0x40];
        buf[0] = b'M';
        buf[1] = b'Z';
        assert!(pe_looks_like_image(&buf));

        assert!(!pe_looks_like_image(&buf[..0x3F]), "under 0x40 is rejected");

        buf[0] = b'X';
        assert!(!pe_looks_like_image(&buf), "wrong magic is rejected");
    }

    /// Classification is by file name only and is case- and path-insensitive,
    /// because import tables spell system DLLs inconsistently.
    ///
    /// The backslash case is the one that matters here: it must hold on Linux
    /// too, which is why the implementation splits separators explicitly instead
    /// of asking `std::path` — `Path::file_name()` would return the whole string
    /// on a non-Windows host and classify a system DLL as a game-local one.
    #[test]
    fn system_dll_classification_ignores_case_and_directory() {
        assert!(is_system_import_dll("KERNEL32.dll"));
        assert!(is_system_import_dll("kernel32.dll"));
        assert!(is_system_import_dll("C:\\Windows\\System32\\kernel32.dll"));
        assert!(is_system_import_dll("System32/kernel32.dll"));
        assert!(!is_system_import_dll("steam_api64.dll"));
        assert!(!is_system_import_dll("C:\\game\\steam_api64.dll"));
    }

    /// A truncated buffer must return Err, never panic: `build_image` is the
    /// first thing that touches attacker-influenced bytes on the staging path.
    #[test]
    fn build_image_rejects_a_truncated_header_without_panicking() {
        assert!(build_image(&[]).is_err());
        assert!(build_image(b"MZ").is_err());

        let mut buf = vec![0u8; 0x40];
        buf[0] = b'M';
        buf[1] = b'Z';
        // e_lfanew points past the end of the buffer.
        buf[0x3C..0x40].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert!(build_image(&buf).is_err());
    }

    /// An import directory of zero yields no names rather than an error.
    #[test]
    fn import_dll_names_is_empty_when_there_is_no_import_directory() {
        let img = vec![0u8; 0x400];
        assert!(import_dll_names(&img, 0x80).is_empty());
    }
}
