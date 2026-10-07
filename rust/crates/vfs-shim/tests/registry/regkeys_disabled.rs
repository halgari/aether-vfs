//! With the registry overlay off (`VFS_REGISTRY` unset), no registry detour is installed and
//! every call goes straight to the real registry: real handles, nothing tracked, and a create
//! creates the real key. The shared `NtClose` / `NtQueryObject` hooks keep working.
//!
//! Its own process: `regclient::enabled` is decided once per process, and `install` is one-shot.

use crate::reg;

use reg::{close, object_name, open_abs};
use vfs_shim::{install, is_synthetic_key_handle, registry_handle_counts, registry_handle_path};

#[test]
fn every_registry_call_is_the_real_one() {
    isolate!();
    std::env::remove_var(vfs_env::REGISTRY);
    let root = std::env::temp_dir().join(format!("vfs-shim-regkeys-off-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let _hooks = install().expect("install");
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
    let (st, h) = open_abs(name, 0x2_0019);
    assert_eq!(st, 0);
    assert!(!is_synthetic_key_handle(h));
    assert_eq!(registry_handle_path(h), None, "nothing is tracked");
    // The shared `NtQueryObject` hook answers a real key's name from the real call.
    let real_name = object_name(h).expect("object name of a real key");
    assert!(
        real_name
            .to_ascii_uppercase()
            .starts_with(r"\REGISTRY\MACHINE\SOFTWARE"),
        "{real_name}"
    );

    // A volatile scratch key under HKCU, through Win32 (so through `NtCreateKey`): created
    // for real, then deleted for real.
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, REG_CREATED_NEW_KEY, REG_OPTION_VOLATILE,
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyW,
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
        close(h);
    }
}
