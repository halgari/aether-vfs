//! The registry tests' shared fixture pieces: NT key calls, scratch real keys, the unhooked
//! checker process, and the director plus hooks every registry test runs against.
//!
//! A test file keeps only what is its own: its `BASE` key, the real keys it makes, and the
//! extras its `Fixture` carries. Declare this module in the test binary as
//! `#[path = "common/reg.rs"] mod reg;` (the checker is then `reg::reg_checker`, see
//! [`CHECKER_TEST`]).
#![allow(dead_code)] // each registry test file uses a different subset

use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{FILETIME, LocalFree, HANDLE};
use windows_sys::Win32::System::Registry::{
    RegSetKeySecurity,
    HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_READ, REG_OPTION_NON_VOLATILE, RegCloseKey,
    RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW,
};

use super::fakedirector::{self, Fake};
use vfs_shim::{Engine, install, regclient};

pub(crate) const STATUS_SUCCESS: i32 = 0;
pub(crate) const OBJ_CASE_INSENSITIVE: u32 = 0x40;
pub(crate) const OBJECT_NAME_INFORMATION: u32 = 1;

/// Marks the checker process (the same test binary running [`reg_checker`]).
const CHECKER_ENV: &str = "AETHER_VFS_REG_CHECKER";
/// The libtest name of [`reg_checker`] when this file is `mod reg;` at a binary's root.
const CHECKER_TEST: &str = "reg::reg_checker";

#[repr(C)]
pub(crate) struct UnicodeString {
    pub(crate) length: u16,
    pub(crate) maximum_length: u16,
    pub(crate) buffer: *const u16,
}

#[repr(C)]
pub(crate) struct ObjectAttributes {
    pub(crate) length: u32,
    pub(crate) root_directory: isize,
    pub(crate) object_name: *const UnicodeString,
    pub(crate) attributes: u32,
    pub(crate) security_descriptor: *const c_void,
    pub(crate) security_qos: *const c_void,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtOpenKeyEx(key: *mut isize, access: u32, oa: *const ObjectAttributes, options: u32) -> i32;
    fn NtClose(h: isize) -> i32;
    fn NtQueryObject(h: isize, class: u32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// `f` with a `UNICODE_STRING` for `s`.
pub(crate) fn with_us<R>(s: &str, f: impl FnOnce(*const UnicodeString) -> R) -> R {
    let w: Vec<u16> = s.encode_utf16().collect();
    let us = UnicodeString {
        length: (w.len() * 2) as u16,
        maximum_length: (w.len() * 2) as u16,
        buffer: w.as_ptr(),
    };
    f(&us)
}

/// `f` with `OBJECT_ATTRIBUTES` naming `name` relative to `root` (0: absolute).
pub(crate) fn with_oa<R>(
    root: isize,
    name: &str,
    f: impl FnOnce(*const ObjectAttributes) -> R,
) -> R {
    with_us(name, |us| {
        let oa = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: root,
            object_name: us,
            attributes: OBJ_CASE_INSENSITIVE,
            security_descriptor: std::ptr::null(),
            security_qos: std::ptr::null(),
        };
        f(&oa)
    })
}

/// `NtOpenKeyEx` of the absolute NT name `name`.
pub(crate) fn open_abs(name: &str, access: u32) -> (i32, isize) {
    let mut h = 0isize;
    let st = with_oa(0, name, |oa| unsafe { NtOpenKeyEx(&mut h, access, oa, 0) });
    (st, h)
}

pub(crate) fn close(h: isize) -> i32 {
    unsafe { NtClose(h) }
}

/// `NtQueryObject` for a class whose answer starts with a `UNICODE_STRING`.
pub(crate) fn object_string(h: isize, class: u32) -> Result<String, i32> {
    let mut buf = vec![0u64; 512];
    let mut ret = 0u32;
    let st = unsafe { NtQueryObject(h, class, buf.as_mut_ptr().cast(), 4096, &mut ret) };
    if st != STATUS_SUCCESS {
        return Err(st);
    }
    let us = unsafe { &*(buf.as_ptr() as *const UnicodeString) };
    let s = unsafe { std::slice::from_raw_parts(us.buffer, us.length as usize / 2) };
    Ok(String::from_utf16_lossy(s))
}

/// `NtQueryObject(ObjectNameInformation)`.
pub(crate) fn object_name(h: isize) -> Result<String, i32> {
    object_string(h, OBJECT_NAME_INFORMATION)
}

/// Create (or open) `HKCU\<sub>` with `class`, KEY_ALL_ACCESS.
pub(crate) fn reg_create_class(sub: &str, class: Option<&str>) -> HKEY {
    let mut k: HKEY = std::ptr::null_mut();
    let class_w = class.map(wide);
    let st = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(sub).as_ptr(),
            0,
            class_w.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            REG_OPTION_NON_VOLATILE,
            KEY_ALL_ACCESS,
            std::ptr::null(),
            &mut k,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(st, 0, "RegCreateKeyExW {sub}");
    k
}

pub(crate) fn reg_create(sub: &str) -> HKEY {
    reg_create_class(sub, None)
}

/// The current user's SID as a string.
pub(crate) fn user_sid() -> String {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
            0
        );
        let mut buf = vec![0u64; 64];
        let mut need = 0;
        assert_ne!(
            GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), 512, &mut need),
            0
        );
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s: *mut u16 = std::ptr::null_mut();
        assert_ne!(ConvertSidToStringSidW(tu.User.Sid, &mut s), 0);
        let len = (0..).take_while(|&i| *s.add(i) != 0).count();
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
        LocalFree(s.cast());
        out
    }
}

/// Apply the SDDL string `sddl` to the open key `k`.
unsafe fn apply_sddl(k: HKEY, sddl: &str) {
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    let mut sd: *mut c_void = std::ptr::null_mut();
    assert_ne!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide(sddl).as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        },
        0
    );
    assert_eq!(unsafe { RegSetKeySecurity(k, DACL_SECURITY_INFORMATION, sd) }, 0);
    unsafe { LocalFree(sd) };
}

/// Set `HKCU\<sub>`'s DACL from SDDL, creating the key if need be.
pub(crate) fn set_dacl(sub: &str, sddl: &str) {
    let k = reg_create(sub);
    unsafe {
        apply_sddl(k, sddl);
        RegCloseKey(k);
    }
}

/// Give back everyone full access to `HKCU\<sub>` (so it can be deleted), if it exists.
fn unlock(sub: &str) {
    const WRITE_DAC: u32 = 0x4_0000;
    let mut k: HKEY = std::ptr::null_mut();
    let st = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(sub).as_ptr(), 0, WRITE_DAC, &mut k) };
    if st == 0 {
        unsafe {
            apply_sddl(k, "D:(A;;GA;;;WD)");
            RegCloseKey(k);
        }
    }
}

/// Delete `HKCU\<base>` and wait until it is gone. `locked` names the subkeys (relative to
/// `base`) a test gives a restrictive DACL: Wine's server keeps a key's DACL for as long as it
/// runs, so a previous test's locked keys survive a delete until they are unlocked.
pub(crate) fn reset_base(base: &str, locked: &[&str]) {
    for rel in locked {
        unlock(&format!(r"{base}\{rel}"));
    }
    unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(base).as_ptr()) };
    let mut k: HKEY = std::ptr::null_mut();
    let st = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(base).as_ptr(), 0, KEY_READ, &mut k) };
    if st == 0 {
        unsafe { RegCloseKey(k) };
        panic!("HKCU\\{base} could not be deleted");
    }
}

/// The NT and canonical spellings of a test's keys under `HKCU\<base>`.
pub(crate) struct Paths {
    pub(crate) sid: String,
    base: &'static str,
}

impl Paths {
    pub(crate) fn new(base: &'static str) -> Paths {
        Paths {
            sid: user_sid(),
            base,
        }
    }

    /// `HKCU\<base>\<rel>` as an absolute NT name.
    pub(crate) fn nt(&self, rel: &str) -> String {
        format!(r"\REGISTRY\USER\{}\{}\{rel}", self.sid, self.base)
    }

    /// ... and as the canonical path the overlay stores.
    pub(crate) fn canon(&self, rel: &str) -> String {
        format!(r"\Registry\User\<CurrentUser>\{}\{rel}", self.base)
    }
}

/// The unhooked process that answers what is really in the registry: this test binary, started
/// before the hooks went in, running the ignored [`reg_checker`].
pub(crate) struct Checker {
    child: Child,
    /// The checker process's handle, for tests that duplicate handles into it.
    pub(crate) process: isize,
    io: Mutex<(ChildStdin, BufReader<ChildStdout>)>,
}

impl Checker {
    /// Start it. Call before the hooks are installed.
    pub(crate) fn spawn() -> Checker {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([CHECKER_TEST, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
            .env(CHECKER_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("checker");
        use std::os::windows::io::AsRawHandle;
        let process = child.as_raw_handle() as isize;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Checker {
            child,
            process,
            io: Mutex::new((stdin, stdout)),
        }
    }

    pub(crate) fn ask(&self, q: &str) -> String {
        let mut io = self.io.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(io.0, "{q}").unwrap();
        io.0.flush().unwrap();
        loop {
            let mut line = String::new();
            assert_ne!(io.1.read_line(&mut line).unwrap(), 0, "checker exited");
            // libtest's own `test reg_checker ... ` may lead the first answer's line.
            if let Some(i) = line.find("CHECK:") {
                return line[i + 6..].trim().to_string();
            }
        }
    }

    /// Whether `HKCU\<key>` exists in the real registry.
    pub(crate) fn really_exists(&self, key: &str) -> bool {
        self.ask(&format!("K|{key}")) == "1"
    }

    /// The real value's data as hex, or `none` (`nokey` when the key is missing).
    pub(crate) fn real_value(&self, key: &str, name: &str) -> String {
        self.ask(&format!("V|{key}|{name}"))
    }

    /// The real key's last-write time, its value count and its subkey count.
    pub(crate) fn real_info(&self, key: &str) -> String {
        self.ask(&format!("I|{key}"))
    }
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Not a test of its own: the checker process [`Checker`] asks. Each stdin line is a query on
/// the real registry under HKCU: `K|key` (exists: 1 or 0), `V|key|name` (the value's data in
/// hex, or `none`), `I|key` (last-write time, value and subkey counts).
#[test]
#[ignore]
fn reg_checker() {
    if std::env::var_os(CHECKER_ENV).is_none() {
        return;
    }
    let mut out = std::io::stdout();
    for line in std::io::stdin().lines() {
        let line = line.unwrap();
        let parts: Vec<&str> = line.trim().split('|').collect();
        let mut k: HKEY = std::ptr::null_mut();
        let st = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide(parts[1]).as_ptr(),
                0,
                KEY_READ,
                &mut k,
            )
        };
        let answer = match (parts[0], st) {
            ("K", st) => (if st == 0 { "1" } else { "0" }).to_string(),
            (_, st) if st != 0 => "nokey".to_string(),
            ("V", _) => {
                let mut buf = [0u8; 256];
                let mut len = 256u32;
                let mut ty = 0u32;
                let r = unsafe {
                    RegQueryValueExW(
                        k,
                        wide(parts[2]).as_ptr(),
                        std::ptr::null(),
                        &mut ty,
                        buf.as_mut_ptr(),
                        &mut len,
                    )
                };
                if r == 0 {
                    hex(&buf[..len as usize])
                } else {
                    "none".to_string()
                }
            }
            _ => {
                let (mut subs, mut vals) = (0u32, 0u32);
                let mut ft = FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0,
                };
                let n = std::ptr::null_mut();
                unsafe {
                    RegQueryInfoKeyW(
                        k,
                        std::ptr::null_mut(),
                        n,
                        std::ptr::null(),
                        &mut subs,
                        n,
                        n,
                        &mut vals,
                        n,
                        n,
                        n,
                        &mut ft,
                    )
                };
                format!("{}:{}:{vals}:{subs}", ft.dwHighDateTime, ft.dwLowDateTime)
            }
        };
        if st == 0 {
            unsafe { RegCloseKey(k) };
        }
        writeln!(out, "CHECK:{answer}").unwrap();
        out.flush().unwrap();
    }
}

/// Start the fake director with its real registry behind the ring and switch the registry
/// overlay on (`VFS_REGISTRY`, which `regclient::enabled` reads once). Returns the virtual
/// root's directory too.
pub(crate) fn start_director(tag: &str) -> (&'static Fake, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("vfs-shim-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var(vfs_env::REGISTRY, "1");
    let fake = fakedirector::install(&root, Fake::new().with_registry(), 0);
    (fake, root)
}

/// [`start_director`], then the shim's hooks over an empty root. Never uninstalled.
pub(crate) fn install_hooks(tag: &str) -> &'static Fake {
    let (fake, root) = start_director(tag);
    let snapshot = {
        use vfs_core::{Layer, LayerId, build};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    std::mem::forget(install(engine).expect("install"));
    assert_eq!(
        vfs_shim::reg_overlay_disabled_by(),
        None,
        "every registry detour is in"
    );
    assert!(regclient::enabled());
    fake
}
