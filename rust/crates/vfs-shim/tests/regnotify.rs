//! Registry change notifications, the out-of-scope calls, key security, handle flags and the
//! registry stats (registry overlay spec sections 3.4, 3.6 and 6), through the real ntdll entry
//! points with the shim's detours installed and a real director registry behind the ring
//! (`fakedirector` with [`Fake::with_registry`]).
//!
//! Real scratch keys live under `HKCU\Software\AetherVfsRegNotifyTest`, made before the hooks go
//! in. What the *real* registry holds afterwards is asked of a checker: this same test binary,
//! started before the hooks were installed (so it has none), running the ignored `reg_checker`
//! test, which answers over its stdin and stdout.
//!
//! The ntdll calls are looked up at run time: Wine's ntdll lacks some of them, and an import of
//! a missing export would stop the binary loading at all.
//!
//! Every test takes [`LOCK`]: they share one director, and one of them detaches its registry.
#![cfg(windows)]

mod fakedirector;

use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use fakedirector::Fake;
use vfs_shim::{
    install, is_synthetic_key_handle, reg_notify_count, reg_read_fallback_count,
    reg_write_refused_count, regclient, registry_handle_path, registry_notify_pending, Engine,
    RegNotify,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetHandleInformation, LocalFree, SetHandleInformation, FILETIME, HANDLE,
    HANDLE_FLAG_INHERIT, HANDLE_FLAG_PROTECT_FROM_CLOSE,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegQueryInfoKeyW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_READ, REG_OPTION_NON_VOLATILE,
};
use windows_sys::Win32::System::Threading::{CreateEventW, SleepEx, WaitForSingleObject};

static LOCK: Mutex<()> = Mutex::new(());

const BASE: &str = r"Software\AetherVfsRegNotifyTest";
const CHECKER_ENV: &str = "AETHER_VFS_REGNOTIFY_CHECKER";

const STATUS_SUCCESS: i32 = 0;
const STATUS_PENDING: i32 = 0x103;
const STATUS_UNSUCCESSFUL: i32 = 0xC000_0001u32 as i32;
const STATUS_NOTIFY_CLEANUP: i32 = 0x10B;
const STATUS_NOTIFY_ENUM_DIR: i32 = 0x10C;
const STATUS_ACCESS_VIOLATION: i32 = 0xC000_0005u32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
const STATUS_NOT_SUPPORTED: i32 = 0xC000_00BBu32 as i32;
const STATUS_INVALID_BUFFER_SIZE: i32 = 0xC000_0206u32 as i32;
const STATUS_HANDLE_NOT_CLOSABLE: i32 = 0xC000_0235u32 as i32;

const NT_KEY_QUERY_VALUE: u32 = 0x1;
const NT_KEY_SET_VALUE: u32 = 0x2;
const NT_KEY_NOTIFY: u32 = 0x10;
const NT_KEY_READ: u32 = 0x2_0019;
const NT_KEY_ALL_ACCESS: u32 = 0xF_003F;
const WRITE_DAC: u32 = 0x4_0000;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
const OBJECT_HANDLE_FLAG_INFORMATION: u32 = 4;
const REG_NOTIFY_CHANGE_NAME: u32 = 1;
const REG_NOTIFY_CHANGE_LAST_SET: u32 = 4;
const REG_DWORD: u32 = 4;
const REG_CREATED_NEW_KEY: u32 = 1;
const OWNER_GROUP_DACL: u32 = 0x7;
const DACL_SECURITY_INFORMATION: u32 = 0x4;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x102;

/// Long enough for several notifier polls (250 ms each) to have come and gone.
const QUIET: u32 = 1200;
/// How long a notification that must fire may take.
const FIRES: u32 = 5000;

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

/// `IO_STATUS_BLOCK`.
#[repr(C)]
struct Iosb {
    status: usize,
    information: usize,
}

const IOSB_UNTOUCHED: usize = 0x7777_7777;

fn iosb() -> Iosb {
    Iosb {
        status: IOSB_UNTOUCHED,
        information: 0x5555,
    }
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
    fn NtSetValueKey(
        h: isize,
        name: *const UnicodeString,
        title_index: u32,
        ty: u32,
        data: *const u8,
        size: u32,
    ) -> i32;
}

type ApcFn = unsafe extern "system" fn(*mut c_void, *mut Iosb, u32);

type NotifyFn = unsafe extern "system" fn(
    isize,
    isize,
    Option<ApcFn>,
    *mut c_void,
    *mut Iosb,
    u32,
    u8,
    *mut c_void,
    u32,
    u8,
) -> i32;

type NotifyMultipleFn = unsafe extern "system" fn(
    isize,
    u32,
    *const ObjectAttributes,
    isize,
    Option<ApcFn>,
    *mut c_void,
    *mut Iosb,
    u32,
    u8,
    *mut c_void,
    u32,
    u8,
) -> i32;

/// An ntdll export, if this ntdll has it.
fn ntdll(name: &str) -> Option<usize> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    let c = std::ffi::CString::new(name).unwrap();
    unsafe {
        let m = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        GetProcAddress(m, c.as_ptr().cast()).map(|p| p as usize)
    }
}

/// An ntdll export this test needs, as `T`.
fn ntfn<T: Copy>(name: &str) -> T {
    let p = ntdll(name).unwrap_or_else(|| panic!("ntdll has no {name}"));
    assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<usize>());
    unsafe { std::mem::transmute_copy(&p) }
}

fn nt_notify() -> NotifyFn {
    ntfn("NtNotifyChangeKey")
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

/// `f` with `OBJECT_ATTRIBUTES` naming `name` relative to `root` (0: absolute).
fn with_oa<R>(root: isize, name: &str, f: impl FnOnce(*const ObjectAttributes) -> R) -> R {
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

fn open_abs(name: &str, access: u32) -> (i32, isize) {
    let mut h = 0isize;
    let st = with_oa(0, name, |oa| unsafe { NtOpenKeyEx(&mut h, access, oa, 0) });
    (st, h)
}

fn close(h: isize) -> i32 {
    unsafe { NtClose(h) }
}

fn set_dword(h: isize, name: &str, v: u32) -> i32 {
    with_us(name, |us| unsafe {
        NtSetValueKey(h, us, 0, REG_DWORD, v.to_le_bytes().as_ptr(), 4)
    })
}

fn event() -> isize {
    // Manual reset, created signalled: registration must reset it.
    unsafe { CreateEventW(std::ptr::null(), 1, 1, std::ptr::null()) as isize }
}

fn wait(ev: isize, ms: u32) -> u32 {
    unsafe { WaitForSingleObject(ev as HANDLE, ms) }
}

/// An asynchronous event-form notification on `h`.
fn notify_event(h: isize, ev: isize, subtree: bool, io: &mut Iosb) -> i32 {
    unsafe {
        nt_notify()(
            h,
            ev,
            None,
            std::ptr::null_mut(),
            io,
            REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_CHANGE_NAME,
            subtree as u8,
            std::ptr::null_mut(),
            0,
            1,
        )
    }
}

/// What APCs ran: (ApcContext, IoStatusBlock, the block's status when the APC ran).
static APCS: Mutex<Vec<(usize, usize, usize)>> = Mutex::new(Vec::new());

unsafe extern "system" fn apc(ctx: *mut c_void, io: *mut Iosb, _reserved: u32) {
    let status = (*io).status;
    APCS.lock()
        .unwrap()
        .push((ctx as usize, io as usize, status));
}

/// Wait alertably until an APC with `ctx` ran on this thread, or `ms` passed.
fn apc_ran(ctx: usize, ms: u64) -> Option<(usize, usize, usize)> {
    let end = Instant::now() + Duration::from_millis(ms);
    loop {
        if let Some(a) = APCS.lock().unwrap().iter().find(|a| a.0 == ctx) {
            return Some(*a);
        }
        if Instant::now() >= end {
            return None;
        }
        unsafe { SleepEx(50, 1) };
    }
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
    /// A directory outside the VFS root, for the hive files the save and load calls take.
    files: std::path::PathBuf,
    /// The stats report the shim writes.
    report: std::path::PathBuf,
}

impl Fixture {
    fn nt(&self, rel: &str) -> String {
        format!(r"\REGISTRY\USER\{}\{BASE}\{rel}", self.sid)
    }

    fn canon(&self, rel: &str) -> String {
        format!(r"\Registry\User\<CurrentUser>\{BASE}\{rel}")
    }

    fn open(&self, rel: &str, access: u32) -> isize {
        let (st, h) = open_abs(&self.nt(rel), access);
        assert_eq!(st, STATUS_SUCCESS, "open {rel}");
        h
    }

    /// Give `rel` overlay content (a value), so an open of it is synthetic.
    fn touch(&self, rel: &str) {
        regclient::set_value(&self.canon(rel), "touched", REG_DWORD, &[1, 0, 0, 0]).unwrap();
    }

    fn ask(&self, q: &str) -> String {
        let mut c = self.checker.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(c.stdin, "{q}").unwrap();
        c.stdin.flush().unwrap();
        loop {
            let mut line = String::new();
            assert_ne!(c.stdout.read_line(&mut line).unwrap(), 0, "checker exited");
            if let Some(i) = line.find("CHECK:") {
                return line[i + 6..].trim().to_string();
            }
        }
    }

    /// Whether the key exists in the real registry.
    fn really_exists(&self, rel: &str) -> bool {
        self.ask(&format!(r"K|{BASE}\{rel}")) == "1"
    }

    /// The real key's last-write time, its value count and its subkey count.
    fn real_info(&self, rel: &str) -> String {
        self.ask(&format!(r"I|{BASE}\{rel}"))
    }

    /// A new file for a save or restore call (outside the VFS root, so the file hooks leave it
    /// to the real disk).
    fn file(&self, name: &str) -> isize {
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ,
            FILE_GENERIC_WRITE,
        };
        let p = self.files.join(name);
        let w: Vec<u16> = p.to_str().unwrap().encode_utf16().chain(Some(0)).collect();
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                std::ptr::null(),
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        assert!(h as isize > 0, "create {}", p.display());
        h as isize
    }
}

fn fixture() -> (MutexGuard<'static, ()>, &'static Fixture) {
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    static F: OnceLock<Fixture> = OnceLock::new();
    let f = F.get_or_init(|| {
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(BASE).as_ptr()) };
        for k in [
            "Syn",
            "Apc",
            "Pass",
            "Close",
            "Sync",
            "Multi",
            "Dead",
            "Unsup",
            "UnsupReal",
            "Sec",
            "SecReal",
            "Flags",
            "EvGone",
            "Protect",
            "Loader",
        ] {
            real_key(k, &[("v", 1)]);
        }
        real_key(r"Tree\Sub", &[("v", 1)]);
        real_key(r"Unsup\Child", &[]);

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

        let tmp = std::env::temp_dir();
        let root = tmp.join(format!("vfs-shim-regnotify-{}", std::process::id()));
        let files = tmp.join(format!("vfs-shim-regnotify-files-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&files).unwrap();
        let report = files.join("shim-stats.log");
        std::env::set_var(vfs_env::SHIM_STATS_LOG, &report);
        std::env::set_var(vfs_env::SHIM_STATS_INTERVAL_MS, "50");
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
        assert_eq!(
            vfs_shim::reg_overlay_disabled_by(),
            None,
            "every registry detour is in"
        );
        assert!(regclient::enabled());
        Fixture {
            fake,
            sid: user_sid(),
            checker: Mutex::new(Checker {
                _child: child,
                stdin,
                stdout,
            }),
            files,
            report,
        }
    });
    (guard, f)
}

/// Not a test of its own: the checker process [`Fixture::ask`] asks. Each stdin line is a
/// query on the real registry under HKCU: `K|key` (exists: 1 or 0), `I|key` (last-write time,
/// value and subkey counts).
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
fn a_notify_on_a_synthetic_key_fires_on_an_overlay_write() {
    let (_g, f) = fixture();
    f.touch("Syn");
    let h = f.open("Syn", NT_KEY_READ);
    assert!(is_synthetic_key_handle(h));
    let w = f.open("Syn", NT_KEY_SET_VALUE);
    let ev = event();
    let mut io = iosb();
    let registered = reg_notify_count(RegNotify::Registered);
    let completed = reg_notify_count(RegNotify::Completed);

    assert_eq!(notify_event(h, ev, false, &mut io), STATUS_PENDING);
    assert_eq!(wait(ev, 0), WAIT_TIMEOUT, "registration resets the event");
    assert_eq!(reg_notify_count(RegNotify::Registered), registered + 1);
    assert!(registry_notify_pending() >= 1);
    assert_eq!(wait(ev, QUIET), WAIT_TIMEOUT, "no change, no notification");

    assert_eq!(set_dword(w, "new", 5), STATUS_SUCCESS);
    assert_eq!(
        wait(ev, FIRES),
        WAIT_OBJECT_0,
        "the overlay write completes it"
    );
    assert_eq!(reg_notify_count(RegNotify::Completed), completed + 1);
    // An event and no APC: the status block is not written (Wine never writes it, and its own
    // `RegNotifyChangeKeyValue` passes one on a stack that is gone by now).
    assert_eq!(io.status, IOSB_UNTOUCHED);

    // One-shot: a second write does not signal it again once reset.
    unsafe { windows_sys::Win32::System::Threading::ResetEvent(ev as HANDLE) };
    assert_eq!(set_dword(w, "new", 6), STATUS_SUCCESS);
    assert_eq!(wait(ev, QUIET), WAIT_TIMEOUT);

    // Without KEY_NOTIFY the call is refused, as on Windows.
    let q = f.open("Syn", NT_KEY_QUERY_VALUE);
    assert!(is_synthetic_key_handle(q));
    assert_eq!(notify_event(q, ev, false, &mut io), STATUS_ACCESS_DENIED);
    close(q);
    close(w);
    close(h);
    unsafe { CloseHandle(ev as HANDLE) };
}

#[test]
fn a_notify_with_an_apc_runs_it_on_the_calling_thread() {
    let (_g, f) = fixture();
    f.touch("Apc");
    let h = f.open("Apc", NT_KEY_READ | NT_KEY_SET_VALUE);
    assert!(is_synthetic_key_handle(h));
    let mut io = iosb();
    let ctx = 0xA9C1usize;
    let st = unsafe {
        nt_notify()(
            h,
            0,
            Some(apc),
            ctx as *mut c_void,
            &mut io,
            REG_NOTIFY_CHANGE_LAST_SET,
            0,
            std::ptr::null_mut(),
            0,
            1,
        )
    };
    assert_eq!(st, STATUS_PENDING);
    assert_eq!(apc_ran(ctx, QUIET as u64), None, "nothing changed yet");
    assert_eq!(set_dword(h, "x", 1), STATUS_SUCCESS);
    let (_, at, status) = apc_ran(ctx, FIRES as u64).expect("the APC ran");
    assert_eq!(
        at, &mut io as *mut Iosb as usize,
        "called with the status block"
    );
    assert_eq!(
        status as i32, STATUS_NOTIFY_ENUM_DIR,
        "written before the APC"
    );
    assert_eq!(io.information, 0);

    // No event and no APC: the status block is the only way to learn of it, so it is written.
    let mut io2 = iosb();
    let st = unsafe {
        nt_notify()(
            h,
            0,
            None,
            std::ptr::null_mut(),
            &mut io2,
            REG_NOTIFY_CHANGE_LAST_SET,
            0,
            std::ptr::null_mut(),
            0,
            1,
        )
    };
    assert_eq!(st, STATUS_PENDING);
    assert_eq!(set_dword(h, "x", 2), STATUS_SUCCESS);
    let end = Instant::now() + Duration::from_millis(FIRES as u64);
    while unsafe { std::ptr::read_volatile(&io2.status) } == IOSB_UNTOUCHED && Instant::now() < end
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(io2.status as i32, STATUS_NOTIFY_ENUM_DIR);
    close(h);
}

#[test]
fn a_notify_on_a_pass_through_key_fires_on_a_write_through_the_same_handle() {
    let (_g, f) = fixture();
    let before = f.real_info("Pass");
    let h = f.open("Pass", NT_KEY_READ | NT_KEY_SET_VALUE);
    assert!(!is_synthetic_key_handle(h), "untouched: the real handle");
    let ev = event();
    let mut io = iosb();
    assert_eq!(notify_event(h, ev, false, &mut io), STATUS_PENDING);
    assert_eq!(wait(ev, QUIET), WAIT_TIMEOUT);
    // Copy-on-write: the write goes to the overlay, never to the real key.
    assert_eq!(set_dword(h, "cow", 3), STATUS_SUCCESS);
    assert_eq!(wait(ev, FIRES), WAIT_OBJECT_0);
    assert_eq!(f.real_info("Pass"), before, "the real key is unchanged");
    close(h);
    unsafe { CloseHandle(ev as HANDLE) };
}

#[test]
fn watch_tree_sees_subkey_writes_and_a_plain_watch_does_not() {
    let (_g, f) = fixture();
    // The subkey is in the overlay before the watches start, so the write below changes only
    // the subkey (the first overlay touch of a real subkey also adds it to its parent's list).
    regclient::set_value(&f.canon(r"Tree\Sub"), "pre", REG_DWORD, &[0; 4]).unwrap();
    let h = f.open("Tree", NT_KEY_READ | NT_KEY_SET_VALUE);
    let sub = f.open(r"Tree\Sub", NT_KEY_SET_VALUE);
    let (plain, tree) = (event(), event());
    let (mut io1, mut io2) = (iosb(), iosb());
    assert_eq!(notify_event(h, plain, false, &mut io1), STATUS_PENDING);
    assert_eq!(notify_event(h, tree, true, &mut io2), STATUS_PENDING);

    assert_eq!(set_dword(sub, "deep", 1), STATUS_SUCCESS);
    assert_eq!(
        wait(tree, FIRES),
        WAIT_OBJECT_0,
        "WatchTree sees the subkey"
    );
    assert_eq!(wait(plain, QUIET), WAIT_TIMEOUT, "a plain watch does not");

    // The plain watch is still pending, and fires for the key itself.
    assert_eq!(set_dword(h, "own", 1), STATUS_SUCCESS);
    assert_eq!(wait(plain, FIRES), WAIT_OBJECT_0);
    close(sub);
    close(h);
    unsafe {
        CloseHandle(plain as HANDLE);
        CloseHandle(tree as HANDLE);
    }
}

#[test]
fn closing_the_handle_ends_a_pending_notify_with_notify_cleanup() {
    let (_g, f) = fixture();
    f.touch("Close");
    let h = f.open("Close", NT_KEY_READ);
    let ev = event();
    let mut io = iosb();
    let ctx = 0xC105Eusize;
    let cleaned = reg_notify_count(RegNotify::CleanedUp);
    let st = unsafe {
        nt_notify()(
            h,
            ev,
            Some(apc),
            ctx as *mut c_void,
            &mut io,
            REG_NOTIFY_CHANGE_NAME,
            1,
            std::ptr::null_mut(),
            0,
            1,
        )
    };
    assert_eq!(st, STATUS_PENDING);
    assert_eq!(close(h), STATUS_SUCCESS);
    assert_eq!(wait(ev, 0), WAIT_OBJECT_0, "signalled by the close itself");
    let (_, _, status) = apc_ran(ctx, FIRES as u64).expect("the APC ran");
    assert_eq!(status as i32, STATUS_NOTIFY_CLEANUP);
    assert_eq!(reg_notify_count(RegNotify::CleanedUp), cleaned + 1);
    unsafe { CloseHandle(ev as HANDLE) };
}

#[test]
fn a_synchronous_notify_blocks_until_a_change_or_a_close() {
    let (_g, f) = fixture();
    let sync_notify = |h: isize| {
        std::thread::spawn(move || {
            let mut io = iosb();
            let st = unsafe {
                nt_notify()(
                    h,
                    0,
                    None,
                    std::ptr::null_mut(),
                    &mut io,
                    REG_NOTIFY_CHANGE_LAST_SET,
                    0,
                    std::ptr::null_mut(),
                    0,
                    0,
                )
            };
            (st, io.status as i32)
        })
    };
    // A change: STATUS_SUCCESS, as Wine's synchronous call returns.
    let h = f.open("Sync", NT_KEY_READ | NT_KEY_SET_VALUE);
    let t = sync_notify(h);
    std::thread::sleep(Duration::from_millis(QUIET as u64));
    assert!(!t.is_finished(), "blocked while nothing changes");
    assert_eq!(set_dword(h, "s", 1), STATUS_SUCCESS);
    assert_eq!(t.join().unwrap(), (STATUS_SUCCESS, STATUS_SUCCESS));

    // A close: STATUS_NOTIFY_CLEANUP.
    let t = sync_notify(h);
    std::thread::sleep(Duration::from_millis(400));
    assert!(!t.is_finished());
    assert_eq!(close(h), STATUS_SUCCESS);
    assert_eq!(
        t.join().unwrap(),
        (STATUS_NOTIFY_CLEANUP, STATUS_NOTIFY_CLEANUP)
    );
}

#[test]
fn notify_change_multiple_keys_refuses_subordinate_keys_on_a_served_key() {
    let (_g, f) = fixture();
    f.touch("Multi");
    let h = f.open("Multi", NT_KEY_READ | NT_KEY_SET_VALUE);
    let multi: NotifyMultipleFn = ntfn("NtNotifyChangeMultipleKeys");
    let ev = event();
    let mut io = iosb();
    let st = with_oa(0, &f.nt("Sec"), |oa| unsafe {
        multi(
            h,
            1,
            oa,
            ev,
            None,
            std::ptr::null_mut(),
            &mut io,
            REG_NOTIFY_CHANGE_LAST_SET,
            0,
            std::ptr::null_mut(),
            0,
            1,
        )
    });
    assert_eq!(st, STATUS_NOT_SUPPORTED);
    // Count 0 is NtNotifyChangeKey.
    let st = unsafe {
        multi(
            h,
            0,
            std::ptr::null(),
            ev,
            None,
            std::ptr::null_mut(),
            &mut io,
            REG_NOTIFY_CHANGE_LAST_SET,
            0,
            std::ptr::null_mut(),
            0,
            1,
        )
    };
    assert_eq!(st, STATUS_PENDING);
    assert_eq!(set_dword(h, "m", 1), STATUS_SUCCESS);
    assert_eq!(wait(ev, FIRES), WAIT_OBJECT_0);
    close(h);
    unsafe { CloseHandle(ev as HANDLE) };
}

#[test]
fn a_waiter_rides_out_a_director_failure_without_firing() {
    let (_g, f) = fixture();
    let host = f.fake.director().registry().unwrap();
    f.fake.director().set_registry(None);
    // With the director gone the key opens as the real handle; the notification still becomes
    // an overlay waiter, with no version to watch from yet.
    let h = f.open("Dead", NT_KEY_READ | NT_KEY_SET_VALUE);
    let errors = reg_notify_count(RegNotify::PollError);
    let ev = event();
    let mut io = iosb();
    assert_eq!(notify_event(h, ev, false, &mut io), STATUS_PENDING);
    assert_eq!(wait(ev, QUIET), WAIT_TIMEOUT, "no spurious completion");
    assert!(
        reg_notify_count(RegNotify::PollError) >= errors + 2,
        "the failed polls are counted"
    );
    // A write now is refused, and counted as a refused write, not as a read fallback.
    let (refused, fallbacks) = (reg_write_refused_count(), reg_read_fallback_count());
    assert_eq!(set_dword(h, "lost", 1), STATUS_UNSUCCESSFUL);
    assert_eq!(reg_write_refused_count(), refused + 1);
    assert_eq!(reg_read_fallback_count(), fallbacks);
    f.fake.director().set_registry(Some(host));
    // The first answered poll sets the version; it does not complete the waiter.
    assert_eq!(wait(ev, QUIET), WAIT_TIMEOUT);
    assert_eq!(set_dword(h, "back", 1), STATUS_SUCCESS);
    assert_eq!(wait(ev, FIRES), WAIT_OBJECT_0);
    close(h);
    unsafe { CloseHandle(ev as HANDLE) };
}

type SaveKeyFn = unsafe extern "system" fn(isize, isize) -> i32;
type SaveKeyExFn = unsafe extern "system" fn(isize, isize, u32) -> i32;
type SaveMergedFn = unsafe extern "system" fn(isize, isize, isize) -> i32;
type RestoreKeyFn = unsafe extern "system" fn(isize, isize, u32) -> i32;
type KeyOnlyFn = unsafe extern "system" fn(isize) -> i32;
type ReplaceKeyFn =
    unsafe extern "system" fn(*const ObjectAttributes, isize, *const ObjectAttributes) -> i32;
type LoadKeyFn = unsafe extern "system" fn(*const ObjectAttributes, *const ObjectAttributes) -> i32;
type LoadKey2Fn =
    unsafe extern "system" fn(*const ObjectAttributes, *const ObjectAttributes, u32) -> i32;
type UnloadKeyFn = unsafe extern "system" fn(*const ObjectAttributes) -> i32;
type CreateKeyTxFn = unsafe extern "system" fn(
    *mut isize,
    u32,
    *const ObjectAttributes,
    u32,
    *const UnicodeString,
    u32,
    isize,
    *mut u32,
) -> i32;
type OpenKeyTxFn =
    unsafe extern "system" fn(*mut isize, u32, *const ObjectAttributes, isize) -> i32;
type OpenKeyTxExFn =
    unsafe extern "system" fn(*mut isize, u32, *const ObjectAttributes, u32, isize) -> i32;

/// The calls that would write the real key, on a key `root`/`name` names (`root` 0: absolute)
/// and through handle `h`: each refused.
fn assert_writes_refused(f: &Fixture, h: isize, root: isize, name: &str) {
    let restore: RestoreKeyFn = ntfn("NtRestoreKey");
    let replace: ReplaceKeyFn = ntfn("NtReplaceKey");
    let load: LoadKeyFn = ntfn("NtLoadKey");
    let load2: LoadKey2Fn = ntfn("NtLoadKey2");
    let unload: UnloadKeyFn = ntfn("NtUnloadKey");
    let create_tx: CreateKeyTxFn = ntfn("NtCreateKeyTransacted");
    let open_tx: OpenKeyTxFn = ntfn("NtOpenKeyTransacted");
    let open_tx_ex: OpenKeyTxExFn = ntfn("NtOpenKeyTransactedEx");
    let file = f.file("restore.hiv");
    let hive = f.files.join("load.hiv");
    let hive = format!(r"\??\{}", hive.to_str().unwrap());
    unsafe {
        assert_eq!(restore(h, file, 0), STATUS_NOT_SUPPORTED, "NtRestoreKey");
        with_oa(0, &hive, |new| {
            with_oa(0, &hive, |old| {
                assert_eq!(replace(new, h, old), STATUS_NOT_SUPPORTED, "NtReplaceKey")
            })
        });
        let child = if name.is_empty() {
            "Loaded".to_string()
        } else {
            format!(r"{name}\Loaded")
        };
        with_oa(root, &child, |target| {
            with_oa(0, &hive, |src| {
                assert_eq!(load(target, src), STATUS_NOT_SUPPORTED, "NtLoadKey");
                assert_eq!(load2(target, src, 0), STATUS_NOT_SUPPORTED, "NtLoadKey2");
            })
        });
        with_oa(root, name, |target| {
            assert_eq!(unload(target), STATUS_NOT_SUPPORTED, "NtUnloadKey");
        });
        let tx_name = if name.is_empty() {
            "TxNew".to_string()
        } else {
            format!(r"{name}\TxNew")
        };
        let mut out = 0isize;
        let mut disp = 0u32;
        with_oa(root, &tx_name, |oa| {
            assert_eq!(
                create_tx(
                    &mut out,
                    NT_KEY_ALL_ACCESS,
                    oa,
                    0,
                    std::ptr::null(),
                    0,
                    0,
                    &mut disp
                ),
                STATUS_NOT_SUPPORTED,
                "NtCreateKeyTransacted"
            )
        });
        with_oa(root, name, |oa| {
            assert_eq!(
                open_tx(&mut out, NT_KEY_READ, oa, 0),
                STATUS_NOT_SUPPORTED,
                "NtOpenKeyTransacted"
            );
            assert_eq!(
                open_tx_ex(&mut out, NT_KEY_READ, oa, 0, 0),
                STATUS_NOT_SUPPORTED,
                "NtOpenKeyTransactedEx"
            );
        });
        CloseHandle(file as HANDLE);
    }
}

#[test]
fn the_out_of_scope_calls_are_refused_on_keys_the_overlay_serves() {
    let (_g, f) = fixture();
    // A synthetic key: every one of them.
    f.touch("Unsup");
    let hs = f.open("Unsup", NT_KEY_ALL_ACCESS);
    assert!(is_synthetic_key_handle(hs));
    let file = f.file("save.hiv");
    unsafe {
        let save: SaveKeyFn = ntfn("NtSaveKey");
        assert_eq!(save(hs, file), STATUS_NOT_SUPPORTED, "NtSaveKey");
        if let Some(p) = ntdll("NtSaveKeyEx") {
            let save_ex: SaveKeyExFn = std::mem::transmute(p);
            assert_eq!(save_ex(hs, file, 1), STATUS_NOT_SUPPORTED, "NtSaveKeyEx");
        }
        if let Some(p) = ntdll("NtSaveMergedKeys") {
            let merged: SaveMergedFn = std::mem::transmute(p);
            assert_eq!(
                merged(hs, hs, file),
                STATUS_NOT_SUPPORTED,
                "NtSaveMergedKeys"
            );
        }
        for name in ["NtCompressKey", "NtLockRegistryKey"] {
            if let Some(p) = ntdll(name) {
                let k: KeyOnlyFn = std::mem::transmute(p);
                assert_eq!(k(hs), STATUS_NOT_SUPPORTED, "{name}");
            }
        }
        CloseHandle(file as HANDLE);
    }
    // Relative to the synthetic handle, and through it.
    assert_writes_refused(f, hs, hs, "");
    assert_writes_refused(f, hs, hs, "Child");

    // A real (pass-through) key on a virtualised path: the calls that would write it.
    let before = f.real_info("UnsupReal");
    let hp = f.open("UnsupReal", NT_KEY_ALL_ACCESS);
    assert!(!is_synthetic_key_handle(hp));
    assert_writes_refused(f, hp, 0, &f.nt("UnsupReal"));
    assert_writes_refused(f, hp, hp, "");
    assert_eq!(
        f.real_info("UnsupReal"),
        before,
        "the real key is unchanged"
    );
    for k in [
        r"UnsupReal\Loaded",
        r"UnsupReal\TxNew",
        r"Unsup\TxNew",
        r"Unsup\Loaded",
    ] {
        assert!(!f.really_exists(k), "{k} was not made for real");
    }
    // Saving reads the real key: passed through (Wine's own answer, whatever it is).
    let file = f.file("save-real.hiv");
    let save: SaveKeyFn = ntfn("NtSaveKey");
    let saved = unsafe { save(hp, file) };
    assert_ne!(saved, STATUS_NOT_SUPPORTED);
    unsafe { CloseHandle(file as HANDLE) };

    // With the hooks bypassed (the shim's own work on this thread) and the overlay on, the calls
    // that would change the real key fail closed rather than reach it; a save still passes
    // through.
    let restore_file = f.file("restore-bypassed.hiv");
    let save_file = f.file("save-bypassed.hiv");
    let sd = sddl("O:BAG:BAD:(A;;KA;;;WD)");
    let (restored, secured, saved_bypassed) = vfs_shim::as_shim_io_for_tests(|| unsafe {
        let restore: RestoreKeyFn = ntfn("NtRestoreKey");
        let set: SetSecurityFn = ntfn("NtSetSecurityObject");
        (
            restore(hp, restore_file, 0),
            set(hp, 4 /* DACL_SECURITY_INFORMATION */, sd),
            save(hp, save_file),
        )
    });
    assert_eq!(restored, STATUS_UNSUCCESSFUL, "bypassed NtRestoreKey");
    assert_eq!(secured, STATUS_UNSUCCESSFUL, "bypassed NtSetSecurityObject");
    assert_eq!(saved_bypassed, saved, "a bypassed NtSaveKey is the real call");
    unsafe {
        windows_sys::Win32::Foundation::LocalFree(sd);
        CloseHandle(restore_file as HANDLE);
        CloseHandle(save_file as HANDLE);
    }
    assert_eq!(
        f.real_info("UnsupReal"),
        before,
        "the real key is unchanged"
    );

    // A handle that is not a key, and a name outside the virtualised hives, are not ours.
    let restore: RestoreKeyFn = ntfn("NtRestoreKey");
    let ev = event();
    assert_ne!(unsafe { restore(ev, 0, 0) }, STATUS_NOT_SUPPORTED);
    unsafe { CloseHandle(ev as HANDLE) };
    let open_tx_ex: OpenKeyTxExFn = ntfn("NtOpenKeyTransactedEx");
    let mut out = 0isize;
    let st = with_oa(0, r"\Registry", |oa| unsafe {
        open_tx_ex(&mut out, NT_KEY_READ, oa, 0, 0)
    });
    assert_ne!(st, STATUS_NOT_SUPPORTED);
    if st >= 0 {
        close(out);
    }
    close(hp);
    close(hs);
}

type QuerySecurityFn = unsafe extern "system" fn(isize, u32, *mut u8, u32, *mut u32) -> i32;
type SetSecurityFn = unsafe extern "system" fn(isize, u32, *const c_void) -> i32;

fn query_sd(h: isize, info: u32) -> Result<Vec<u8>, i32> {
    let q: QuerySecurityFn = ntfn("NtQuerySecurityObject");
    let mut buf = vec![0u8; 4096];
    let mut need = 0u32;
    let st = unsafe { q(h, info, buf.as_mut_ptr(), 4096, &mut need) };
    if st != STATUS_SUCCESS {
        return Err(st);
    }
    buf.truncate(need as usize);
    Ok(buf)
}

/// A self-relative security descriptor from SDDL (freed with `LocalFree`).
fn sddl(s: &str) -> *mut c_void {
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    let mut sd: *mut c_void = std::ptr::null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(s).as_ptr(),
            1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "SDDL {s}");
    sd
}

#[test]
fn security_on_a_synthetic_key_is_the_real_keys_and_a_set_is_ignored() {
    let (_g, f) = fixture();
    let set: SetSecurityFn = ntfn("NtSetSecurityObject");
    // The real key's descriptor, through a real handle before the overlay touches it.
    let hr = f.open("Sec", NT_KEY_READ);
    assert!(!is_synthetic_key_handle(hr));
    let real_sd = query_sd(hr, OWNER_GROUP_DACL).expect("real descriptor");
    assert!(!real_sd.is_empty());

    f.touch("Sec");
    let hs = f.open("Sec", NT_KEY_READ | WRITE_DAC);
    assert!(is_synthetic_key_handle(hs));
    assert_eq!(query_sd(hs, OWNER_GROUP_DACL), Ok(real_sd.clone()));
    // A key created in the overlay has its parent's descriptor.
    let mut made = 0isize;
    let mut disp = 0u32;
    let st = with_oa(0, &f.nt(r"Sec\Made"), |oa| unsafe {
        NtCreateKey(
            &mut made,
            NT_KEY_ALL_ACCESS,
            oa,
            0,
            std::ptr::null(),
            0,
            &mut disp,
        )
    });
    assert_eq!((st, disp), (STATUS_SUCCESS, REG_CREATED_NEW_KEY));
    assert!(is_synthetic_key_handle(made));
    assert_eq!(query_sd(made, OWNER_GROUP_DACL), Ok(real_sd.clone()));
    // The handle needs READ_CONTROL, as on Windows.
    let weak = f.open("Sec", NT_KEY_QUERY_VALUE | NT_KEY_NOTIFY);
    assert!(is_synthetic_key_handle(weak));
    assert_eq!(query_sd(weak, OWNER_GROUP_DACL), Err(STATUS_ACCESS_DENIED));

    // A set is checked, accepted and ignored.
    let everyone = sddl("D:P(A;;KA;;;WD)");
    unsafe {
        assert_eq!(set(hs, DACL_SECURITY_INFORMATION, everyone), STATUS_SUCCESS);
        assert_eq!(
            set(weak, DACL_SECURITY_INFORMATION, everyone),
            STATUS_ACCESS_DENIED
        );
        assert_eq!(
            set(hs, DACL_SECURITY_INFORMATION, std::ptr::null()),
            STATUS_ACCESS_VIOLATION
        );
    }
    assert_eq!(query_sd(hs, OWNER_GROUP_DACL), Ok(real_sd.clone()));
    assert_eq!(query_sd(hr, OWNER_GROUP_DACL), Ok(real_sd.clone()));

    // A real handle on an untouched virtualised key: the query is the real call, and a set is
    // a write to the real registry, so it is accepted and ignored too.
    let hp = f.open("SecReal", NT_KEY_ALL_ACCESS);
    assert!(!is_synthetic_key_handle(hp));
    let before = query_sd(hp, OWNER_GROUP_DACL).expect("real descriptor");
    unsafe {
        assert_eq!(set(hp, DACL_SECURITY_INFORMATION, everyone), STATUS_SUCCESS);
        LocalFree(everyone);
    }
    assert_eq!(query_sd(hp, OWNER_GROUP_DACL), Ok(before));
    for h in [hp, weak, made, hs, hr] {
        close(h);
    }
}

#[test]
fn handle_flags_on_a_synthetic_key_round_trip() {
    let (_g, f) = fixture();
    let set_info: unsafe extern "system" fn(isize, u32, *const u8, u32) -> i32 =
        ntfn("NtSetInformationObject");
    f.touch("Flags");
    let h = f.open("Flags", NT_KEY_READ);
    assert!(is_synthetic_key_handle(h));
    let flags = |h: isize| {
        let mut b = [9u8; 2];
        let mut ret = 0u32;
        let st = unsafe {
            NtQueryObject(
                h,
                OBJECT_HANDLE_FLAG_INFORMATION,
                b.as_mut_ptr(),
                2,
                &mut ret,
            )
        };
        assert_eq!(st, STATUS_SUCCESS);
        b
    };
    assert_eq!(flags(h), [0, 0]);
    unsafe {
        assert_eq!(
            set_info(h, OBJECT_HANDLE_FLAG_INFORMATION, [1u8, 0].as_ptr(), 2),
            STATUS_SUCCESS
        );
        assert_eq!(flags(h), [1, 0]);
        assert_eq!(
            set_info(h, OBJECT_HANDLE_FLAG_INFORMATION, [0u8, 1].as_ptr(), 2),
            STATUS_SUCCESS
        );
        assert_eq!(flags(h), [0, 1]);
        // Protected from close: the handle stays.
        assert_eq!(close(h), STATUS_HANDLE_NOT_CLOSABLE);
        assert!(registry_handle_path(h).is_some());
        assert_eq!(
            set_info(h, OBJECT_HANDLE_FLAG_INFORMATION, [0u8].as_ptr(), 1),
            STATUS_INVALID_BUFFER_SIZE
        );
        // The Win32 calls, which are these two underneath.
        let hh = h as HANDLE;
        assert_ne!(
            SetHandleInformation(
                hh,
                HANDLE_FLAG_INHERIT | HANDLE_FLAG_PROTECT_FROM_CLOSE,
                HANDLE_FLAG_INHERIT
            ),
            0
        );
        let mut got = 0u32;
        assert_ne!(GetHandleInformation(hh, &mut got), 0);
        assert_eq!(got, HANDLE_FLAG_INHERIT);
    }
    assert_eq!(flags(h), [1, 0]);
    assert_eq!(close(h), STATUS_SUCCESS);
    assert!(registry_handle_path(h).is_none());
}

#[test]
fn the_stats_report_has_the_registry_rows() {
    let (_g, f) = fixture();
    // Something for each row to count.
    f.touch("Syn");
    let h = f.open("Syn", NT_KEY_READ);
    let ev = event();
    let mut io = iosb();
    assert_eq!(notify_event(h, ev, false, &mut io), STATUS_PENDING);
    let _ = query_sd(h, OWNER_GROUP_DACL);
    close(h);
    unsafe { CloseHandle(ev as HANDLE) };
    let end = Instant::now() + Duration::from_secs(10);
    let want = [
        "NtNotifyChangeKey",
        "NtQuerySecurityObject",
        "registry key handles:",
        "registry notifications:",
    ];
    let mut report = String::new();
    while Instant::now() < end {
        report = std::fs::read_to_string(&f.report).unwrap_or_default();
        if want.iter().all(|w| report.contains(w)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for w in want {
        assert!(
            report.contains(w),
            "no {w:?} in the stats report:\n{report}"
        );
    }
}

#[test]
fn a_caller_that_closes_its_event_first_gets_nothing_else_signalled() {
    let (_g, f) = fixture();
    let h = f.open("EvGone", NT_KEY_READ | NT_KEY_SET_VALUE);
    let ev = event();
    let mut io = iosb();
    assert_eq!(notify_event(h, ev, false, &mut io), STATUS_PENDING);
    let completed = reg_notify_count(RegNotify::Completed);
    // The caller lets go of its event; a new, unsignalled event may well get the same value.
    unsafe { CloseHandle(ev as HANDLE) };
    let other = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) as isize };
    assert_eq!(set_dword(h, "x", 1), STATUS_SUCCESS);
    let end = Instant::now() + Duration::from_millis(FIRES as u64);
    while reg_notify_count(RegNotify::Completed) == completed && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        reg_notify_count(RegNotify::Completed),
        completed + 1,
        "it completed"
    );
    assert_eq!(
        wait(other, 0),
        WAIT_TIMEOUT,
        "the completion signalled the shim's own duplicate, not whatever the value names now \
         (same value: {})",
        other == ev
    );
    close(h);
    unsafe { CloseHandle(other as HANDLE) };
}

#[test]
fn a_real_handle_protected_from_close_keeps_its_notification() {
    let (_g, f) = fixture();
    let h = f.open("Protect", NT_KEY_READ | NT_KEY_SET_VALUE);
    assert!(!is_synthetic_key_handle(h));
    let hh = h as HANDLE;
    let ev = event();
    let mut io = iosb();
    assert_eq!(notify_event(h, ev, false, &mut io), STATUS_PENDING);
    assert_ne!(
        unsafe {
            SetHandleInformation(
                hh,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
                HANDLE_FLAG_PROTECT_FROM_CLOSE,
            )
        },
        0
    );
    assert_eq!(close(h), STATUS_HANDLE_NOT_CLOSABLE);
    assert_eq!(
        wait(ev, 0),
        WAIT_TIMEOUT,
        "not cleaned up: the handle is still open"
    );
    assert!(registry_handle_path(h).is_some(), "its record is back");
    assert_eq!(set_dword(h, "still", 1), STATUS_SUCCESS);
    assert_eq!(
        wait(ev, FIRES),
        WAIT_OBJECT_0,
        "and its notification still fires"
    );
    assert_ne!(
        unsafe { SetHandleInformation(hh, HANDLE_FLAG_PROTECT_FROM_CLOSE, 0) },
        0
    );
    assert_eq!(close(h), STATUS_SUCCESS);
    unsafe { CloseHandle(ev as HANDLE) };
}

#[test]
fn a_synchronous_notify_under_the_loader_lock_is_refused() {
    const STATUS_POSSIBLE_DEADLOCK: i32 = 0xC000_0194u32 as i32;
    let (_g, f) = fixture();
    let h = f.open("Loader", NT_KEY_READ);
    let lock: unsafe extern "system" fn(u32, *mut u32, *mut usize) -> i32 =
        ntfn("LdrLockLoaderLock");
    let unlock: unsafe extern "system" fn(u32, usize) -> i32 = ntfn("LdrUnlockLoaderLock");
    let mut cookie = 0usize;
    assert_eq!(
        unsafe { lock(0, std::ptr::null_mut(), &mut cookie) },
        STATUS_SUCCESS
    );
    let mut io = iosb();
    let st = unsafe {
        nt_notify()(
            h,
            0,
            None,
            std::ptr::null_mut(),
            &mut io,
            REG_NOTIFY_CHANGE_LAST_SET,
            0,
            std::ptr::null_mut(),
            0,
            0,
        )
    };
    assert_eq!(unsafe { unlock(0, cookie) }, STATUS_SUCCESS);
    assert_eq!(st, STATUS_POSSIBLE_DEADLOCK);
    close(h);
}
