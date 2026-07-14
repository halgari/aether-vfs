//! Zero-import, ntdll-only early hook payload for pre-init injection.
//!
//! Pre-init (before `LdrpInitializeProcess`) only ntdll and the EXE image are
//! mapped — kernel32/CRT are not present. This payload imports nothing; the
//! injector reflectively maps it (section copy + base relocs) and calls
//! `shim_install` with every address it needs inside [`Config`].
//!
//! Hooks NtOpenFile / NtCreateFile / NtQueryAttributesFile /
//! NtQueryFullAttributesFile and redirects object names whose final path
//! component matches a Config redirect-table suffix to the corresponding
//! backing NT path.
#![no_std]
#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicUsize, Ordering};

// ---- minimal NT ABI ----------------------------------------------------------

#[repr(C)]
pub struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *mut u16,
}

#[repr(C)]
pub struct ObjectAttributes {
    pub length: u32,
    pub root_directory: *mut c_void,
    pub object_name: *const UnicodeString,
    pub attributes: u32,
    pub security_descriptor: *const c_void,
    pub security_qos: *const c_void,
}

type Nt = i32;

type NtProtectFn = unsafe extern "system" fn(
    process: *mut c_void,
    base: *mut *mut c_void,
    size: *mut usize,
    new_protect: u32,
    old_protect: *mut u32,
) -> Nt;

type NtOpenFileFn = unsafe extern "system" fn(
    handle: *mut *mut c_void,
    access: u32,
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    share: u32,
    options: u32,
) -> Nt;

type NtQAttrFn = unsafe extern "system" fn(oa: *const ObjectAttributes, info: *mut c_void) -> Nt;

type NtCreateFileFn = unsafe extern "system" fn(
    handle: *mut *mut c_void,
    access: u32,
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    alloc_size: *const i64,
    file_attrs: u32,
    share: u32,
    disposition: u32,
    options: u32,
    ea: *const c_void,
    ea_len: u32,
) -> Nt;

/// One path-suffix → backing redirect. Lengths are UTF-16 code units excluding NUL.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RedirectEntry {
    pub suffix_ptr: usize,
    pub suffix_wlen: u32,
    pub backing_ptr: usize,
    pub backing_wlen: u32,
    pub backing_size: u64,
}

pub const MAX_REDIRECTS: usize = 4;

/// Everything the payload needs, supplied by the injector. Field layout must
/// match the injector's Config byte-for-byte.
#[repr(C)]
pub struct Config {
    pub nt_protect: usize,

    pub open_target: usize,
    pub open_tramp: usize,

    pub qattr_target: usize,
    pub qattr_tramp: usize,

    pub qfull_target: usize,
    pub qfull_tramp: usize,

    pub create_target: usize,
    pub create_tramp: usize,

    pub install_mask: u32,
    pub redirect_count: u32,
    pub redirects: [RedirectEntry; MAX_REDIRECTS],

    /// `*mut [u32; …]` hit tallies / diagnostics (optional; 0 = disabled).
    pub counters: usize,

    /// Full-shim secondary dispatch (0 until `install_late` publishes them).
    /// When non-zero, unmatched opens/attrs are forwarded here instead of the
    /// original ntdll trampoline — so the Engine rides on early-owned stubs.
    pub secondary_open: usize,
    pub secondary_create: usize,
    pub secondary_qattr: usize,
    pub secondary_qfull: usize,
}

static CONFIG: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn cfg() -> &'static Config {
    unsafe { &*(CONFIG.load(Ordering::Acquire) as *const Config) }
}

// ---- freestanding helpers ----------------------------------------------------

#[inline]
fn lc(c: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&c) {
        c + 32
    } else {
        c
    }
}

/// Does the UNICODE_STRING at `oa` end with `suffix` (case-insensitive), with a
/// path separator (or start) immediately before the match?
unsafe fn ends_with_suffix(oa: *const ObjectAttributes, suffix_ptr: usize, suffix_wlen: u32) -> bool {
    if oa.is_null() || suffix_ptr == 0 || suffix_wlen == 0 {
        return false;
    }
    let name = (*oa).object_name;
    if name.is_null() || (*name).buffer.is_null() {
        return false;
    }
    let buf = (*name).buffer;
    let n = (*name).length as usize / 2;
    let sn = suffix_wlen as usize;
    if n < sn {
        return false;
    }
    let start = n - sn;
    let suf = suffix_ptr as *const u16;
    let mut i = 0;
    while i < sn {
        if lc(*buf.add(start + i)) != lc(*suf.add(i)) {
            return false;
        }
        i += 1;
    }
    if start > 0 {
        let prev = *buf.add(start - 1);
        if prev != b'\\' as u16 && prev != b'/' as u16 {
            return false;
        }
    }
    true
}

/// Find the first redirect entry matching `oa`, if any.
unsafe fn match_redirect(oa: *const ObjectAttributes) -> Option<&'static RedirectEntry> {
    let c = cfg();
    let n = c.redirect_count as usize;
    let lim = if n > MAX_REDIRECTS { MAX_REDIRECTS } else { n };
    let mut i = 0;
    while i < lim {
        let e = &c.redirects[i];
        if ends_with_suffix(oa, e.suffix_ptr, e.suffix_wlen) {
            return Some(e);
        }
        i += 1;
    }
    None
}

unsafe fn redirect_oa(entry: &RedirectEntry) -> (ObjectAttributes, UnicodeString) {
    const OBJ_CASE_INSENSITIVE: u32 = 0x40;
    let blen = (entry.backing_wlen * 2) as u16;
    let us = UnicodeString {
        length: blen,
        maximum_length: blen,
        buffer: entry.backing_ptr as *mut u16,
    };
    let oa = ObjectAttributes {
        length: core::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: core::ptr::null_mut(),
        object_name: core::ptr::null(),
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: core::ptr::null(),
        security_qos: core::ptr::null(),
    };
    (oa, us)
}

// ---- hooks -------------------------------------------------------------------

unsafe fn bump(idx: usize) {
    let p = cfg().counters;
    if p != 0 {
        let arr = p as *mut u32;
        let v = core::ptr::read_volatile(arr.add(idx));
        core::ptr::write_volatile(arr.add(idx), v.wrapping_add(1));
    }
}

unsafe fn record_status(st: Nt) {
    let p = cfg().counters;
    if p != 0 {
        let arr = p as *mut u32;
        core::ptr::write_volatile(arr.add(5), st as u32);
        if st < 0 {
            let v = core::ptr::read_volatile(arr.add(6));
            core::ptr::write_volatile(arr.add(6), v.wrapping_add(1));
        }
    }
}

unsafe extern "system" fn create_hook(
    handle: *mut *mut c_void,
    access: u32,
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    alloc_size: *const i64,
    file_attrs: u32,
    share: u32,
    disposition: u32,
    options: u32,
    ea: *const c_void,
    ea_len: u32,
) -> Nt {
    bump(3);
    let c = cfg();
    let orig: NtCreateFileFn = core::mem::transmute(c.create_tramp);
    if let Some(entry) = match_redirect(oa) {
        bump(4);
        let (mut new_oa, us) = redirect_oa(entry);
        new_oa.object_name = &us;
        let st = orig(
            handle, access, &new_oa, iosb, alloc_size, file_attrs, share, disposition, options, ea,
            ea_len,
        );
        record_status(st);
        return st;
    }
    if c.secondary_create != 0 {
        let sec: NtCreateFileFn = core::mem::transmute(c.secondary_create);
        return sec(
            handle, access, oa, iosb, alloc_size, file_attrs, share, disposition, options, ea,
            ea_len,
        );
    }
    orig(
        handle, access, oa, iosb, alloc_size, file_attrs, share, disposition, options, ea, ea_len,
    )
}

unsafe extern "system" fn open_hook(
    handle: *mut *mut c_void,
    access: u32,
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    share: u32,
    options: u32,
) -> Nt {
    bump(2);
    let c = cfg();
    let orig: NtOpenFileFn = core::mem::transmute(c.open_tramp);
    if let Some(entry) = match_redirect(oa) {
        bump(4);
        let (mut new_oa, us) = redirect_oa(entry);
        new_oa.object_name = &us;
        let st = orig(handle, access, &new_oa, iosb, share, options);
        record_status(st);
        return st;
    }
    if c.secondary_open != 0 {
        let sec: NtOpenFileFn = core::mem::transmute(c.secondary_open);
        return sec(handle, access, oa, iosb, share, options);
    }
    orig(handle, access, oa, iosb, share, options)
}

const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

unsafe extern "system" fn qattr_hook(oa: *const ObjectAttributes, info: *mut c_void) -> Nt {
    bump(1);
    let c = cfg();
    if let Some(_entry) = match_redirect(oa) {
        if !info.is_null() {
            bump(4);
            let b = info as *mut u8;
            let mut i = 0;
            while i < 40 {
                core::ptr::write_volatile(b.add(i), 0);
                i += 1;
            }
            core::ptr::write_volatile(b.add(32) as *mut u32, FILE_ATTRIBUTE_NORMAL);
            return 0;
        }
    }
    if c.secondary_qattr != 0 {
        let sec: NtQAttrFn = core::mem::transmute(c.secondary_qattr);
        return sec(oa, info);
    }
    let orig: NtQAttrFn = core::mem::transmute(c.qattr_tramp);
    orig(oa, info)
}

unsafe extern "system" fn qfull_hook(oa: *const ObjectAttributes, info: *mut c_void) -> Nt {
    bump(0);
    let c = cfg();
    if let Some(entry) = match_redirect(oa) {
        if !info.is_null() {
            bump(4);
            let b = info as *mut u8;
            let mut i = 0;
            while i < 56 {
                core::ptr::write_volatile(b.add(i), 0);
                i += 1;
            }
            core::ptr::write_volatile(b.add(32) as *mut i64, entry.backing_size as i64);
            core::ptr::write_volatile(b.add(40) as *mut i64, entry.backing_size as i64);
            core::ptr::write_volatile(b.add(48) as *mut u32, FILE_ATTRIBUTE_NORMAL);
            return 0;
        }
    }
    if c.secondary_qfull != 0 {
        let sec: NtQAttrFn = core::mem::transmute(c.secondary_qfull);
        return sec(oa, info);
    }
    let orig: NtQAttrFn = core::mem::transmute(c.qfull_tramp);
    orig(oa, info)
}

// ---- inline patch install ----------------------------------------------------

fn abs_jmp(dst: *mut u8, target: usize) {
    let bytes: [u8; 6] = [0xFF, 0x25, 0x00, 0x00, 0x00, 0x00];
    unsafe {
        let mut i = 0;
        while i < 6 {
            core::ptr::write_volatile(dst.add(i), bytes[i]);
            i += 1;
        }
        let t = target as u64;
        let mut j = 0;
        while j < 8 {
            core::ptr::write_volatile(dst.add(6 + j), (t >> (j * 8)) as u8);
            j += 1;
        }
    }
}

unsafe fn build_trampoline(target: usize, tramp: usize) {
    const STOLEN: usize = 16;
    let t = target as *const u8;
    let tr = tramp as *mut u8;
    let mut i = 0;
    while i < STOLEN {
        core::ptr::write_volatile(tr.add(i), core::ptr::read_volatile(t.add(i)));
        i += 1;
    }
    abs_jmp(tr.add(STOLEN), target + STOLEN);
}

unsafe fn hook_one(nt_protect: NtProtectFn, target: usize, tramp: usize, hook: usize) {
    build_trampoline(target, tramp);

    const PAGE_EXECUTE_READWRITE: u32 = 0x40;
    let cur = usize::MAX as *mut c_void;

    let mut base = target as *mut c_void;
    let mut size: usize = 16;
    let mut old: u32 = 0;
    nt_protect(cur, &mut base, &mut size, PAGE_EXECUTE_READWRITE, &mut old);

    abs_jmp(target as *mut u8, hook);

    let mut base2 = target as *mut c_void;
    let mut size2: usize = 16;
    let mut old2: u32 = 0;
    nt_protect(cur, &mut base2, &mut size2, old, &mut old2);
}

/// Entry the injector calls (reflectively-mapped address) to arm the hooks.
/// Returns 0 on success.
#[no_mangle]
pub unsafe extern "system" fn shim_install(config: *const Config) -> u32 {
    if config.is_null() {
        return 1;
    }
    CONFIG.store(config as usize, Ordering::Release);
    let c = &*config;
    let nt_protect: NtProtectFn = core::mem::transmute(c.nt_protect);

    let m = c.install_mask;
    let qf = qfull_hook as *const () as usize;
    let qa = qattr_hook as *const () as usize;
    let op = open_hook as *const () as usize;
    let cr = create_hook as *const () as usize;
    if m & 1 != 0 {
        hook_one(nt_protect, c.qfull_target, c.qfull_tramp, qf);
    }
    if m & 2 != 0 {
        hook_one(nt_protect, c.qattr_target, c.qattr_tramp, qa);
    }
    if m & 4 != 0 {
        hook_one(nt_protect, c.open_target, c.open_tramp, op);
    }
    if m & 8 != 0 {
        hook_one(nt_protect, c.create_target, c.create_tramp, cr);
    }
    0
}

// ---- freestanding runtime glue ----------------------------------------------

#[no_mangle]
pub extern "system" fn DllMain(_h: *mut c_void, _reason: u32, _reserved: *mut c_void) -> i32 {
    1
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub static _fltused: i32 = 0;

#[no_mangle]
pub extern "C" fn __CxxFrameHandler3() -> i32 {
    0
}

#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
        i += 1;
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if (dst as usize) < (src as usize) {
        let mut i = 0;
        while i < n {
            core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
            i += 1;
        }
    } else {
        let mut i = n;
        while i > 0 {
            i -= 1;
            core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i)));
        }
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut u8, val: i32, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        core::ptr::write_volatile(dst.add(i), val as u8);
        i += 1;
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let d = core::ptr::read_volatile(a.add(i)) as i32 - core::ptr::read_volatile(b.add(i)) as i32;
        if d != 0 {
            return d;
        }
        i += 1;
    }
    0
}
