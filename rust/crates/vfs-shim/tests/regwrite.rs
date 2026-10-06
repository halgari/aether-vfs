//! The registry write hooks (registry overlay spec sections 3.1, 3.2 and 6): `NtSetValueKey`,
//! `NtDeleteValueKey`, `NtDeleteKey`, `NtRenameKey`, `NtSetInformationKey` and `NtFlushKey`,
//! through the real ntdll entry points with the shim's detours installed and a real director
//! registry behind the ring (`fakedirector` with [`Fake::with_registry`]).
//!
//! Real scratch keys live under `HKCU\Software\AetherVfsRegWriteTest`, made before the hooks go
//! in. What the *real* registry holds afterwards is asked of a checker: this same test binary,
//! started before the hooks were installed (so it has none), running the ignored `reg_checker`
//! test, which answers over its stdin and stdout.
//!
//! Every test takes [`LOCK`]: they share one director, and one of them detaches its registry.
#![cfg(windows)]

mod fakedirector;

use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};

use fakedirector::Fake;
use vfs_registry::Lookup;
use vfs_shim::{install, is_synthetic_key_handle, regclient, registry_handle_path, Engine};
use windows_sys::Win32::Foundation::{LocalFree, FILETIME, HANDLE};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegQueryInfoKeyW,
    RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_READ,
    REG_OPTION_NON_VOLATILE,
};

static LOCK: Mutex<()> = Mutex::new(());

const BASE: &str = r"Software\AetherVfsRegWriteTest";
const CHECKER_ENV: &str = "AETHER_VFS_REGWRITE_CHECKER";

const STATUS_SUCCESS: i32 = 0;
const STATUS_UNSUCCESSFUL: i32 = 0xC000_0001u32 as i32;
const STATUS_INVALID_INFO_CLASS: i32 = 0xC000_0003u32 as i32;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;
const STATUS_INVALID_PARAMETER: i32 = 0xC000_000Du32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
const STATUS_CANNOT_DELETE: i32 = 0xC000_0121u32 as i32;
const STATUS_KEY_DELETED: i32 = 0xC000_017Cu32 as i32;

const NT_KEY_QUERY_VALUE: u32 = 0x1;
const NT_KEY_SET_VALUE: u32 = 0x2;
const NT_KEY_READ: u32 = 0x2_0019;
const NT_KEY_WRITE: u32 = 0x2_0006;
const NT_KEY_ALL_ACCESS: u32 = 0xF_003F;
const DELETE: u32 = 0x1_0000;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
const OBJECT_NAME_INFORMATION: u32 = 1;
const KEY_VALUE_PARTIAL_INFORMATION: u32 = 2;
const KEY_NAME_INFORMATION: u32 = 3;
const REG_SZ: u32 = 1;
const REG_BINARY: u32 = 3;
const REG_DWORD: u32 = 4;
const REG_CREATED_NEW_KEY: u32 = 1;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: isize,
    object_name: *const UnicodeString,
    attributes: u32,
    security_descriptor: *const c_void,
    security_qos: *const c_void,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtOpenKeyEx(key: *mut isize, access: u32, oa: *const ObjectAttributes, options: u32) -> i32;
    fn NtCreateKey(
        key: *mut isize,
        access: u32,
        oa: *const ObjectAttributes,
        title_index: u32,
        class: *const UnicodeString,
        options: u32,
        disposition: *mut u32,
    ) -> i32;
    fn NtClose(h: isize) -> i32;
    fn NtQueryObject(h: isize, class: u32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
    fn NtQueryKey(h: isize, class: u32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
    fn NtQueryValueKey(
        h: isize,
        name: *const UnicodeString,
        class: u32,
        info: *mut u8,
        len: u32,
        ret: *mut u32,
    ) -> i32;
    fn NtSetValueKey(
        h: isize,
        name: *const UnicodeString,
        title_index: u32,
        ty: u32,
        data: *const u8,
        size: u32,
    ) -> i32;
    fn NtDeleteValueKey(h: isize, name: *const UnicodeString) -> i32;
    fn NtDeleteKey(h: isize) -> i32;
    fn NtRenameKey(h: isize, name: *const UnicodeString) -> i32;
    fn NtSetInformationKey(h: isize, class: u32, info: *const u8, len: u32) -> i32;
    fn NtFlushKey(h: isize) -> i32;
}

/// `f` with a `UNICODE_STRING` for `s`.
fn with_us<R>(s: &str, f: impl FnOnce(*const UnicodeString) -> R) -> R {
    let w: Vec<u16> = s.encode_utf16().collect();
    let us = UnicodeString {
        length: (w.len() * 2) as u16,
        maximum_length: (w.len() * 2) as u16,
        buffer: w.as_ptr(),
    };
    f(&us)
}

fn open_abs(name: &str, access: u32) -> (i32, isize) {
    let mut h = 0isize;
    let st = with_us(name, |us| {
        let oa = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: 0,
            object_name: us,
            attributes: OBJ_CASE_INSENSITIVE,
            security_descriptor: std::ptr::null(),
            security_qos: std::ptr::null(),
        };
        unsafe { NtOpenKeyEx(&mut h, access, &oa, 0) }
    });
    (st, h)
}

fn create_abs(name: &str, access: u32) -> (i32, isize, u32) {
    let mut h = 0isize;
    let mut disp = 0u32;
    let st = with_us(name, |us| {
        let oa = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: 0,
            object_name: us,
            attributes: OBJ_CASE_INSENSITIVE,
            security_descriptor: std::ptr::null(),
            security_qos: std::ptr::null(),
        };
        unsafe { NtCreateKey(&mut h, access, &oa, 0, std::ptr::null(), 0, &mut disp) }
    });
    (st, h, disp)
}

fn close(h: isize) -> i32 {
    unsafe { NtClose(h) }
}

fn set(h: isize, name: &str, ty: u32, data: &[u8]) -> i32 {
    with_us(name, |us| unsafe {
        NtSetValueKey(h, us, 0, ty, data.as_ptr(), data.len() as u32)
    })
}

fn set_dword(h: isize, name: &str, v: u32) -> i32 {
    set(h, name, REG_DWORD, &v.to_le_bytes())
}

fn delete_value(h: isize, name: &str) -> i32 {
    with_us(name, |us| unsafe { NtDeleteValueKey(h, us) })
}

fn rename(h: isize, name: &str) -> i32 {
    with_us(name, |us| unsafe { NtRenameKey(h, us) })
}

/// A value's (type, data), through `NtQueryValueKey(KeyValuePartialInformation)`.
fn query(h: isize, name: &str) -> Result<(u32, Vec<u8>), i32> {
    let mut buf = vec![0u64; 512];
    let mut ret = 0u32;
    let st = with_us(name, |us| unsafe {
        NtQueryValueKey(
            h,
            us,
            KEY_VALUE_PARTIAL_INFORMATION,
            buf.as_mut_ptr().cast(),
            4096,
            &mut ret,
        )
    });
    if st != STATUS_SUCCESS {
        return Err(st);
    }
    let b = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, 4096) };
    let ty = u32::from_le_bytes(b[4..8].try_into().unwrap());
    let n = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
    Ok((ty, b[12..12 + n].to_vec()))
}

fn query_dword(h: isize, name: &str) -> Result<u32, i32> {
    query(h, name).map(|(_, d)| u32::from_le_bytes(d[..4].try_into().unwrap()))
}

/// `NtQueryKey(KeyNameInformation)`.
fn key_name(h: isize) -> Result<String, i32> {
    let mut buf = vec![0u32; 512];
    let mut ret = 0u32;
    let st = unsafe {
        NtQueryKey(
            h,
            KEY_NAME_INFORMATION,
            buf.as_mut_ptr().cast(),
            2048,
            &mut ret,
        )
    };
    if st != STATUS_SUCCESS {
        return Err(st);
    }
    let n = buf[0] as usize / 2;
    let units = unsafe { std::slice::from_raw_parts((buf.as_ptr() as *const u16).add(2), n) };
    Ok(String::from_utf16_lossy(units))
}

/// `NtQueryObject(ObjectNameInformation)`.
fn object_name(h: isize) -> Result<String, i32> {
    let mut buf = vec![0u64; 512];
    let mut ret = 0u32;
    let st = unsafe {
        NtQueryObject(
            h,
            OBJECT_NAME_INFORMATION,
            buf.as_mut_ptr().cast(),
            4096,
            &mut ret,
        )
    };
    if st != STATUS_SUCCESS {
        return Err(st);
    }
    let us = unsafe { &*(buf.as_ptr() as *const UnicodeString) };
    let s = unsafe { std::slice::from_raw_parts(us.buffer, us.length as usize / 2) };
    Ok(String::from_utf16_lossy(s))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn reg_create(sub: &str) -> HKEY {
    let mut k: HKEY = std::ptr::null_mut();
    let st = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(sub).as_ptr(),
            0,
            std::ptr::null(),
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

/// Make the real key `HKCU\BASE\<rel>` with DWORD values.
fn real_key(rel: &str, values: &[(&str, u32)]) {
    let k = reg_create(&format!(r"{BASE}\{rel}"));
    for (n, v) in values {
        let st = unsafe {
            RegSetValueExW(
                k,
                wide(n).as_ptr(),
                0,
                REG_DWORD,
                v.to_le_bytes().as_ptr(),
                4,
            )
        };
        assert_eq!(st, 0);
    }
    unsafe { RegCloseKey(k) };
}

fn user_sid() -> String {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
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

struct Checker {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

struct Fixture {
    fake: &'static Fake,
    sid: String,
    checker: Mutex<Checker>,
    /// `HKCU\BASE\Pre`, opened KEY_ALL_ACCESS before the hooks (so neither table knows it).
    pre: isize,
}

impl Fixture {
    fn nt(&self, rel: &str) -> String {
        format!(r"\REGISTRY\USER\{}\{BASE}\{rel}", self.sid)
    }

    fn canon(&self, rel: &str) -> String {
        format!(r"\Registry\User\<CurrentUser>\{BASE}\{rel}")
    }

    fn open(&self, rel: &str, access: u32) -> (i32, isize) {
        open_abs(&self.nt(rel), access)
    }

    fn ask(&self, q: &str) -> String {
        let mut c = self.checker.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(c.stdin, "{q}").unwrap();
        c.stdin.flush().unwrap();
        loop {
            let mut line = String::new();
            assert_ne!(c.stdout.read_line(&mut line).unwrap(), 0, "checker exited");
            // libtest's own `test reg_checker ... ` may lead the first answer's line.
            if let Some(i) = line.find("CHECK:") {
                return line[i + 6..].trim().to_string();
            }
        }
    }

    /// Whether the key exists in the real registry.
    fn really_exists(&self, rel: &str) -> bool {
        self.ask(&format!(r"K|{BASE}\{rel}")) == "1"
    }

    /// The real value's data as hex, or `none`.
    fn real_value(&self, rel: &str, name: &str) -> String {
        self.ask(&format!(r"V|{BASE}\{rel}|{name}"))
    }

    /// The real key's last-write time, its value count and its subkey count.
    fn real_info(&self, rel: &str) -> String {
        self.ask(&format!(r"I|{BASE}\{rel}"))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn fixture() -> (MutexGuard<'static, ()>, &'static Fixture) {
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    static F: OnceLock<Fixture> = OnceLock::new();
    let f = F.get_or_init(|| {
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(BASE).as_ptr()) };
        real_key("Cow", &[("orig", 1), ("dv", 7)]);
        real_key("Seen", &[]);
        real_key(r"Del\Parent\Child", &[]);
        real_key(r"Del\Leaf", &[("x", 1)]);
        real_key(r"Del\Dead", &[]);
        real_key(r"Del\Tree\Sub", &[]);
        real_key(r"Ren\Real", &[("a", 1)]);
        real_key(r"Ren\Real\S1", &[("b", 2)]);
        real_key(r"Ren\Real\S1\Deep", &[("d", 4)]);
        real_key(r"Ren\Real\S2", &[]);
        real_key(r"Ren\Real3", &[]);
        real_key(r"Ren\Other", &[]);
        real_key("Denied", &[("v", 1)]);
        real_key("Info", &[("v", 1)]);
        real_key("Dead", &[("v", 1)]);
        real_key("Pre", &[]);
        let sid = user_sid();
        let (st, pre) = open_abs(
            &format!(r"\REGISTRY\USER\{sid}\{BASE}\Pre"),
            NT_KEY_ALL_ACCESS,
        );
        assert_eq!(st, STATUS_SUCCESS);

        // The checker: started now, so it has no hooks.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "reg_checker",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHECKER_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("checker");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());

        let root = std::env::temp_dir().join(format!("vfs-shim-regwrite-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var(vfs_env::REGISTRY, "1");
        let fake = fakedirector::install(&root, Fake::new().with_registry(), 0);
        let snapshot = {
            use vfs_core::{build, Layer, LayerId};
            let tree = build(vec![Layer {
                id: LayerId(0),
                entries: vec![],
            }])
            .unwrap();
            vfs_shared::bridge::flatten(&tree)
        };
        let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
        std::mem::forget(install(engine).expect("install"));
        assert!(regclient::enabled());
        Fixture {
            fake,
            sid,
            checker: Mutex::new(Checker {
                _child: child,
                stdin,
                stdout,
            }),
            pre,
        }
    });
    (guard, f)
}

/// Not a test of its own: the checker process [`Fixture::ask`] asks. Each stdin line is a
/// query on the real registry under HKCU: `K|key` (exists: 1 or 0), `V|key|name` (the value's
/// data in hex, or `none`), `I|key` (last-write time, value and subkey counts).
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

#[test]
fn a_write_through_a_real_handle_goes_to_the_overlay_and_the_handle_then_merges() {
    let (_g, f) = fixture();
    let before = f.real_info("Cow");
    let (st, h) = f.open("Cow", NT_KEY_READ | NT_KEY_SET_VALUE);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(!is_synthetic_key_handle(h), "untouched: the real handle");
    // A second handle, opened before the write.
    let (st, h2) = f.open("Cow", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(query_dword(h, "orig"), Ok(1));

    assert_eq!(set_dword(h, "orig", 2), STATUS_SUCCESS);
    assert_eq!(
        set(h, "new", REG_SZ, &[b'h', 0, b'i', 0, 0, 0]),
        STATUS_SUCCESS
    );
    // Any type number, and empty data.
    assert_eq!(set(h, "odd", 0x1234_5678, &[]), STATUS_SUCCESS);
    // The default value: an empty name.
    assert_eq!(set(h, "", REG_BINARY, &[9, 9]), STATUS_SUCCESS);
    // A NULL name is the default value too.
    let st = unsafe { NtSetValueKey(h, std::ptr::null(), 0, REG_BINARY, [8u8].as_ptr(), 1) };
    assert_eq!(st, STATUS_SUCCESS);

    assert!(!is_synthetic_key_handle(h), "the caller keeps its handle");
    assert_eq!(
        regclient::lookup(&f.canon("Cow")),
        Ok((Lookup::Present { created: false }, false))
    );
    // The same handle now answers from the merge.
    assert_eq!(query_dword(h, "orig"), Ok(2));
    assert_eq!(query(h, "new"), Ok((REG_SZ, vec![b'h', 0, b'i', 0, 0, 0])));
    assert_eq!(query(h, "odd"), Ok((0x1234_5678, vec![])));
    assert_eq!(query(h, ""), Ok((REG_BINARY, vec![8])));
    assert_eq!(query_dword(h, "dv"), Ok(7), "a real value still shows");
    // So does the handle opened before the write, and a fresh one.
    assert_eq!(query_dword(h2, "orig"), Ok(2));
    let (_, h3) = f.open("Cow", NT_KEY_READ);
    assert_eq!(query_dword(h3, "orig"), Ok(2));

    // The real key is untouched.
    assert_eq!(f.real_value("Cow", "orig"), hex(&1u32.to_le_bytes()));
    assert_eq!(f.real_value("Cow", "new"), "none");
    assert_eq!(f.real_value("Cow", ""), "none");
    assert_eq!(f.real_info("Cow"), before);
    for x in [h, h2, h3] {
        close(x);
    }
}

#[test]
fn a_handle_opened_before_the_hooks_writes_to_the_overlay() {
    let (_g, f) = fixture();
    let st = unsafe {
        RegSetValueExW(
            f.pre as HKEY,
            wide("viaWin32").as_ptr(),
            0,
            REG_DWORD,
            5u32.to_le_bytes().as_ptr(),
            4,
        )
    };
    assert_eq!(st, 0);
    assert_eq!(query_dword(f.pre, "viaWin32"), Ok(5));
    assert_eq!(f.real_value("Pre", "viaWin32"), "none");
    assert_eq!(registry_handle_path(f.pre), Some(f.canon("Pre")));
}

#[test]
fn delete_value_tombstones_the_name_and_leaves_the_real_value() {
    let (_g, f) = fixture();
    let (st, h) = f.open("Cow", NT_KEY_READ | NT_KEY_SET_VALUE);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(query_dword(h, "dv"), Ok(7));
    assert_eq!(delete_value(h, "DV"), STATUS_SUCCESS, "case-insensitive");
    assert_eq!(query(h, "dv"), Err(STATUS_OBJECT_NAME_NOT_FOUND));
    assert_eq!(delete_value(h, "dv"), STATUS_OBJECT_NAME_NOT_FOUND);
    assert_eq!(delete_value(h, "never"), STATUS_OBJECT_NAME_NOT_FOUND);
    // An overlay value goes too.
    assert_eq!(set_dword(h, "tmp", 3), STATUS_SUCCESS);
    assert_eq!(delete_value(h, "tmp"), STATUS_SUCCESS);
    assert_eq!(query(h, "tmp"), Err(STATUS_OBJECT_NAME_NOT_FOUND));
    assert_eq!(f.real_value("Cow", "dv"), hex(&7u32.to_le_bytes()));
    close(h);
}

#[test]
fn delete_key_refuses_a_key_with_subkeys_and_marks_the_handle_deleted() {
    let (_g, f) = fixture();
    let (st, parent) = f.open(r"Del\Parent", NT_KEY_ALL_ACCESS);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(unsafe { NtDeleteKey(parent) }, STATUS_CANNOT_DELETE);

    let (st, leaf) = f.open(r"Del\Leaf", NT_KEY_READ | DELETE | NT_KEY_SET_VALUE);
    assert_eq!(st, STATUS_SUCCESS);
    let (st, other) = f.open(r"Del\Leaf", NT_KEY_READ | NT_KEY_SET_VALUE);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(unsafe { NtDeleteKey(leaf) }, STATUS_SUCCESS);

    // Through the deleting handle.
    assert_eq!(query(leaf, "x"), Err(STATUS_KEY_DELETED));
    assert_eq!(key_name(leaf), Err(STATUS_KEY_DELETED));
    assert_eq!(object_name(leaf), Err(STATUS_KEY_DELETED));
    assert_eq!(set_dword(leaf, "y", 1), STATUS_KEY_DELETED);
    assert_eq!(delete_value(leaf, "x"), STATUS_KEY_DELETED);
    assert_eq!(unsafe { NtDeleteKey(leaf) }, STATUS_KEY_DELETED);
    assert_eq!(unsafe { NtFlushKey(leaf) }, STATUS_KEY_DELETED);
    // Through another handle opened before the delete.
    assert_eq!(query(other, "x"), Err(STATUS_KEY_DELETED));
    assert_eq!(set_dword(other, "y", 1), STATUS_KEY_DELETED);
    // Gone from the merged view.
    assert_eq!(
        f.open(r"Del\Leaf", NT_KEY_READ).0,
        STATUS_OBJECT_NAME_NOT_FOUND
    );
    assert_eq!(close(leaf), STATUS_SUCCESS);
    assert_eq!(close(other), STATUS_SUCCESS);
    assert!(f.really_exists(r"Del\Leaf"), "the real key is untouched");

    // Delete the child first, then the parent goes.
    let (st, child) = f.open(r"Del\Parent\Child", DELETE);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(unsafe { NtDeleteKey(child) }, STATUS_SUCCESS);
    close(child);
    assert_eq!(unsafe { NtDeleteKey(parent) }, STATUS_SUCCESS);
    assert_eq!(query(parent, "x"), Err(STATUS_KEY_DELETED));
    close(parent);
    assert!(f.really_exists(r"Del\Parent\Child"));

    // A handle on a key below one deleted elsewhere reports it deleted too.
    let (st, below) = f.open(r"Del\Tree\Sub", NT_KEY_ALL_ACCESS);
    assert_eq!(st, STATUS_SUCCESS);
    regclient::delete_key(&f.canon(r"Del\Tree")).unwrap();
    assert_eq!(query(below, "x"), Err(STATUS_KEY_DELETED));
    assert_eq!(set_dword(below, "y", 1), STATUS_KEY_DELETED);
    assert_eq!(unsafe { NtDeleteKey(below) }, STATUS_KEY_DELETED);
    close(below);
    assert!(f.really_exists(r"Del\Tree\Sub"));

    // Keys created here: a key with a created subkey cannot go; the subkey, then it, can.
    let (st, made, disp) = create_abs(&f.nt(r"Del\Made"), NT_KEY_ALL_ACCESS);
    assert_eq!((st, disp), (STATUS_SUCCESS, REG_CREATED_NEW_KEY));
    let (st, kid, _) = create_abs(&f.nt(r"Del\Made\Kid"), NT_KEY_ALL_ACCESS);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(unsafe { NtDeleteKey(made) }, STATUS_CANNOT_DELETE);
    assert_eq!(unsafe { NtDeleteKey(kid) }, STATUS_SUCCESS);
    assert_eq!(unsafe { NtDeleteKey(made) }, STATUS_SUCCESS);
    assert_eq!(object_name(made), Err(STATUS_KEY_DELETED));
    assert_eq!(
        f.open(r"Del\Made", NT_KEY_READ).0,
        STATUS_OBJECT_NAME_NOT_FOUND
    );
    close(kid);
    close(made);
    assert!(!f.really_exists(r"Del\Made"));
}

#[test]
fn rename_of_a_key_created_here_moves_it() {
    let (_g, f) = fixture();
    let (st, h, _) = create_abs(&f.nt(r"Ren\Made"), NT_KEY_ALL_ACCESS);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(set_dword(h, "v", 11), STATUS_SUCCESS);
    let (st, kid, _) = create_abs(&f.nt(r"Ren\Made\Kid"), NT_KEY_ALL_ACCESS);
    assert_eq!(st, STATUS_SUCCESS);
    close(kid);

    assert_eq!(rename(h, "Moved"), STATUS_SUCCESS);
    assert_eq!(registry_handle_path(h), Some(f.canon(r"Ren\Moved")));
    assert!(key_name(h).unwrap().ends_with(r"\Ren\Moved"));
    assert_eq!(query_dword(h, "v"), Ok(11));
    assert_eq!(
        f.open(r"Ren\Made", NT_KEY_READ).0,
        STATUS_OBJECT_NAME_NOT_FOUND
    );
    let (st, moved_kid) = f.open(r"Ren\Moved\Kid", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    close(moved_kid);
    assert!(!f.really_exists(r"Ren\Moved"));
    close(h);
}

#[test]
fn rename_of_a_real_key_copies_its_merged_subtree() {
    let (_g, f) = fixture();
    // Overlay changes in the subtree before the rename: a value, a deleted subkey, a volatile
    // created subkey.
    regclient::set_value(&f.canon(r"Ren\Real"), "c", REG_DWORD, &3u32.to_le_bytes()).unwrap();
    regclient::delete_key(&f.canon(r"Ren\Real\S2")).unwrap();
    regclient::create_key(&f.canon(r"Ren\Real\Vol"), true).unwrap();

    let (st, h) = f.open(r"Ren\Real", NT_KEY_ALL_ACCESS);
    assert_eq!(st, STATUS_SUCCESS);
    // An existing name, the key's own in another case, an invalid name.
    assert_eq!(rename(h, "Other"), STATUS_CANNOT_DELETE);
    assert_eq!(rename(h, "REAL"), STATUS_CANNOT_DELETE);
    assert_eq!(rename(h, r"a\b"), STATUS_INVALID_PARAMETER);
    assert_eq!(rename(h, ""), STATUS_INVALID_PARAMETER);

    assert_eq!(rename(h, "RealNew"), STATUS_SUCCESS);
    assert_eq!(registry_handle_path(h), Some(f.canon(r"Ren\RealNew")));
    assert!(object_name(h).unwrap().ends_with(r"\Ren\RealNew"));
    assert!(key_name(h).unwrap().ends_with(r"\Ren\RealNew"));
    assert_eq!(query_dword(h, "a"), Ok(1));
    assert_eq!(query_dword(h, "c"), Ok(3));

    assert_eq!(
        f.open(r"Ren\Real", NT_KEY_READ).0,
        STATUS_OBJECT_NAME_NOT_FOUND
    );
    let (st, s1) = f.open(r"Ren\RealNew\S1", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(query_dword(s1, "b"), Ok(2));
    close(s1);
    let (st, deep) = f.open(r"Ren\RealNew\S1\Deep", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(query_dword(deep, "d"), Ok(4));
    close(deep);
    assert_eq!(
        f.open(r"Ren\RealNew\S2", NT_KEY_READ).0,
        STATUS_OBJECT_NAME_NOT_FOUND,
        "a subkey deleted before the rename stays deleted"
    );
    assert_eq!(
        regclient::lookup(&f.canon(r"Ren\RealNew")),
        Ok((Lookup::Present { created: true }, true))
    );
    assert!(
        regclient::key(&f.canon(r"Ren\RealNew\Vol"))
            .unwrap()
            .unwrap()
            .volatile,
        "volatile carries over"
    );
    assert!(
        !regclient::key(&f.canon(r"Ren\RealNew\S1"))
            .unwrap()
            .unwrap()
            .volatile
    );

    // The real registry is unchanged.
    assert!(f.really_exists(r"Ren\Real"));
    assert!(f.really_exists(r"Ren\Real\S1\Deep"));
    assert!(f.really_exists(r"Ren\Real\S2"));
    assert!(!f.really_exists(r"Ren\RealNew"));
    assert_eq!(f.real_value(r"Ren\Real", "c"), "none");
    close(h);
}

#[test]
fn each_write_needs_its_access_right() {
    let (_g, f) = fixture();
    let before = f.real_info("Denied");
    // A pass-through handle, and a synthetic one (the key overlaid first).
    let (st, real_h) = f.open("Denied", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(!is_synthetic_key_handle(real_h));
    regclient::set_value(&f.canon("Denied"), "o", REG_DWORD, &1u32.to_le_bytes()).unwrap();
    let (st, synth) = f.open("Denied", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(is_synthetic_key_handle(synth));
    let t = 0u64.to_le_bytes();
    for h in [real_h, synth] {
        assert_eq!(set_dword(h, "v", 2), STATUS_ACCESS_DENIED);
        assert_eq!(delete_value(h, "v"), STATUS_ACCESS_DENIED);
        assert_eq!(unsafe { NtDeleteKey(h) }, STATUS_ACCESS_DENIED);
        assert_eq!(rename(h, "Elsewhere"), STATUS_ACCESS_DENIED);
        assert_eq!(
            unsafe { NtSetInformationKey(h, 0, t.as_ptr(), 8) },
            STATUS_ACCESS_DENIED
        );
        // The arguments are checked before the access.
        assert_eq!(
            unsafe { NtSetInformationKey(h, 0, t.as_ptr(), 4) },
            STATUS_INFO_LENGTH_MISMATCH
        );
        assert_eq!(
            unsafe { NtSetInformationKey(h, 99, t.as_ptr(), 8) },
            STATUS_INVALID_INFO_CLASS
        );
        assert_eq!(rename(h, ""), STATUS_INVALID_PARAMETER);
        assert_eq!(
            unsafe { NtFlushKey(h) },
            STATUS_SUCCESS,
            "flush needs no right"
        );
    }
    close(real_h);
    close(synth);
    // KEY_SET_VALUE alone does not rename (KEY_WRITE does), nor delete.
    let (st, h) = f.open("Denied", NT_KEY_SET_VALUE);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(rename(h, "Elsewhere"), STATUS_ACCESS_DENIED);
    assert_eq!(unsafe { NtDeleteKey(h) }, STATUS_ACCESS_DENIED);
    close(h);
    let (st, h) = f.open("Denied", NT_KEY_WRITE | NT_KEY_QUERY_VALUE);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(unsafe { NtDeleteKey(h) }, STATUS_ACCESS_DENIED);
    close(h);
    assert_eq!(f.real_value("Denied", "v"), hex(&1u32.to_le_bytes()));
    assert_eq!(f.real_info("Denied"), before);
    assert!(f.really_exists("Denied"));
}

#[test]
fn flush_and_set_information_succeed_without_touching_the_real_key() {
    let (_g, f) = fixture();
    let before = f.real_info("Info");
    let (st, h) = f.open("Info", NT_KEY_ALL_ACCESS);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(!is_synthetic_key_handle(h));
    let t = 0x01D0_0000_0000_0000u64.to_le_bytes();
    assert_eq!(
        unsafe { NtSetInformationKey(h, 0, t.as_ptr(), 8) },
        STATUS_SUCCESS
    );
    assert_eq!(unsafe { NtFlushKey(h) }, STATUS_SUCCESS);
    // On a synthetic handle too.
    let (st, made, _) = create_abs(&f.nt("InfoMade"), NT_KEY_ALL_ACCESS);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(
        unsafe { NtSetInformationKey(made, 0, t.as_ptr(), 8) },
        STATUS_SUCCESS
    );
    assert_eq!(unsafe { NtFlushKey(made) }, STATUS_SUCCESS);
    close(made);
    close(h);
    assert_eq!(f.real_info("Info"), before);
    assert!(!f.really_exists("InfoMade"));
}

#[test]
fn a_dead_director_fails_writes_and_leaves_the_real_key() {
    let (_g, f) = fixture();
    let before = f.real_info("Dead");
    let (st, h) = f.open("Dead", NT_KEY_ALL_ACCESS);
    assert_eq!(st, STATUS_SUCCESS);
    let (st, del) = f.open(r"Del\Dead", NT_KEY_ALL_ACCESS);
    assert_eq!(st, STATUS_SUCCESS);
    let host = f.fake.director().registry().unwrap();
    f.fake.director().set_registry(None);
    let results = [
        set_dword(h, "v", 9),
        set_dword(h, "brandnew", 9),
        delete_value(h, "v"),
        rename(h, "DeadNew"),
        unsafe { NtDeleteKey(del) },
    ];
    f.fake.director().set_registry(Some(host));
    assert_eq!(results, [STATUS_UNSUCCESSFUL; 5]);
    close(h);
    close(del);
    assert_eq!(f.real_value("Dead", "v"), hex(&1u32.to_le_bytes()));
    assert_eq!(f.real_value("Dead", "brandnew"), "none");
    assert_eq!(f.real_info("Dead"), before);
    assert!(f.really_exists(r"Del\Dead"));
    assert!(!f.really_exists("DeadNew"));
}
