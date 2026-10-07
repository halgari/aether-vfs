//! With the registry overlay off (`VFS_REGISTRY` unset), no registry detour is installed and
//! every call goes straight to the real registry: real handles, nothing tracked, and a create
//! creates the real key. The shared `NtClose` / `NtQueryObject` hooks keep working.
//!
//! Its own binary: `regclient::enabled` is decided once per process, and `install` is one-shot.
#![cfg(windows)]

use std::ffi::c_void;

use vfs_shim::{
    install, is_synthetic_key_handle, registry_handle_counts, registry_handle_path, Engine,
};

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
    fn NtClose(h: isize) -> i32;
    fn NtQueryObject(h: isize, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

fn with_oa<R>(name: &str, f: impl FnOnce(*const ObjectAttributes) -> R) -> R {
    let w: Vec<u16> = name.encode_utf16().collect();
    let us = UnicodeString {
        length: (w.len() * 2) as u16,
        maximum_length: (w.len() * 2) as u16,
        buffer: w.as_ptr(),
    };
    let oa = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: 0,
        object_name: &us,
        attributes: 0x40,
        security_descriptor: std::ptr::null(),
        security_qos: std::ptr::null(),
    };
    f(&oa)
}

#[test]
fn every_registry_call_is_the_real_one() {
    std::env::remove_var(vfs_env::REGISTRY);
    let root = std::env::temp_dir().join(format!("vfs-shim-regkeys-off-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let snapshot = {
        use vfs_core::{build, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let _hooks = install(Engine::new(root.to_str().unwrap(), snapshot).unwrap()).expect("install");
    assert!(!vfs_shim::regclient::enabled());
    assert_eq!(
        vfs_shim::registry_detours_installed(),
        0,
        "no registry detour goes in with the overlay off"
    );
    assert_eq!(
        vfs_shim::regclient::detours_outcome(),
        Some(Err(vfs_shim::regclient::OFF)),
        "the off outcome is recorded"
    );
    assert_eq!(
        vfs_shim::reg_overlay_disabled_by(),
        None,
        "off is not a disabled overlay"
    );

    let name = r"\Registry\Machine\Software";
    let mut h = 0isize;
    assert_eq!(
        with_oa(name, |oa| unsafe { NtOpenKeyEx(&mut h, 0x2_0019, oa, 0) }),
        0
    );
    assert!(!is_synthetic_key_handle(h));
    assert_eq!(registry_handle_path(h), None, "nothing is tracked");
    // The shared `NtQueryObject` hook answers a real key's name from the real call.
    let mut buf = [0u64; 128];
    let mut ret = 0u32;
    assert_eq!(
        unsafe { NtQueryObject(h, 1, buf.as_mut_ptr().cast(), 1024, &mut ret) },
        0,
        "object name of a real key"
    );
    let name_len = (buf[0] & 0xffff) as usize / 2;
    let name_ptr = buf[1] as *const u16;
    let real_name =
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(name_ptr, name_len) });
    assert!(
        real_name
            .to_ascii_uppercase()
            .starts_with(r"\REGISTRY\MACHINE\SOFTWARE"),
        "{real_name}"
    );

    // A volatile scratch key under HKCU, through Win32 (so through `NtCreateKey`): created
    // for real, then deleted for real.
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS,
        REG_CREATED_NEW_KEY, REG_OPTION_VOLATILE,
    };
    let sub: Vec<u16> = r"Software\AetherVfsRegKeysOffTest"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut k: HKEY = std::ptr::null_mut();
    let mut disp = 0u32;
    let st = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_VOLATILE,
            KEY_ALL_ACCESS,
            std::ptr::null(),
            &mut k,
            &mut disp,
        )
    };
    assert_eq!(st, 0, "real create");
    assert!(!is_synthetic_key_handle(k as isize));
    assert_eq!(disp, REG_CREATED_NEW_KEY, "created for real");
    assert_eq!(registry_handle_counts(), (0, 0), "nothing is tracked");
    unsafe {
        RegCloseKey(k);
        assert_eq!(RegDeleteKeyW(HKEY_CURRENT_USER, sub.as_ptr()), 0);
        NtClose(h);
    }
}
