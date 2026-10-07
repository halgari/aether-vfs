//! **A name NT refuses is refused, not forwarded.** (cleanup C2, fix round 1.)
//!
//! An `OBJECT_ATTRIBUTES` name with an odd `Length` is `STATUS_OBJECT_NAME_INVALID` to NT. If the
//! hooks treated it as "undecodable" and handed it on, Wine would round the length down to an
//! even one, rebuild the very name the shim virtualises, and act on the real file under the
//! managed root: a `FILE_OVERWRITE_IF` create would truncate it. The create, open, delete and
//! query-attributes hooks must answer the status themselves.
//!
//! Every claim is about filesystem state: the real file under the root holds `HOST` bytes, the
//! director serves other bytes for the same vpath, and after the odd-length calls the real file
//! must be unchanged and the absent name must not exist.
//!
//! Its own binary: the detours, the `FuseClient` and the `Engine` are process-global.

use crate::fakedirector;

use std::ffi::c_void;

use fakedirector::{Fake, ReadStyle};
use vfs_shim::{install, Engine};

const HOST: &[u8] = b"host: data/x.esp";
const DIR: &[u8] = b"director: data/x.esp";

const STATUS_OBJECT_NAME_INVALID: i32 = 0xC000_0033u32 as i32;
const FILE_OVERWRITE_IF: u32 = 5;
const GENERIC_WRITE: u32 = 0x4000_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_SHARE_ALL: u32 = 7;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: *mut c_void,
    object_name: *const UnicodeString,
    attributes: u32,
    security_descriptor: *const c_void,
    security_qos: *const c_void,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtCreateFile(
        h: *mut *mut c_void,
        access: u32,
        oa: *const ObjectAttributes,
        iosb: *mut c_void,
        alloc: *const i64,
        attrs: u32,
        share: u32,
        disp: u32,
        opts: u32,
        ea: *const c_void,
        ealen: u32,
    ) -> i32;
    fn NtOpenFile(
        h: *mut *mut c_void,
        access: u32,
        oa: *const ObjectAttributes,
        iosb: *mut c_void,
        share: u32,
        opts: u32,
    ) -> i32;
    fn NtDeleteFile(oa: *const ObjectAttributes) -> i32;
    fn NtQueryAttributesFile(oa: *const ObjectAttributes, info: *mut c_void) -> i32;
    fn NtQueryFullAttributesFile(oa: *const ObjectAttributes, info: *mut c_void) -> i32;
}

/// Calls `f` with an OA naming `path` whose `Length` is odd: the whole name plus one stray byte.
fn with_odd_name<R>(path: &std::path::Path, f: impl FnOnce(*const ObjectAttributes) -> R) -> R {
    let nt = format!(r"\??\{}", path.display());
    let mut wide: Vec<u16> = nt.encode_utf16().collect();
    wide.push(0x4141);
    let us = UnicodeString {
        length: (wide.len() * 2 - 1) as u16,
        maximum_length: (wide.len() * 2) as u16,
        buffer: wide.as_ptr(),
    };
    let oa = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: std::ptr::null_mut(),
        object_name: &us,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: std::ptr::null(),
        security_qos: std::ptr::null(),
    };
    f(&oa)
}

#[test]
fn an_odd_length_name_under_a_managed_root_is_refused_and_touches_nothing() {
    isolate!();
    let base = std::env::temp_dir().join(format!("vfs-odd-name-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(root.join("data")).unwrap();
    let real = root.join("data").join("x.esp");
    let absent = root.join("data").join("absent.esp");
    std::fs::write(&real, HOST).unwrap();

    std::env::set_var(vfs_env::SHIM_STATS_LOG, base.join("shim-stats.log"));
    std::env::set_var(vfs_env::SHIM_STATS_INTERVAL_MS, "3600000");

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/x.esp".into(),
                kind: EntryKind::File,
                source: real.to_string_lossy().as_ref().into(),
                size: 0,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let _fake = fakedirector::install(
        &root,
        Fake::new()
            .with("data/x.esp", DIR.to_vec(), ReadStyle::Whole)
            .with_dir("data")
            .writable_under("data/"),
        0,
    );
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let hooks = install(engine).expect("install");

    let create = |p: &std::path::Path| {
        with_odd_name(p, |oa| {
            let mut h = std::ptr::null_mut();
            let mut iosb = [0u8; 16];
            let st = unsafe {
                NtCreateFile(
                    &mut h,
                    GENERIC_WRITE | SYNCHRONIZE,
                    oa,
                    iosb.as_mut_ptr().cast(),
                    std::ptr::null(),
                    0,
                    FILE_SHARE_ALL,
                    FILE_OVERWRITE_IF,
                    FILE_SYNCHRONOUS_IO_NONALERT,
                    std::ptr::null(),
                    0,
                )
            };
            (st, h)
        })
    };
    let (create_real, h1) = create(&real);
    let (create_absent, h2) = create(&absent);
    let (open_real, h3) = with_odd_name(&real, |oa| {
        let mut h = std::ptr::null_mut();
        let mut iosb = [0u8; 16];
        let st = unsafe {
            NtOpenFile(
                &mut h,
                GENERIC_WRITE | SYNCHRONIZE,
                oa,
                iosb.as_mut_ptr().cast(),
                FILE_SHARE_ALL,
                FILE_SYNCHRONOUS_IO_NONALERT,
            )
        };
        (st, h)
    });
    let delete = with_odd_name(&real, |oa| unsafe { NtDeleteFile(oa) });
    let mut info = [0u8; 64];
    let qattr = with_odd_name(&real, |oa| unsafe {
        NtQueryAttributesFile(oa, info.as_mut_ptr().cast())
    });
    let qfull = with_odd_name(&real, |oa| unsafe {
        NtQueryFullAttributesFile(oa, info.as_mut_ptr().cast())
    });

    drop(hooks);

    for (what, st) in [
        ("NtCreateFile (overwrite, existing)", create_real),
        ("NtCreateFile (overwrite, absent)", create_absent),
        ("NtOpenFile", open_real),
        ("NtDeleteFile", delete),
        ("NtQueryAttributesFile", qattr),
        ("NtQueryFullAttributesFile", qfull),
    ] {
        assert_eq!(st, STATUS_OBJECT_NAME_INVALID, "{what}: {st:#x}");
    }
    assert!(
        h1.is_null() && h2.is_null() && h3.is_null(),
        "no handle came back"
    );
    assert_eq!(
        std::fs::read(&real).ok().as_deref(),
        Some(HOST),
        "the real file under the managed root was truncated, replaced or deleted by an \
         odd-length name"
    );
    assert!(
        !absent.exists(),
        "an odd-length create made a real file under the managed root"
    );
}
