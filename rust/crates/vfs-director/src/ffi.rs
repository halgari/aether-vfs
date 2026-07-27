//! C ABI — configure session, serve IPC, **vfs_launch** (primary); open/read optional.
//!
//! # Threading
//! The C API is **single-threaded per session**: do not call `vfs_*` on the same
//! `vfs_director*` from multiple threads. Ring workers call backend ops; C
//! backends must ensure `userdata` is safe for concurrent worker access
//! (`Send + Sync` equivalent).
//!
//! # Errors
//! Failures return `vfs-protocol` status codes (`VFS_ERR_*` in the header).
//! Detailed messages are not exported yet (use host logging around Rust wrappers).

#![allow(unsafe_code)]

use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::Arc;

use crate::ops::{Backend, BackendHandle, DirEntry, Stat};
use crate::session::{LaunchOpts, Session};

#[repr(C)]
pub struct VfsStat {
    pub kind: u8,
    pub size: u64,
    pub mtime: i64,
}

#[repr(C)]
pub struct VfsBackendOps {
    pub getattr: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *mut VfsStat) -> c_int>,
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
    pub read:
        Option<unsafe extern "C" fn(*mut c_void, u64, u64, *mut u8, u32, *mut u32) -> c_int>,
    pub release: Option<unsafe extern "C" fn(*mut c_void, u64) -> c_int>,
}

#[repr(C)]
pub struct VfsLaunchOpts {
    pub image: *const c_char,
    pub argv: *const *const c_char,
    pub argc: c_int,
    pub wait: c_int,
    pub hollow_pe: c_int,
    pub shim_dll: *const c_char,
    pub payload_dll: *const c_char,
}

struct CBackend {
    ops: VfsBackendOps,
    userdata: *mut c_void,
}

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

fn session<'a>(d: *mut Session) -> Result<&'a mut Session, i32> {
    if d.is_null() {
        return Err(vfs_protocol::ST_BAD_REQUEST);
    }
    Ok(unsafe { &mut *d })
}

#[no_mangle]
pub extern "C" fn vfs_director_create() -> *mut Session {
    Box::into_raw(Box::new(Session::new()))
}

#[no_mangle]
pub extern "C" fn vfs_director_destroy(d: *mut Session) {
    if d.is_null() {
        return;
    }
    unsafe {
        let mut s = Box::from_raw(d);
        s.stop_serve();
        drop(s);
    }
}

#[no_mangle]
pub extern "C" fn vfs_director_set_root(d: *mut Session, path: *const c_char) -> c_int {
    let s = match session(d) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = match cstr(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    s.set_root(path);
    0
}

#[no_mangle]
pub extern "C" fn vfs_director_set_overlay(d: *mut Session, path: *const c_char) -> c_int {
    let s = match session(d) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = match cstr(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    s.set_overlay(path);
    0
}

#[no_mangle]
pub extern "C" fn vfs_director_set_state_dir(d: *mut Session, path: *const c_char) -> c_int {
    let s = match session(d) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = match cstr(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    s.set_state_dir(path);
    0
}

#[no_mangle]
pub extern "C" fn vfs_director_mount(
    d: *mut Session,
    prefix: *const c_char,
    ops: *const VfsBackendOps,
    userdata: *mut c_void,
) -> c_int {
    let s = match session(d) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if ops.is_null() {
        return vfs_protocol::ST_BAD_REQUEST;
    }
    let prefix = match cstr(prefix) {
        Ok(p) => p,
        Err(e) => return e,
    };
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
    match s.mount(prefix, backend) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/// Mount a Stored zip as a backend (`prefix` is always root for zip layers).
/// Requires the crate `zip` feature (default).
#[cfg(feature = "zip")]
#[no_mangle]
pub extern "C" fn vfs_director_mount_zip(d: *mut Session, zip_path: *const c_char) -> c_int {
    let s = match session(d) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = match cstr(zip_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match s.mount_zip(path) {
        Ok(()) => 0,
        Err(_) => vfs_protocol::ST_IO_ERROR,
    }
}

#[no_mangle]
pub extern "C" fn vfs_director_serve(d: *mut Session) -> c_int {
    let s = match session(d) {
        Ok(s) => s,
        Err(e) => return e,
    };
    match s.serve() {
        Ok(()) => 0,
        Err(_) => vfs_protocol::ST_IO_ERROR,
    }
}

#[no_mangle]
pub extern "C" fn vfs_launch(
    d: *mut Session,
    opts: *const VfsLaunchOpts,
    exit_code: *mut i32,
) -> c_int {
    let s = match session(d) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if opts.is_null() {
        return vfs_protocol::ST_BAD_REQUEST;
    }
    let o = unsafe { &*opts };
    let image = match cstr(o.image) {
        Ok(p) => p.to_string(),
        Err(e) => return e,
    };
    let mut args = Vec::new();
    if !o.argv.is_null() && o.argc > 0 {
        for i in 0..o.argc as isize {
            let p = unsafe { *o.argv.offset(i) };
            if let Ok(a) = cstr(p) {
                args.push(a.to_string());
            }
        }
    }
    let shim = if o.shim_dll.is_null() {
        None
    } else {
        cstr(o.shim_dll).ok().map(|s| s.to_string())
    };
    let payload = if o.payload_dll.is_null() {
        None
    } else {
        cstr(o.payload_dll).ok().map(|s| s.to_string())
    };
    let launch = LaunchOpts {
        image,
        args,
        wait: o.wait != 0,
        hollow_pe: o.hollow_pe != 0,
        shim_dll: shim,
        payload_dll: payload,
    };
    match s.launch(&launch) {
        Ok(code) => {
            if !exit_code.is_null() {
                unsafe {
                    *exit_code = code;
                }
            }
            0
        }
        Err(_) => vfs_protocol::ST_IO_ERROR,
    }
}

/* ---- Optional host inspection (uses kernel only) ---- */

#[no_mangle]
pub extern "C" fn vfs_getattr(d: *mut Session, path: *const c_char, out: *mut VfsStat) -> c_int {
    let s = match session(d) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if out.is_null() {
        return vfs_protocol::ST_BAD_REQUEST;
    }
    let path = match cstr(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match s.kernel().getattr(path) {
        Ok(Some(st)) => {
            unsafe {
                *out = VfsStat {
                    kind: st.kind,
                    size: st.size,
                    mtime: st.mtime,
                };
            }
            0
        }
        Ok(None) => vfs_protocol::ST_NOT_FOUND,
        Err(e) => e,
    }
}

#[no_mangle]
pub extern "C" fn vfs_readdir(
    d: *mut Session,
    path: *const c_char,
    fill_ctx: *mut c_void,
    fill: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const VfsStat) -> c_int>,
) -> c_int {
    let s = match session(d) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let path = match cstr(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let entries = match s.kernel().readdir(path) {
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
    0
}

#[no_mangle]
pub extern "C" fn vfs_open(
    d: *mut Session,
    path: *const c_char,
    flags: u32,
    fh_out: *mut u64,
    size_out: *mut u64,
    is_dir_out: *mut u8,
) -> c_int {
    let s = match session(d) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if fh_out.is_null() || size_out.is_null() || is_dir_out.is_null() {
        return vfs_protocol::ST_BAD_REQUEST;
    }
    let path = match cstr(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match s.kernel().open(path, flags) {
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
    d: *mut Session,
    fh: u64,
    offset: u64,
    buf: *mut u8,
    len: u32,
    nread: *mut u32,
) -> c_int {
    let s = match session(d) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if buf.is_null() || nread.is_null() {
        return vfs_protocol::ST_BAD_REQUEST;
    }
    let slice = unsafe { std::slice::from_raw_parts_mut(buf, len as usize) };
    match s.kernel().read(fh, offset, slice) {
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
pub extern "C" fn vfs_close(d: *mut Session, fh: u64) -> c_int {
    let s = match session(d) {
        Ok(s) => s,
        Err(e) => return e,
    };
    match s.kernel().close(fh) {
        Ok(()) => 0,
        Err(e) => e,
    }
}


