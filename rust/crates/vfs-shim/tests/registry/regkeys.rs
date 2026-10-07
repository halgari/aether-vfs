//! The registry hooks' key handles: open, create, duplicate, close and the object name
//! (registry overlay spec sections 3.1, 3.2 and 6), through the real ntdll entry points with
//! the shim's detours installed and a real director registry behind the ring
//! (`fakedirector` with [`Fake::with_registry`]).
//!
//! Real scratch keys live under `HKCU\Software\AetherVfsRegKeysTest`, made before the hooks go
//! in (afterwards no write reaches the real registry, which is the point). Whether a key exists
//! *for real* is asked of a checker: this same test binary, started before the hooks were
//! installed (so it has none), running the ignored `reg_checker` test (see `common/reg.rs`),
//! which answers over its stdin and stdout.
//!
//! Every test takes [`LOCK`]: they share one director, and one of them detaches its registry.

use crate::fakedirector;
use crate::reg;

use std::ops::Deref;
use std::sync::{Mutex, MutexGuard, OnceLock};

use fakedirector::Fake;
use reg::{
    close, object_string, reg_create, wide, with_oa, Checker, ObjectAttributes, Paths,
    UnicodeString, OBJECT_NAME_INFORMATION,
};
use vfs_registry::Lookup;
use vfs_shim::{is_synthetic_key_handle, regclient, registry_handle_path};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_READ, KEY_SET_VALUE, REG_DWORD,
    REG_OPTION_NON_VOLATILE,
};

static LOCK: Mutex<()> = Mutex::new(());

const BASE: &str = r"Software\AetherVfsRegKeysTest";

const STATUS_SUCCESS: i32 = 0;
const STATUS_UNSUCCESSFUL: i32 = 0xC000_0001u32 as i32;
const STATUS_INVALID_HANDLE: i32 = 0xC000_0008u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
const STATUS_NOT_SUPPORTED: i32 = 0xC000_00BBu32 as i32;
const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005u32 as i32;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;

const NT_KEY_READ: u32 = 0x2_0019;
const NT_KEY_SET_VALUE: u32 = 0x2;
const NT_KEY_QUERY_VALUE: u32 = 0x1;
const NT_KEY_ALL_ACCESS: u32 = 0xF_003F;
const REG_OPTION_VOLATILE: u32 = 1;
const REG_CREATED_NEW_KEY: u32 = 1;
const REG_OPENED_EXISTING_KEY: u32 = 2;
const DUPLICATE_CLOSE_SOURCE: u32 = 1;
const DUPLICATE_SAME_ACCESS: u32 = 2;
const OBJECT_BASIC_INFORMATION: u32 = 0;
const OBJECT_HANDLE_FLAG_INFORMATION: u32 = 4;
const OBJECT_TYPE_INFORMATION: u32 = 2;
const CURRENT_PROCESS: isize = -1;

#[link(name = "ntdll")]
extern "system" {
    fn NtOpenKey(key: *mut isize, access: u32, oa: *const ObjectAttributes) -> i32;
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
    fn NtDuplicateObject(
        src_process: isize,
        src: isize,
        dst_process: isize,
        dst: *mut isize,
        access: u32,
        attributes: u32,
        options: u32,
    ) -> i32;
    fn NtQueryObject(h: isize, class: u32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
}

fn open_rel(root: isize, name: &str, access: u32) -> (i32, isize) {
    let mut h = 0isize;
    let st = with_oa(root, name, |oa| unsafe {
        NtOpenKeyEx(&mut h, access, oa, 0)
    });
    (st, h)
}

fn create_rel(root: isize, name: &str, access: u32, options: u32) -> (i32, isize, u32) {
    let mut h = 0isize;
    let mut disp = 0u32;
    let st = with_oa(root, name, |oa| unsafe {
        NtCreateKey(&mut h, access, oa, 0, std::ptr::null(), options, &mut disp)
    });
    (st, h, disp)
}

struct Fixture {
    fake: &'static Fake,
    paths: Paths,
    checker: Checker,
    /// The restrictive DACL refused a write open before the hooks went in.
    locked_refuses_write: bool,
    /// `ReadLimited` refused KEY_READ and granted KEY_QUERY_VALUE before the hooks went in.
    read_limited_refuses_key_read: bool,
}

impl Deref for Fixture {
    type Target = Paths;
    fn deref(&self) -> &Paths {
        &self.paths
    }
}

impl Fixture {
    fn open(&self, rel: &str, access: u32) -> (i32, isize) {
        open_rel(0, &self.nt(rel), access)
    }

    fn create(&self, rel: &str, options: u32) -> (i32, isize, u32) {
        create_rel(0, &self.nt(rel), NT_KEY_ALL_ACCESS, options)
    }

    /// Whether the key exists in the real registry, asked of the unhooked checker process.
    fn really_exists(&self, rel: &str) -> bool {
        self.checker.really_exists(&format!(r"{BASE}\{rel}"))
    }
}

/// Real keys the tests expect, made before the hooks are installed.
const REAL_KEYS: &[&str] = &[
    "Untouched",
    r"Untouched\Sub",
    "Overlaid",
    r"Tomb\Child",
    "Existing",
    "Locked",
    r"Parent\Gone\RealChild",
    r"Parent\Gone2",
    "DirectorDown",
    "Dup",
    "ReadLimited",
    "PerFrame",
];

/// Whether a Win32 open of `HKCU\<sub>` with `access` is refused (before the hooks).
fn refused(sub: &str, access: u32) -> bool {
    let mut k: HKEY = std::ptr::null_mut();
    let st = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(sub).as_ptr(), 0, access, &mut k) };
    if st == 0 {
        unsafe { RegCloseKey(k) };
    }
    st == 5 // ERROR_ACCESS_DENIED
}

fn fixture() -> (MutexGuard<'static, ()>, &'static Fixture) {
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    static F: OnceLock<Fixture> = OnceLock::new();
    let f = F.get_or_init(|| {
        reg::reset_base(BASE, &["Locked", "ReadLimited"]);
        for k in REAL_KEYS {
            unsafe { RegCloseKey(reg_create(&format!(r"{BASE}\{k}"))) };
        }
        // Everyone may read `Locked`, nobody may write it.
        reg::set_dacl(&format!(r"{BASE}\Locked"), "D:P(A;;KR;;;WD)");
        let locked_refuses_write = refused(&format!(r"{BASE}\Locked"), NT_KEY_SET_VALUE);
        // `ReadLimited` grants only KEY_QUERY_VALUE: a KEY_READ open is refused.
        reg::set_dacl(&format!(r"{BASE}\ReadLimited"), "D:P(A;;0x1;;;WD)");
        let read_limited_refuses_key_read = refused(&format!(r"{BASE}\ReadLimited"), NT_KEY_READ)
            && !refused(&format!(r"{BASE}\ReadLimited"), NT_KEY_QUERY_VALUE);

        // The checker: started now, so it has no hooks.
        let checker = Checker::spawn();
        let fake = reg::install_hooks("regkeys");
        assert!(vfs_shim::registry_detours_installed() > 0);
        Fixture {
            fake,
            paths: Paths::new(BASE),
            checker,
            locked_refuses_write,
            read_limited_refuses_key_read,
        }
    });
    (guard, f)
}

fn real(h: isize) -> bool {
    !is_synthetic_key_handle(h) && object_string(h, OBJECT_TYPE_INFORMATION).as_deref() == Ok("Key")
}

#[test]
fn an_untouched_key_gives_the_real_handle() {
    isolate!();
    let (_g, f) = fixture();
    let (st, h) = f.open("Untouched", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(real(h), "a kernel key object");
    assert_eq!(
        registry_handle_path(h),
        Some(f.canon("Untouched")),
        "recorded as pass-through"
    );

    // Relative to the tracked handle.
    let (st, sub) = open_rel(h, "Sub", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(real(sub));
    assert_eq!(registry_handle_path(sub), Some(f.canon(r"Untouched\Sub")));

    // Through Win32, relative to the HKCU handle kernelbase opened before the hooks went in:
    // a root only the real key's name can resolve.
    let mut k: HKEY = std::ptr::null_mut();
    let st = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(&format!(r"{BASE}\Untouched")).as_ptr(),
            0,
            KEY_READ,
            &mut k,
        )
    };
    assert_eq!(st, 0);
    assert!(real(k as isize));
    assert_eq!(registry_handle_path(k as isize), Some(f.canon("Untouched")));

    for x in [h, sub, k as isize] {
        assert_eq!(close(x), STATUS_SUCCESS);
        assert_eq!(
            registry_handle_path(x),
            None,
            "the record goes with the handle"
        );
    }
}

#[test]
fn a_key_with_overlay_content_gives_a_synthetic_handle() {
    isolate!();
    let (_g, f) = fixture();
    regclient::set_value(&f.canon("Overlaid"), "Fov", 4, &90u32.to_le_bytes()).unwrap();

    let (st, h) = f.open("Overlaid", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(is_synthetic_key_handle(h));
    assert_eq!(registry_handle_path(h), Some(f.canon("Overlaid")));

    // `NtOpenKey` decides the same way.
    let mut h2 = 0isize;
    let st = with_oa(0, &f.nt("Overlaid"), |oa| unsafe {
        NtOpenKey(&mut h2, NT_KEY_READ, oa)
    });
    assert_eq!(st, STATUS_SUCCESS);
    assert!(is_synthetic_key_handle(h2));

    // The base key has overlay content below it, so it is synthetic too; relative to it, an
    // untouched child is the real key and the overlaid child is synthetic.
    let (st, base) = open_rel(0, &format!(r"\Registry\User\{}\{BASE}", f.sid), NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(is_synthetic_key_handle(base));
    let (st, child) = open_rel(base, "Untouched", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(
        real(child),
        "a pass-through child of a synthetic root is the real key"
    );
    let (st, over) = open_rel(base, "overlaid", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(is_synthetic_key_handle(over));
    // An empty relative name is the root itself.
    let (st, same) = open_rel(base, "", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(registry_handle_path(same), registry_handle_path(base));

    for x in [h, h2, base, child, over, same] {
        assert_eq!(close(x), STATUS_SUCCESS);
    }
}

#[test]
fn an_overlay_only_key_opens_and_a_tombstoned_key_is_not_found() {
    isolate!();
    let (_g, f) = fixture();
    regclient::create_key(&f.canon("OnlyHere"), false).unwrap();
    let (st, h) = f.open("OnlyHere", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(is_synthetic_key_handle(h));
    assert_eq!(close(h), STATUS_SUCCESS);
    assert!(!f.really_exists("OnlyHere"));

    regclient::delete_key(&f.canon("Tomb")).unwrap();
    assert_eq!(f.open("Tomb", NT_KEY_READ).0, STATUS_OBJECT_NAME_NOT_FOUND);
    assert_eq!(
        f.open(r"Tomb\Child", NT_KEY_READ).0,
        STATUS_OBJECT_NAME_NOT_FOUND
    );
    assert!(
        f.really_exists(r"Tomb\Child"),
        "the real key is hidden, not deleted"
    );
}

#[test]
fn create_reports_dispositions_and_never_creates_a_real_key() {
    isolate!();
    let (_g, f) = fixture();

    // A key that exists for real: the real key, opened.
    let (st, h, disp) = f.create("Existing", 0);
    assert_eq!((st, disp), (STATUS_SUCCESS, REG_OPENED_EXISTING_KEY));
    assert!(real(h));
    assert_eq!(registry_handle_path(h), Some(f.canon("Existing")));
    close(h);

    // A new key: created here, in the overlay only.
    let (st, h, disp) = f.create(r"Parent\New", 0);
    assert_eq!((st, disp), (STATUS_SUCCESS, REG_CREATED_NEW_KEY));
    assert!(is_synthetic_key_handle(h));
    close(h);
    assert_eq!(
        regclient::lookup(&f.canon(r"Parent\New")),
        Ok((Lookup::Present { created: true }, false))
    );
    assert!(!f.really_exists(r"Parent\New"));
    // Again: it exists now.
    let (st, h, disp) = f.create(r"Parent\New", 0);
    assert_eq!((st, disp), (STATUS_SUCCESS, REG_OPENED_EXISTING_KEY));
    assert!(is_synthetic_key_handle(h));
    close(h);
    // Below a key created here.
    let (st, h, disp) = f.create(r"Parent\New\Deeper", 0);
    assert_eq!((st, disp), (STATUS_SUCCESS, REG_CREATED_NEW_KEY));
    close(h);

    // Volatile is passed on.
    let (st, h, _) = f.create(r"Parent\Volatile", REG_OPTION_VOLATILE);
    assert_eq!(st, STATUS_SUCCESS);
    close(h);
    assert!(
        regclient::key(&f.canon(r"Parent\Volatile"))
            .unwrap()
            .unwrap()
            .volatile
    );

    // No parent, as on Windows.
    assert_eq!(
        f.create(r"Parent\Missing\Child", 0).0,
        STATUS_OBJECT_NAME_NOT_FOUND
    );
    assert!(!f.really_exists(r"Parent\Missing"));

    // A tombstoned key is created again, created here: its real children stay hidden.
    regclient::delete_key(&f.canon(r"Parent\Gone")).unwrap();
    let (st, h, disp) = f.create(r"Parent\Gone", 0);
    assert_eq!((st, disp), (STATUS_SUCCESS, REG_CREATED_NEW_KEY));
    close(h);
    assert_eq!(
        regclient::lookup(&f.canon(r"Parent\Gone")),
        Ok((Lookup::Present { created: true }, false))
    );
    assert_eq!(
        f.open(r"Parent\Gone\RealChild", NT_KEY_READ).0,
        STATUS_OBJECT_NAME_NOT_FOUND
    );
    // Under a tombstoned parent there is no parent.
    regclient::delete_key(&f.canon(r"Parent\Gone2")).unwrap();
    assert_eq!(
        f.create(r"Parent\Gone2\X", 0).0,
        STATUS_OBJECT_NAME_NOT_FOUND
    );
    assert!(f.really_exists(r"Parent\Gone\RealChild"));
    assert!(f.really_exists(r"Parent\Gone2"));
}

#[test]
fn a_write_open_the_real_key_refuses_gets_a_synthetic_handle() {
    isolate!();
    let (_g, f) = fixture();
    assert!(
        f.locked_refuses_write,
        "the restrictive DACL did not refuse a write open before the hooks; this host does \
         not enforce key security and the test cannot show the fallback"
    );
    let (st, h) = f.open("Locked", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(real(h), "a read open is the real key");
    close(h);

    let (st, h) = f.open("Locked", NT_KEY_SET_VALUE | NT_KEY_READ);
    assert_eq!(
        st, STATUS_SUCCESS,
        "write access is granted: writes go to the overlay"
    );
    assert!(is_synthetic_key_handle(h));
    assert_eq!(registry_handle_path(h), Some(f.canon("Locked")));
    close(h);
    let (st, h, disp) = f.create("Locked", 0);
    assert_eq!((st, disp), (STATUS_SUCCESS, REG_OPENED_EXISTING_KEY));
    assert!(is_synthetic_key_handle(h));
    close(h);
}

#[test]
fn duplicates_are_tracked_and_closes_release_them() {
    isolate!();
    let (_g, f) = fixture();
    regclient::set_value(&f.canon("Dup"), "v", 4, &1u32.to_le_bytes()).unwrap();
    let before = vfs_shim::registry_handle_counts();

    // Synthetic: a new synthetic handle that outlives its source.
    let (_, s1) = f.open("Dup", NT_KEY_READ);
    assert!(is_synthetic_key_handle(s1));
    let mut s2 = 0isize;
    let st = unsafe {
        NtDuplicateObject(
            CURRENT_PROCESS,
            s1,
            CURRENT_PROCESS,
            &mut s2,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    assert_eq!(st, STATUS_SUCCESS);
    assert!(is_synthetic_key_handle(s2));
    assert_ne!(s1, s2);
    assert_eq!(close(s1), STATUS_SUCCESS);
    assert_eq!(object_string(s2, OBJECT_NAME_INFORMATION), Ok(f.nt("Dup")));
    // `DUPLICATE_CLOSE_SOURCE` closes the source.
    let mut s3 = 0isize;
    let st = unsafe {
        NtDuplicateObject(
            CURRENT_PROCESS,
            s2,
            CURRENT_PROCESS,
            &mut s3,
            0,
            0,
            DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE,
        )
    };
    assert_eq!(st, STATUS_SUCCESS);
    assert_eq!(close(s2), STATUS_INVALID_HANDLE);
    // Not into another process: nothing there could resolve it.
    let other = f.checker.process;
    let mut s4 = 0isize;
    let st = unsafe {
        NtDuplicateObject(
            CURRENT_PROCESS,
            s3,
            other,
            &mut s4,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    assert_eq!(st, STATUS_NOT_SUPPORTED);
    assert_eq!(close(s3), STATUS_SUCCESS);
    assert_eq!(close(s3), STATUS_INVALID_HANDLE);

    // Pass-through: the duplicate is a real handle with the same record.
    let (_, r1) = f.open("Untouched", NT_KEY_READ);
    let mut r2 = 0isize;
    let st = unsafe {
        NtDuplicateObject(
            CURRENT_PROCESS,
            r1,
            CURRENT_PROCESS,
            &mut r2,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    assert_eq!(st, STATUS_SUCCESS);
    assert!(real(r2));
    assert_eq!(registry_handle_path(r2), Some(f.canon("Untouched")));
    close(r1);
    assert_eq!(registry_handle_path(r2), Some(f.canon("Untouched")));
    close(r2);
    assert_eq!(registry_handle_path(r2), None);

    // The tables hold the live handles only.
    for _ in 0..200 {
        let (_, a) = f.open("Untouched", NT_KEY_READ);
        let (_, b) = f.open("Dup", NT_KEY_READ);
        close(a);
        close(b);
    }
    assert_eq!(vfs_shim::registry_handle_counts(), before);
}

#[test]
fn the_object_name_of_a_synthetic_key_is_its_nt_name() {
    isolate!();
    let (_g, f) = fixture();
    regclient::set_value(&f.canon("Overlaid"), "Fov", 4, &90u32.to_le_bytes()).unwrap();
    let (_, s) = f.open("Overlaid", NT_KEY_READ);
    assert!(is_synthetic_key_handle(s));
    assert_eq!(
        object_string(s, OBJECT_NAME_INFORMATION),
        Ok(f.nt("Overlaid"))
    );
    // The real key's own name has the same shape. The hive's capitalisation is the host's:
    // Windows reports `\REGISTRY\USER`, which the shim uses; Wine reports `\REGISTRY\User`.
    let (_, r) = f.open("Untouched", NT_KEY_READ);
    let real_name = object_string(r, OBJECT_NAME_INFORMATION).unwrap();
    assert!(
        real_name.eq_ignore_ascii_case(&f.nt("Untouched")) && real_name.starts_with(r"\REGISTRY\"),
        "{real_name}"
    );
    // A short buffer: the required length, and no data.
    let mut buf = [0u8; 20];
    let mut need = 0u32;
    let st = unsafe {
        NtQueryObject(
            s,
            OBJECT_NAME_INFORMATION,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut need,
        )
    };
    assert_eq!(st, STATUS_BUFFER_OVERFLOW);
    assert_eq!(
        need as usize,
        16 + f.nt("Overlaid").encode_utf16().count() * 2 + 2
    );
    close(s);
    close(r);
}

#[test]
fn a_root_that_names_no_key_is_passed_through_and_counted() {
    isolate!();
    let (_g, _f) = fixture();
    use windows_sys::Win32::System::Threading::CreateEventW;
    let ev = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) } as isize;
    let before = vfs_shim::reg_unresolved_count();
    let (st, h) = open_rel(ev, "Software", NT_KEY_READ);
    assert!(st < 0, "the real call's own failure: {st:#x}");
    assert_eq!(h, 0);
    assert_eq!(vfs_shim::reg_unresolved_count(), before + 1);
    close(ev);
}

#[test]
fn a_failing_director_reads_the_real_registry_and_refuses_creates() {
    isolate!();
    let (_g, f) = fixture();
    regclient::set_value(&f.canon("Overlaid"), "Fov", 4, &90u32.to_le_bytes()).unwrap();
    let host = f.fake.director().registry().unwrap();
    f.fake.director().set_registry(None);
    let fallbacks = vfs_shim::reg_read_fallback_count();

    let (st, h) = f.open("Overlaid", NT_KEY_READ);
    let open_overlaid = (st, real(h));
    close(h);
    let create_new = f.create(r"DirectorDown\New", 0);
    let (st, h, disp) = f.create("Existing", 0);
    let create_existing = (st, disp, real(h));
    close(h);
    let fell_back = vfs_shim::reg_read_fallback_count() - fallbacks;
    f.fake.director().set_registry(Some(host));

    assert_eq!(open_overlaid, (STATUS_SUCCESS, true), "the real key alone");
    assert_eq!(create_new.0, STATUS_UNSUCCESSFUL);
    assert!(
        !f.really_exists(r"DirectorDown\New"),
        "never created for real"
    );
    assert_eq!(
        create_existing,
        (STATUS_SUCCESS, REG_OPENED_EXISTING_KEY, true)
    );
    assert!(
        fell_back >= 3,
        "every failed lookup is counted: {fell_back}"
    );
}

#[test]
fn an_overlay_node_with_no_real_key_opens_and_takes_children() {
    isolate!();
    let (_g, f) = fixture();
    // A value written to a key that does not exist for real: an overlay node that overlays a
    // real key (created:false) whose real key is missing. It exists in the merged view.
    regclient::set_value(&f.canon("Phantom"), "v", 4, &1u32.to_le_bytes()).unwrap();
    assert_eq!(
        regclient::lookup(&f.canon("Phantom")),
        Ok((Lookup::Present { created: false }, false))
    );
    assert!(!f.really_exists("Phantom"));
    let (st, h) = f.open("Phantom", NT_KEY_READ);
    assert_eq!(st, STATUS_SUCCESS);
    assert!(is_synthetic_key_handle(h));
    close(h);
    let (st, h, disp) = f.create(r"Phantom\Child", 0);
    assert_eq!((st, disp), (STATUS_SUCCESS, REG_CREATED_NEW_KEY));
    assert!(is_synthetic_key_handle(h));
    close(h);
    assert!(!f.really_exists(r"Phantom\Child"));
}

#[test]
fn a_key_that_refuses_key_read_is_read_with_the_callers_own_rights() {
    isolate!();
    let (_g, f) = fixture();
    assert!(
        f.read_limited_refuses_key_read,
        "the DACL did not refuse KEY_READ (or refused KEY_QUERY_VALUE) before the hooks"
    );
    regclient::set_value(&f.canon("ReadLimited"), "v", 4, &1u32.to_le_bytes()).unwrap();
    let (st, h) = f.open("ReadLimited", NT_KEY_QUERY_VALUE);
    assert_eq!(
        st, STATUS_SUCCESS,
        "the private open retried with KEY_QUERY_VALUE"
    );
    assert!(is_synthetic_key_handle(h));
    // Its duplicate opens its own private handle the same way.
    let mut d = 0isize;
    let st = unsafe {
        NtDuplicateObject(
            CURRENT_PROCESS,
            h,
            CURRENT_PROCESS,
            &mut d,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    assert_eq!(st, STATUS_SUCCESS);
    close(h);
    assert_eq!(
        object_string(d, OBJECT_TYPE_INFORMATION).as_deref(),
        Ok("Key")
    );
    close(d);
}

/// `NtQueryObject` classes beside the name on a synthetic key: the type, the basic information
/// with this handle's own access and attributes, and the handle flags.
#[test]
fn a_synthetic_key_answers_type_basic_and_handle_flag_queries() {
    isolate!();
    let (_g, f) = fixture();
    regclient::set_value(&f.canon("Overlaid"), "Fov", 4, &90u32.to_le_bytes()).unwrap();
    regclient::create_key(&f.canon("TypeOnlyHere"), false).ok();
    for rel in ["Overlaid", "TypeOnlyHere"] {
        let (st, h) = f.open(rel, NT_KEY_READ | NT_KEY_SET_VALUE);
        assert_eq!(st, STATUS_SUCCESS);
        assert!(is_synthetic_key_handle(h));
        assert_eq!(
            object_string(h, OBJECT_TYPE_INFORMATION).as_deref(),
            Ok("Key"),
            "{rel}"
        );

        let mut basic = [0u32; 14];
        let mut need = 0u32;
        let st = unsafe {
            NtQueryObject(
                h,
                OBJECT_BASIC_INFORMATION,
                basic.as_mut_ptr().cast(),
                56,
                &mut need,
            )
        };
        assert_eq!(st, STATUS_SUCCESS, "{rel}");
        assert_eq!(need, 56);
        assert_eq!(basic[0], 0, "attributes: not inheritable");
        assert_eq!(
            basic[1],
            NT_KEY_READ | NT_KEY_SET_VALUE,
            "granted access is the handle's"
        );
        // Too short: the host's own refusal for this class.
        let st = unsafe {
            NtQueryObject(
                h,
                OBJECT_BASIC_INFORMATION,
                basic.as_mut_ptr().cast(),
                8,
                &mut need,
            )
        };
        assert_eq!(st, STATUS_INFO_LENGTH_MISMATCH);

        let mut flags = [0xAAu8; 2];
        let st = unsafe {
            NtQueryObject(
                h,
                OBJECT_HANDLE_FLAG_INFORMATION,
                flags.as_mut_ptr(),
                2,
                &mut need,
            )
        };
        assert_eq!((st, flags, need), (STATUS_SUCCESS, [0, 0], 2));
        close(h);
    }
}

#[test]
fn a_failed_duplicate_with_close_source_drops_the_record() {
    isolate!();
    let (_g, f) = fixture();
    let (_, r) = f.open("Untouched", NT_KEY_READ);
    assert!(registry_handle_path(r).is_some());
    let mut d = 0isize;
    // No target process: the duplication fails, and NT closes the source anyway.
    let st = unsafe {
        NtDuplicateObject(
            CURRENT_PROCESS,
            r,
            0,
            &mut d,
            0,
            0,
            DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE,
        )
    };
    assert!(st < 0, "{st:#x}");
    assert_eq!(registry_handle_path(r), None);
}

/// The pattern a game's per-frame registry write makes through advapi32: open (or create) the
/// key, set a value, `RegCloseKey`. Wine's `RegCloseKey` treats every handle at or above
/// `0x80000000` as a predefined key and returns without calling `NtClose`, so a synthetic
/// handle up there would never reach the close hook and its record (and private real handle)
/// would leak, one per frame.
#[test]
fn reg_close_key_releases_synthetic_handles() {
    isolate!();
    let (_g, f) = fixture();
    regclient::set_value(&f.canon("PerFrame"), "Seed", 4, &0u32.to_le_bytes()).unwrap();
    let before = vfs_shim::registry_handle_counts();
    let sub = wide(&format!(r"{BASE}\PerFrame"));
    let name = wide("Frame");
    for i in 0..500u32 {
        let mut k: HKEY = std::ptr::null_mut();
        let st = if i % 2 == 0 {
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE, &mut k) }
        } else {
            unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    sub.as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_ALL_ACCESS,
                    std::ptr::null(),
                    &mut k,
                    std::ptr::null_mut(),
                )
            }
        };
        assert_eq!(st, 0, "open {i}");
        let h = k as isize;
        assert!(is_synthetic_key_handle(h), "{h:#x}");
        let data = i.to_le_bytes();
        let st = unsafe { RegSetValueExW(k, name.as_ptr(), 0, REG_DWORD, data.as_ptr(), 4) };
        assert_eq!(st, 0, "set {i}");
        assert_eq!(unsafe { RegCloseKey(k) }, 0);
        assert_eq!(
            registry_handle_path(h),
            None,
            "RegCloseKey left {h:#x} open"
        );
    }
    assert_eq!(vfs_shim::registry_handle_counts(), before);
    // The write went to the overlay.
    assert_eq!(
        regclient::lookup(&f.canon("PerFrame")).map(|(l, _)| l),
        Ok(Lookup::Present { created: false })
    );
}
