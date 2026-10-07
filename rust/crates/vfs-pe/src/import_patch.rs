//! Rewrite a PE file so the loader loads one more DLL before every other import
//! (the Detours `setdll` technique).
//!
//! A new section is appended holding a copy of the import descriptor array with
//! one extra descriptor at the front, plus that descriptor's lookup table,
//! address table and names. The import directory is pointed at the new array.
//! No existing byte of code or data moves, so every RVA in the image stays valid.
//!
//! Also cleared, because the patch invalidates them: the bound-import directory
//! (it would name the old layout), the Authenticode certificate (the signature no
//! longer matches) and the header checksum (not checked for an EXE).

fn rd_u16(b: &[u8], o: usize) -> Result<u16, &'static str> {
    b.get(o..o + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or("read past end")
}
fn rd_u32(b: &[u8], o: usize) -> Result<u32, &'static str> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or("read past end")
}
fn wr_u16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn wr_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn align_up(v: usize, a: usize) -> usize {
    if a == 0 {
        v
    } else {
        v.div_ceil(a) * a
    }
}

const IMAGE_DIRECTORY_ENTRY_SECURITY: usize = 4;
const IMAGE_DIRECTORY_ENTRY_IMPORT: usize = 1;
const IMAGE_DIRECTORY_ENTRY_BOUND_IMPORT: usize = 11;
/// `IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE`:
/// the loader writes the new address table, so the section is writable.
const NEW_SECTION_CHARACTERISTICS: u32 = 0xC000_0040;

struct Section {
    va: usize,
    vsize: usize,
    raw_size: usize,
    raw_ptr: usize,
}

/// `raw` with `dll` imported first, by the export name `func`.
///
/// Refuses (rather than guesses) when the file carries data after its last
/// section other than an Authenticode certificate: that is an overlay some
/// programs read by absolute file offset (installers, single-file .NET
/// bundles), and the new section would have to go where it sits.
pub fn add_first_import(raw: &[u8], dll: &str, func: &str) -> Result<Vec<u8>, &'static str> {
    if raw.len() < 0x40 || &raw[..2] != b"MZ" {
        return Err("not an MZ image");
    }
    let e_lfanew = rd_u32(raw, 0x3C)? as usize;
    if raw.get(e_lfanew..e_lfanew + 4) != Some(&b"PE\0\0"[..]) {
        return Err("bad PE signature");
    }
    let n_sections = rd_u16(raw, e_lfanew + 6)? as usize;
    let size_opt = rd_u16(raw, e_lfanew + 20)? as usize;
    let opt = e_lfanew + 24;
    let pe32_plus = match rd_u16(raw, opt)? {
        0x20B => true,
        0x10B => false,
        _ => return Err("unknown optional header magic"),
    };
    let thunk = if pe32_plus { 8 } else { 4 };
    let sect_align = rd_u32(raw, opt + 32)? as usize;
    let file_align = rd_u32(raw, opt + 36)? as usize;
    let size_of_headers = rd_u32(raw, opt + 60)? as usize;
    let (n_dirs_off, dd) = if pe32_plus {
        (opt + 108, opt + 112)
    } else {
        (opt + 92, opt + 96)
    };
    let n_dirs = rd_u32(raw, n_dirs_off)? as usize;
    if n_dirs <= IMAGE_DIRECTORY_ENTRY_IMPORT {
        return Err("no import directory slot");
    }
    // A .NET IL-only image: the loader ignores the native import table.
    let clr_rva = if n_dirs > 14 { rd_u32(raw, dd + 14 * 8)? } else { 0 };
    if clr_rva != 0 {
        return Err("a .NET image (CLR header present) is not supported yet");
    }

    let sect_base = opt + size_opt;
    let mut sections = Vec::with_capacity(n_sections);
    for i in 0..n_sections {
        let s = sect_base + i * 40;
        sections.push(Section {
            vsize: rd_u32(raw, s + 8)? as usize,
            va: rd_u32(raw, s + 12)? as usize,
            raw_size: rd_u32(raw, s + 16)? as usize,
            raw_ptr: rd_u32(raw, s + 20)? as usize,
        });
    }
    let rva_to_off = |rva: usize| -> Result<usize, &'static str> {
        sections
            .iter()
            .find(|s| rva >= s.va && rva < s.va + s.raw_size.max(s.vsize))
            .map(|s| rva - s.va + s.raw_ptr)
            .ok_or("RVA outside every section")
    };

    // Room for one more section header before the first section's data. When
    // there is none (SkyrimSE.exe fills its headers exactly), the table goes at
    // the end of the last section instead, which moves nothing either.
    let new_hdr = sect_base + n_sections * 40;
    let first_raw = sections
        .iter()
        .filter(|s| s.raw_size != 0)
        .map(|s| s.raw_ptr)
        .min()
        .unwrap_or(size_of_headers);
    let header_room = new_hdr + 40 <= size_of_headers.min(first_raw);

    // Everything after the last section's data must be the certificate or nothing.
    let data_end = sections
        .iter()
        .map(|s| s.raw_ptr + s.raw_size)
        .max()
        .unwrap_or(size_of_headers);
    let (cert_off, cert_len) = if n_dirs > IMAGE_DIRECTORY_ENTRY_SECURITY {
        let o = dd + IMAGE_DIRECTORY_ENTRY_SECURITY * 8;
        (rd_u32(raw, o)? as usize, rd_u32(raw, o + 4)? as usize)
    } else {
        (0, 0)
    };
    let tail = &raw[data_end.min(raw.len())..];
    let tail_is_cert_only = tail.is_empty()
        || (cert_len != 0 && cert_off >= data_end && cert_off + cert_len >= raw.len()
            && raw[data_end..cert_off].iter().all(|&b| b == 0));
    if !tail_is_cert_only {
        return Err("data after the last section that is not a certificate (an overlay)");
    }

    // The existing descriptors, up to the all-zero terminator.
    let imp_rva = rd_u32(raw, dd + IMAGE_DIRECTORY_ENTRY_IMPORT * 8)? as usize;
    let mut old_desc = Vec::new();
    if imp_rva != 0 {
        let mut o = rva_to_off(imp_rva)?;
        loop {
            let d = raw.get(o..o + 20).ok_or("import descriptor past end")?;
            if d.iter().all(|&b| b == 0) {
                break;
            }
            old_desc.extend_from_slice(d);
            o += 20;
        }
    }
    let n_old = old_desc.len() / 20;

    // Where the table goes: a new section after the last, or the tail of the
    // last section itself.
    let last_i = (0..n_sections)
        .max_by_key(|&i| sections[i].va)
        .ok_or("no sections")?;
    let last = &sections[last_i];
    let last_virtual_end = last.va + last.vsize.max(last.raw_size);
    let extend_off = align_up(last.vsize.max(last.raw_size), 16);
    if !header_room && last.raw_ptr + last.raw_size != data_end {
        return Err("no room in the headers, and the last section is not last in the file");
    }
    let new_va = if header_room {
        align_up(last_virtual_end, sect_align)
    } else {
        last.va + extend_off
    };
    let desc_len = (n_old + 2) * 20;
    let ilt_off = align_up(desc_len, 8);
    let iat_off = ilt_off + 2 * thunk;
    let hint_off = iat_off + 2 * thunk;
    let name_off = align_up(hint_off + 2 + func.len() + 1, 2);
    let body_len = name_off + dll.len() + 1;
    let mut body = vec![0u8; body_len];
    // Our descriptor first, then the originals, then the terminator.
    wr_u32(&mut body, 0, (new_va + ilt_off) as u32); // OriginalFirstThunk
    wr_u32(&mut body, 12, (new_va + name_off) as u32); // Name
    wr_u32(&mut body, 16, (new_va + iat_off) as u32); // FirstThunk
    body[20..20 + old_desc.len()].copy_from_slice(&old_desc);
    // One import by name in both tables; the high bit clear means "by name".
    wr_u32(&mut body, ilt_off, (new_va + hint_off) as u32);
    wr_u32(&mut body, iat_off, (new_va + hint_off) as u32);
    wr_u16(&mut body, hint_off, 0);
    body[hint_off + 2..hint_off + 2 + func.len()].copy_from_slice(func.as_bytes());
    body[name_off..name_off + dll.len()].copy_from_slice(dll.as_bytes());

    let mut out = raw[..data_end.min(raw.len())].to_vec();
    if header_room {
        let new_raw_ptr = align_up(data_end, file_align);
        let new_raw_size = align_up(body_len, file_align);
        out.resize(new_raw_ptr, 0);
        out.extend_from_slice(&body);
        out.resize(new_raw_ptr + new_raw_size, 0);

        let mut h = [0u8; 40];
        h[..8].copy_from_slice(b".aether\0");
        wr_u32(&mut h, 8, body_len as u32);
        wr_u32(&mut h, 12, new_va as u32);
        wr_u32(&mut h, 16, new_raw_size as u32);
        wr_u32(&mut h, 20, new_raw_ptr as u32);
        wr_u32(&mut h, 36, NEW_SECTION_CHARACTERISTICS);
        out[new_hdr..new_hdr + 40].copy_from_slice(&h);
        wr_u16(&mut out, e_lfanew + 6, (n_sections + 1) as u16);
    } else {
        let at = last.raw_ptr + extend_off;
        let new_vsize = extend_off + body_len;
        let new_raw_size = align_up(new_vsize, file_align);
        out.resize(at, 0);
        out.extend_from_slice(&body);
        out.resize(last.raw_ptr + new_raw_size, 0);

        let h = sect_base + last_i * 40;
        wr_u32(&mut out, h + 8, new_vsize as u32);
        wr_u32(&mut out, h + 16, new_raw_size as u32);
        let ch = rd_u32(&out, h + 36)?;
        // Readable and writable: the loader writes our address table.
        wr_u32(&mut out, h + 36, ch | 0xC000_0040);
    }
    wr_u32(
        &mut out,
        opt + 56,
        align_up(new_va + body_len, sect_align) as u32,
    ); // SizeOfImage
    wr_u32(&mut out, opt + 64, 0); // CheckSum

    // Directories.
    let imp = dd + IMAGE_DIRECTORY_ENTRY_IMPORT * 8;
    wr_u32(&mut out, imp, new_va as u32);
    wr_u32(&mut out, imp + 4, desc_len as u32);
    if n_dirs > IMAGE_DIRECTORY_ENTRY_BOUND_IMPORT {
        let b = dd + IMAGE_DIRECTORY_ENTRY_BOUND_IMPORT * 8;
        wr_u32(&mut out, b, 0);
        wr_u32(&mut out, b + 4, 0);
    }
    if n_dirs > IMAGE_DIRECTORY_ENTRY_SECURITY {
        let s = dd + IMAGE_DIRECTORY_ENTRY_SECURITY * 8;
        wr_u32(&mut out, s, 0);
        wr_u32(&mut out, s + 4, 0);
    }
    Ok(out)
}

/// Raise the header's `SizeOfStackReserve` to at least `min` bytes, in place.
/// The primary thread's stack is sized from it, so a patched exe needs no
/// stack growth at launch. Returns whether it changed.
pub fn raise_stack_reserve(raw: &mut [u8], min: u64) -> Result<bool, &'static str> {
    let e_lfanew = rd_u32(raw, 0x3C)? as usize;
    let opt = e_lfanew + 24;
    let pe32_plus = rd_u16(raw, opt)? == 0x20B;
    let o = opt + 72;
    let cur = if pe32_plus {
        u64::from_le_bytes(raw.get(o..o + 8).ok_or("read past end")?.try_into().unwrap())
    } else {
        rd_u32(raw, o)? as u64
    };
    if cur >= min {
        return Ok(false);
    }
    if pe32_plus {
        raw[o..o + 8].copy_from_slice(&min.to_le_bytes());
    } else {
        wr_u32(raw, o, u32::try_from(min).map_err(|_| "reserve too large for PE32")?);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_dll_names_of_pe;

    /// A real Windows PE from the cross-build, when there is one.
    fn some_built_exe() -> Option<Vec<u8>> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
        std::fs::read(dir.join("vfs-fixture-read.exe")).ok()
    }

    #[test]
    fn the_new_dll_is_imported_first_and_the_rest_keep_their_order() {
        let Some(raw) = some_built_exe() else {
            eprintln!("SKIP: no target/debug/vfs-fixture-read.exe (bin/build-windows)");
            return;
        };
        let before = import_dll_names_of_pe(&raw).unwrap();
        let patched = add_first_import(&raw, "aether_shim.dll", "vfs_shim_activated").unwrap();
        let after = import_dll_names_of_pe(&patched).unwrap();
        assert_eq!(after[0], "aether_shim.dll");
        assert_eq!(&after[1..], &before[..]);
    }

    #[test]
    fn garbage_is_refused_without_panicking() {
        assert!(add_first_import(b"", "a.dll", "f").is_err());
        assert!(add_first_import(&[b'M', b'Z', 0, 0], "a.dll", "f").is_err());
        let mut buf = vec![0u8; 0x40];
        buf[0] = b'M';
        buf[1] = b'Z';
        buf[0x3C..0x40].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert!(add_first_import(&buf, "a.dll", "f").is_err());
    }
}
