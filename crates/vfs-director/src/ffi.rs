//! C ABI for the userspace FUSE director. All `unsafe` for this crate lives here.

#![allow(unsafe_code)]

use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::Arc;

use crate::director::Director;
use crate::ops::{Backend, BackendHandle, DirEntry, Stat, KIND_DIR, KIND_FILE};

#[repr(C)]
pub struct VfsStat {
    pub kind: u8,
    pub size: u64,
    pub mtime: i64,
}

#[repr(C)]
pub struct VfsBackendOps {
    pub getattr: Option<
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut VfsStat) -> c_int,
    >,
    pub readdir: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            *mut c_void,
            Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const VfsStat) -> c_int>,
        ) -> c_int,
    >,
    pub open: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            u32,
            *mut u64,
            *mut u64,
            *mut u8,
        ) -> c_int,
    >,
    pub read: Option<
        unsafe extern "C" fn(*mut c_void, u64, u64, *mut u8, u32, *mut u32) -> c_int,
    >,
    pub release: Option<unsafe extern "C" fn(*mut c_void, u64) -> c_int>,
}

struct CBackend {
    ops: VfsBackendOps,
    userdata: *mut c_void,
}

// SAFETY: host guarantees userdata is valid for the mount lifetime and ops are thread-safe.
unsafe impl Send for CBackend {}
unsafe impl Sync for CBackend {}

impl Backend for CBackend {
    fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
        let f = self.ops.getattr.ok_or(vfs_protocol::ST_BAD_REQUEST)?;
        let cpath = std::ffi::CString::new(path).map_err(|_| vfs_protocol::ST_BAD_REQUEST)?;
        let mut st = VfsStat {
            kind: 0,
            size: 0,
            mtime: 0,
        };
        let rc = unsafe { f(self.userdata, cpath.as_ptr(), &mut st) };
        if rc == vfs_protocol::ST_NOT_FOUND {
            return Ok(None);
        }
        if rc != 0 {
            return Err(rc);
        }
        Ok(Some(Stat {
            kind: st.kind,
            size: st.size,
            mtime: st.mtime,
        }))
    }

    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let f = self.ops.readdir.ok_or(vfs_protocol::ST_BAD_REQUEST)?;
        let cpath = std::ffi::CString::new(path).map_err(|_| vfs_protocol::ST_BAD_REQUEST)?;
        let mut acc: Vec<DirEntry> = Vec::new();
        struct Ctx {
            acc: *mut Vec<DirEntry>,
        }
        unsafe extern "C" fn fill(
            ctx: *mut c_void,
            name: *const c_char,
            st: *const VfsStat,
        ) -> c_int {
            if ctx.is_null() || name.is_null() || st.is_null() {
                return -1;
            }
            let ctx = &*(ctx as *const Ctx);
            let name = match CStr::from_ptr(name).to_str() {
                Ok(s) => s.to_string(),
                Err(_) => return -1,
            };
            let st = &*st;
            (*ctx.acc).push(DirEntry {
                name,
                stat: Stat {
                    kind: st.kind,
                    size: st.size,
                    mtime: st.mtime,
                },
            });
            0
        }
        let mut ctx = Ctx {
            acc: &mut acc as *mut _,
        };
        let rc = unsafe {
            f(
                self.userdata,
                cpath.as_ptr(),
                &mut ctx as *mut _ as *mut c_void,
                Some(fill),
            )
        };
        if rc != 0 {
            return Err(rc);
        }
        Ok(acc)
    }

    fn open(&self, path: &str, flags: u32) -> Result<(BackendHandle, u64, bool), i32> {
        let f = self.ops.open.ok_or(vfs_protocol::ST_BAD_REQUEST)?;
        let cpath = std::ffi::CString::new(path).map_err(|_| vfs_protocol::ST_BAD_REQUEST)?;
        let mut bh = 0u64;
        let mut size = 0u64;
        let mut is_dir = 0u8;
        let rc = unsafe {
            f(
                self.userdata,
                cpath.as_ptr(),
                flags,
                &mut bh,
                &mut size,
                &mut is_dir,
            )
        };
        if rc != 0 {
            return Err(rc);
        }
        Ok((bh, size, is_dir != 0))
    }

    fn read(&self, bh: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let f = self.ops.read.ok_or(vfs_protocol::ST_BAD_REQUEST)?;
        let mut nread = 0u32;
        let rc = unsafe {
            f(
                self.userdata,
                bh,
                offset,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut nread,
            )
        };
        if rc != 0 {
            return Err(rc);
        }
        Ok(nread as usize)
    }

    fn release(&self, bh: BackendHandle) -> Result<(), i32> {
        if let Some(f) = self.ops.release {
            let rc = unsafe { f(self.userdata, bh) };
            if rc != 0 {
                return Err(rc);
            }
        }
        Ok(())
    }
}

fn cstr<'a>(p: *const c_char) -> Result<&'a str, i32> {
    if p.is_null() {
        return Err(vfs_protocol::ST_BAD_REQUEST);
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| vfs_protocol::ST_BAD_REQUEST)
}

#[no_mangle]
pub extern "C" fn vfs_director_create() -> *mut Director {
    Box::into_raw(Box::new(Director::new()))
}

#[no_mangle]
pub extern "C" fn vfs_director_destroy(d: *mut Director) {
    if d.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(d));
    }
}

#[no_mangle]
pub extern "C" fn vfs_director_mount(
    d: *mut Director,
    prefix: *const c_char,
    ops: *const VfsBackendOps,
    userdata: *mut c_void,
) -> c_int {
    if d.is_null() || ops.is_null() {
        return vfs_protocol::ST_BAD_REQUEST;
    }
    let director = unsafe { &*d };
    let prefix = match cstr(prefix) {
        Ok(s) => s,
        Err(e) => return e,
    };
    // Copy function pointers (struct is not Copy if we add non-Copy fields later).
    let ops = unsafe {
        VfsBackendOps {
            getattr: (*ops).getattr,
            readdir: (*ops).readdir,
            open: (*ops).open,
            read: (*ops).read,
            release: (*ops).release,
        }
    };
    let backend = Arc::new(CBackend { ops, userdata });
    match director.mount(prefix, backend) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

#[no_mangle]
pub extern "C" fn vfs_getattr(d: *mut Director, path: *const c_char, out: *mut VfsStat) -> c_int {
    if d.is_null() || out.is_null() {
        return vfs_protocol::ST_BAD_REQUEST;
    }
    let director = unsafe { &*d };
    let path = match cstr(path) {
        Ok(s) => s,
        Err(e) => return e,
    };
    match director.getattr(path) {
        Ok(Some(s)) => {
            unsafe {
                *out = VfsStat {
                    kind: s.kind,
                    size: s.size,
                    mtime: s.mtime,
                };
            }
            0
        }
        Ok(None) => vfs_protocol::ST_NOT_FOUND,
        Err(e) => e,
    }
}

struct FillState {
    fill: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const VfsStat) -> c_int>,
    ctx: *mut c_void,
}

#[no_mangle]
pub extern "C" fn vfs_readdir(
    d: *mut Director,
    path: *const c_char,
    fill_ctx: *mut c_void,
    fill: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const VfsStat) -> c_int>,
) -> c_int {
    if d.is_null() {
        return vfs_protocol::ST_BAD_REQUEST;
    }
    let director = unsafe { &*d };
    let path = match cstr(path) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let entries = match director.readdir(path) {
        Ok(e) => e,
        Err(e) => return e,
    };
    let Some(fill) = fill else {
        return 0;
    };
    for e in entries {
        let name = match std::ffi::CString::new(e.name.as_str()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let st = VfsStat {
            kind: e.stat.kind,
            size: e.stat.size,
            mtime: e.stat.mtime,
        };
        let rc = unsafe { fill(fill_ctx, name.as_ptr(), &st) };
        if rc != 0 {
            break;
        }
    }
    let _ = FillState { fill: Some(fill), ctx: fill_ctx };
    0
}

#[no_mangle]
pub extern "C" fn vfs_open(
    d: *mut Director,
    path: *const c_char,
    flags: u32,
    fh_out: *mut u64,
    size_out: *mut u64,
    is_dir_out: *mut u8,
) -> c_int {
    if d.is_null() || fh_out.is_null() || size_out.is_null() || is_dir_out.is_null() {
        return vfs_protocol::ST_BAD_REQUEST;
    }
    let director = unsafe { &*d };
    let path = match cstr(path) {
        Ok(s) => s,
        Err(e) => return e,
    };
    match director.open(path, flags) {
        Ok((fh, size, is_dir)) => {
            unsafe {
                *fh_out = fh;
                *size_out = size;
                *is_dir_out = if is_dir { 1 } else { 0 };
            }
            0
        }
        Err(e) => e,
    }
}

#[no_mangle]
pub extern "C" fn vfs_read(
    d: *mut Director,
    fh: u64,
    offset: u64,
    buf: *mut u8,
    len: u32,
    nread: *mut u32,
) -> c_int {
    if d.is_null() || buf.is_null() || nread.is_null() {
        return vfs_protocol::ST_BAD_REQUEST;
    }
    let director = unsafe { &*d };
    let slice = unsafe { std::slice::from_raw_parts_mut(buf, len as usize) };
    match director.read(fh, offset, slice) {
        Ok(n) => {
            unsafe {
                *nread = n as u32;
            }
            0
        }
        Err(e) => e,
    }
}

#[no_mangle]
pub extern "C" fn vfs_close(d: *mut Director, fh: u64) -> c_int {
    if d.is_null() {
        return vfs_protocol::ST_BAD_REQUEST;
    }
    let director = unsafe { &*d };
    match director.close(fh) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

// Silence unused import warnings for kind constants used by C consumers only.
const _: (u8, u8) = (KIND_FILE, KIND_DIR);
