//! Sections and views: `NtCreateSection`, `NtMapViewOfSection`, `NtUnmapViewOfSection`.

use super::{ShimIoGuard, TRAMP_CREATE_SECTION, TRAMP_MAP_VIEW, TRAMP_UNMAP_VIEW};
use crate::ntdef::{
    NtCreateSectionFn, ObjectAttributes, SEC_IMAGE, STATUS_INVALID_FILE_FOR_SECTION,
    STATUS_INVALID_HANDLE, STATUS_SECTION_TOO_BIG, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};
use core::ffi::c_void;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// Back a VFS-served PE with a real file so the kernel can build the image
/// section, and return the trampoline's status.
///
/// `None` means the backing file could not be produced, and the caller should
/// fall back to the manual mapper.
///
/// The cache is keyed on the vpath and the image's bytes
/// ([`vfs_pe::image_cache_name`]), so an assembly loaded repeatedly
/// materialises once and a changed build of the same size never reuses it.
/// Files live under the shim's own temp directory and are left for the OS to
/// reclaim; they are content, not secrets, and the process
/// may still have sections open on them at exit.
#[allow(clippy::too_many_arguments)]
unsafe fn real_image_section(
    pe: &[u8],
    file_handle: HANDLE,
    section_handle: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    max_size: *mut i64,
    page_prot: u32,
    alloc_attrs: u32,
    tramp: NtCreateSectionFn,
) -> Option<NTSTATUS> {
    use std::os::windows::io::AsRawHandle;

    // Our own file I/O must not re-enter the hooks that brought us here.
    let _io = ShimIoGuard::enter();

    let vpath = crate::fuse_synth::abs_path(file_handle as isize)?;
    // Named by the vpath and the image's bytes: a same-size different build
    // (a patch, an update) never reuses a stale copy.
    let name = vfs_pe::image_cache_name(&vpath, pe);
    let dir = std::env::temp_dir().join("vfs-pe-cache");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(&name);

    // Write once. A concurrent writer would be writing identical bytes, but a
    // reader must never see a half-written image, so build beside it and rename.
    let good =
        |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).ok() == Some(pe.len() as u64);
    if !good(&path) {
        let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, pe).ok()?;
        // Rename is atomic within a directory; an existing good file wins.
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            if !good(&path) {
                return None;
            }
        }
    }

    // GENERIC_READ | GENERIC_EXECUTE. An *image* section requires execute
    // access on the backing file; a plain `File::open` grants only read and
    // `NtCreateSection` answers STATUS_ACCESS_DENIED (0xC0000022).
    use std::os::windows::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(0x8000_0000 | 0x2000_0000)
        .share_mode(0x0000_0001 | 0x0000_0002) // FILE_SHARE_READ | FILE_SHARE_WRITE
        .open(&path)
        .ok()?;
    let st = tramp(
        section_handle,
        access,
        oa,
        max_size,
        page_prot,
        alloc_attrs,
        f.as_raw_handle() as HANDLE,
    );
    // The section holds its own reference to the file object, so closing ours
    // here (on drop) does not disturb it.
    if st < 0 {
        return None;
    }
    Some(st)
}

/// Map a FUSE synthetic file into a synthetic section.
///
/// - **SEC_IMAGE**: map PE from director bytes (rare; PEs usually host-tramped).
/// - **Data ≤256 MiB**: eager stream into a private mapping (primary stack is
///   expanded to 16 MiB by vfs-inject — matches the known-good director-only path).
/// - **Data >256 MiB**: lazy demand-page (reserve + warm + VEH) so multi‑GiB BSAs
///   never full-preload.
#[allow(clippy::too_many_arguments)]
unsafe fn fuse_create_section(
    section_handle: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    max_size: *mut i64,
    page_prot: u32,
    alloc_attrs: u32,
    file_handle: HANDLE,
    tramp: NtCreateSectionFn,
) -> NTSTATUS {
    let Some((fh, size, is_dir, _, _)) = crate::fuse_synth::lookup(file_handle as isize) else {
        return STATUS_INVALID_HANDLE;
    };
    if is_dir || size == 0 {
        return STATUS_INVALID_FILE_FOR_SECTION;
    }
    // SEC_IMAGE: map PE image from director bytes.
    if alloc_attrs & SEC_IMAGE != 0 {
        if size > 256 * 1024 * 1024 {
            return STATUS_INVALID_FILE_FOR_SECTION;
        }
        let Some(client) = crate::fuse_client::global() else {
            return STATUS_UNSUCCESSFUL;
        };
        let mut pe = vec![0u8; size as usize];
        match client.read_fragmented(fh, 0, &mut pe) {
            Ok(n) if n == pe.len() => {}
            Ok(n) if n > 0 => pe.truncate(n),
            _ => return STATUS_INVALID_FILE_FOR_SECTION,
        }
        if !vfs_pe::pe_looks_like_image(&pe) {
            return STATUS_INVALID_FILE_FOR_SECTION;
        }

        // Preferred path: write the bytes to a real file and let the kernel
        // build the image section.
        //
        // The manual mapper below reimplements what Windows does when it maps
        // a PE, and it is only ever approximately right: one `VirtualAlloc` of
        // `PAGE_EXECUTE_READWRITE` for the whole image, no per-section
        // protections, and a single shared region where a real image section
        // gives each view its own copy-on-write. A .NET application maps
        // hundreds of assemblies and the CLR faulted inside its own code
        // (`c0000005`) on that difference. A genuine section gets all of it
        // from the kernel for the price of one cached file on disk.
        if let Some(st) = real_image_section(
            &pe,
            file_handle,
            section_handle,
            access,
            oa,
            max_size,
            page_prot,
            alloc_attrs,
            tramp,
        ) {
            return st;
        }

        return match vfs_inject::map_image_from_pe_bytes_local(&pe) {
            Ok((base, img_size)) => {
                match crate::zipserve::register_mapped_image(base as usize, img_size as u64) {
                    Some(h) => {
                        if !section_handle.is_null() {
                            *section_handle = h as HANDLE;
                        }
                        STATUS_SUCCESS
                    }
                    None => STATUS_INVALID_FILE_FOR_SECTION,
                }
            }
            Err(_) => STATUS_INVALID_FILE_FOR_SECTION,
        };
    }
    if !max_size.is_null() {
        let want = core::ptr::read_unaligned(max_size);
        if want > 0 && (want as u64) > size {
            return STATUS_SECTION_TOO_BIG;
        }
    }
    // Diagnostic: `VFS_REJECT_FUSE_DATA_SECTION=1` refuses *data* sections only,
    // so the game falls back to ReadFile for content while SEC_IMAGE (DLL
    // loading) keeps working. Rejecting every section — the older
    // VFS_REJECT_FUSE_SECTION — breaks the launch outright.
    //
    // This is the one I/O path nothing else can observe: reads from a mapped
    // view are page faults served by the lazy-section VEH, so they appear in
    // neither NtReadFile nor the hook counters. Bypassing it makes that traffic
    // visible as ordinary reads.
    if vfs_env::present(vfs_env::REJECT_FUSE_DATA_SECTION) {
        return STATUS_INVALID_FILE_FOR_SECTION;
    }

    const EAGER_MAX: u64 = 256 * 1024 * 1024;
    if size > crate::lazy_section::MAX_LAZY {
        return STATUS_SECTION_TOO_BIG;
    }
    if size > EAGER_MAX {
        return match crate::lazy_section::create_lazy_data_section(fh, size) {
            Some(h) => {
                if !section_handle.is_null() {
                    *section_handle = h as HANDLE;
                }
                STATUS_SUCCESS
            }
            None => STATUS_SECTION_TOO_BIG,
        };
    }
    // Eager path (≤256 MiB): stream on this thread into VirtualAlloc.
    // Known-good with expand_primary_stack — avoid CreateThread from NtCreateSection.
    let Some(client) = crate::fuse_client::global() else {
        return STATUS_UNSUCCESSFUL;
    };
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    let map_len = size as usize;
    let base = VirtualAlloc(
        core::ptr::null(),
        map_len,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if base.is_null() {
        return STATUS_UNSUCCESSFUL;
    }
    let dest = core::slice::from_raw_parts_mut(base as *mut u8, map_len);
    let fill_ok = match client.read_fragmented(fh, 0, dest) {
        Ok(n) if n == map_len => true,
        Ok(n) if n > 0 => {
            dest[n..].fill(0);
            true
        }
        _ => false,
    };
    // Opt-in trace only: this runs inside NtCreateSection, so the file I/O
    // re-enters our own hooks on every section the game creates.
    if let Some(path) = vfs_env::raw(vfs_env::SECTION_FILL_LOG) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write;
            let _ = writeln!(f, "eager fh={fh} size={size} ok={fill_ok}");
        }
    }
    if !fill_ok {
        VirtualFree(base, 0, MEM_RELEASE);
        return STATUS_UNSUCCESSFUL;
    }
    // Track the allocation so NtClose frees it — otherwise every eager section
    // leaks up to EAGER_MAX for the life of the process.
    crate::lazy_section::track_eager_section(base as usize, size);
    match crate::zipserve::register_mapped_image(base as usize, size) {
        Some(h) => {
            if !section_handle.is_null() {
                *section_handle = h as HANDLE;
            }
            STATUS_SUCCESS
        }
        None => {
            // Reaps the tracked region (no view, no open section) — which frees
            // `base`, so do not VirtualFree it again here.
            crate::lazy_section::on_section_closed(base as usize);
            STATUS_INVALID_FILE_FOR_SECTION
        }
    }
}

/// `NtCreateSection` hook: a FUSE synthetic file handle becomes a synthetic
/// section (lazy data section, or an eagerly mapped PE for `SEC_IMAGE`) via
/// [`fuse_create_section`]. Every other handle passes through — including, as
/// of gate 4 task 7, the zip-window synthetic file handles this hook used to
/// also answer for, which no longer exist.
pub(super) unsafe fn create_section_hook_body(
    section_handle: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    max_size: *mut i64,
    page_prot: u32,
    alloc_attrs: u32,
    file_handle: HANDLE,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::CreateSection);
    let tramp = match TRAMP_CREATE_SECTION.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // FUSE synthetic file handles: lazy data section or eager SEC_IMAGE.
    // Without this, NtCreateSection fails on fake handles (game mmap of BSAs).
    if crate::fuse_synth::is_fuse_synth(file_handle as isize) {
        // Debug: VFS_REJECT_FUSE_SECTION=1 forces ReadFile path (no section map).
        if vfs_env::present(vfs_env::REJECT_FUSE_SECTION) {
            return STATUS_INVALID_FILE_FOR_SECTION;
        }
        return fuse_create_section(
            section_handle,
            access,
            oa,
            max_size,
            page_prot,
            alloc_attrs,
            file_handle,
            tramp,
        );
    }
    tramp(
        section_handle,
        access,
        oa,
        max_size,
        page_prot,
        alloc_attrs,
        file_handle,
    )
}

/// `NtMapViewOfSection` hook: synthetic sections return a pointer into the
/// region the shim already mapped for them. Real sections pass through.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn map_view_hook_body(
    section: HANDLE,
    process: HANDLE,
    base_address: *mut *mut c_void,
    zero_bits: usize,
    commit_size: usize,
    section_offset: *mut i64,
    view_size: *mut usize,
    inherit: u32,
    alloc_type: u32,
    protect: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::MapView);
    let tramp = match TRAMP_MAP_VIEW.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::zipserve::is_synth_section(section as isize) {
        // Only the current process: cross-process map of our private VA is N/A.
        let off = if section_offset.is_null() {
            0u64
        } else {
            let v = core::ptr::read_unaligned(section_offset);
            if v < 0 {
                return STATUS_UNSUCCESSFUL;
            }
            v as u64
        };
        let want = if view_size.is_null() {
            0u64
        } else {
            *view_size as u64
        };
        match crate::zipserve::map_view(section as isize, off, want) {
            Some((base, size)) => {
                if !base_address.is_null() {
                    let preferred = *base_address;
                    if !preferred.is_null() && preferred as usize != base {
                        // Caller demanded a specific VA we cannot satisfy.
                        crate::zipserve::unmap_view(base);
                        return STATUS_UNSUCCESSFUL;
                    }
                    *base_address = base as *mut c_void;
                }
                if !view_size.is_null() {
                    *view_size = size as usize;
                }
                if !section_offset.is_null() {
                    core::ptr::write_unaligned(section_offset, off as i64);
                }
                STATUS_SUCCESS
            }
            None => STATUS_UNSUCCESSFUL,
        }
    } else {
        tramp(
            section,
            process,
            base_address,
            zero_bits,
            commit_size,
            section_offset,
            view_size,
            inherit,
            alloc_type,
            protect,
        )
    }
}

/// `NtUnmapViewOfSection` hook: synthetic views are bookkeeping-only. Dropping
/// the last reference to one does not tear the memory down here — the region
/// belongs to whoever mapped it (see `lazy_section::on_section_closed`).
pub(super) unsafe fn unmap_view_hook_body(process: HANDLE, base: *mut c_void) -> NTSTATUS {
    let tramp = match TRAMP_UNMAP_VIEW.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if !base.is_null() && crate::zipserve::is_synth_view(base as usize) {
        let b = base as usize;
        // Retire one reference; the backing VA outlives it unless the section
        // handle is already closed and this was the last view — a BSA reader
        // slides views over one open section and must keep the others.
        crate::zipserve::unmap_view(b);
        crate::lazy_section::on_view_unmapped(b);
        return STATUS_SUCCESS;
    }
    tramp(process, base)
}
