//! Telling the process where its image is.
//!
//! A program whose root location is purely virtual is really started from
//! its root's backing directory (`vfs_env::ROOT_BACKINGS`): `CreateProcess`
//! needs a real image, and the loader maps the static imports from beside it
//! before any hook exists. The launcher has already rewritten the command
//! line and working directory (vfs-inject's `procparams`); once the loader is
//! done, this renames what is left — `ProcessParameters.ImagePathName` and
//! the executable's loader entry (`FullDllName`, which `GetModuleFileNameW`
//! reads) — to the image's path under the root's location. From then on
//! `GetModuleFileNameW(NULL)` names the virtual path, and a `LoadLibrary` that
//! searches the application directory searches the virtual one, through the
//! hooks.

use crate::director::{backings_from_env, roots_from_env};
use vfs_protocol::RootId;

/// `path` (a DOS path) moved from under a backing directory to the same place
/// under its root's location; `None` when it is under none of `backings`.
/// Case-insensitive on whole components, and the rest keeps its spelling.
pub(crate) fn to_virtual(
    path: &str,
    backings: &[(RootId, String)],
    locations: &[(RootId, String)],
) -> Option<String> {
    let trim = |s: &str| s.trim_end_matches(['\\', '/']).to_string();
    // The longest backing that holds `path`, so a nested one wins.
    let (root, backing) = backings
        .iter()
        .filter(|(_, b)| {
            let b = trim(b);
            path.len() >= b.len()
                && path.is_char_boundary(b.len())
                && path[..b.len()].eq_ignore_ascii_case(&b)
                && matches!(path[b.len()..].chars().next(), None | Some('\\' | '/'))
        })
        .max_by_key(|(_, b)| trim(b).len())?;
    let location = locations.iter().find(|(id, _)| id == root)?;
    Some(format!(
        "{}{}",
        trim(&location.1),
        &path[trim(backing).len()..]
    ))
}

/// `path` (a DOS path) under a backing directory, spelled under its root's location instead;
/// `None` for any other path, or when no root is purely virtual. The backings and locations
/// are read from the environment once.
pub(crate) fn virtual_spelling(path: &str) -> Option<String> {
    type Map = (Vec<(RootId, String)>, Vec<(RootId, String)>);
    static MAP: std::sync::OnceLock<Option<Map>> = std::sync::OnceLock::new();
    let (backings, locations) = MAP
        .get_or_init(|| {
            let backings = backings_from_env();
            if backings.is_empty() {
                return None;
            }
            let dir = vfs_env::text(vfs_env::VIRTUAL_DIR)?;
            Some((backings, roots_from_env(&dir)))
        })
        .as_ref()?;
    to_virtual(path, backings, locations)
}

/// Rename the image path and the executable's loader entry, if the process
/// was started from a backing directory. Called once, by the bootstrap,
/// after the director's client attached.
///
/// # Safety
/// Reads and writes this process's own PEB and loader data, while no other
/// thread of the program has run (the loader holds its lock through the
/// shim's `DllMain`, and an injected bootstrap runs before the program is
/// resumed).
#[allow(unsafe_code)]
pub(crate) unsafe fn rename_image() {
    let backings = backings_from_env();
    if backings.is_empty() {
        return;
    }
    let Some(dir) = vfs_env::text(vfs_env::VIRTUAL_DIR) else {
        return;
    };
    let locations = roots_from_env(&dir);
    let map = |s: &str| to_virtual(s, &backings, &locations);
    // SAFETY: as this function's contract; every pointer is checked for null
    // and read from structures the loader keeps for the process's lifetime.
    unsafe {
        use crate::peb::{ENTRY_FULL_DLL_NAME, LDR_IN_LOAD_ORDER, PEB_LDR, PP_IMAGE_PATH_NAME};
        let peb = crate::peb::peb();
        if peb == 0 {
            return;
        }
        // ProcessParameters.ImagePathName.
        let params = crate::peb::process_parameters();
        if params != 0 {
            rename_string(params + PP_IMAGE_PATH_NAME, &map);
        }
        // The executable's loader entry: first in PEB.Ldr's InLoadOrderModuleList;
        // LDR_DATA_TABLE_ENTRY.FullDllName.
        // Every loader entry mapped from a backing directory, not only the executable's: the
        // static imports staged beside it (a plugin preloader among them) are loaded from the
        // backing directory too, and one that builds paths from its own module name would
        // otherwise load what SKSE loads by the location under a second spelling — a second
        // copy of the same DLL. The list is walked from its head; `map` leaves every other
        // entry (system DLLs, the shim) as it is.
        let ldr = *((peb + PEB_LDR) as *const usize);
        if ldr != 0 {
            let head = ldr + LDR_IN_LOAD_ORDER;
            let mut entry = *(head as *const usize);
            let mut seen = 0;
            while entry != 0 && entry != head && seen < 4096 {
                rename_string(entry + ENTRY_FULL_DLL_NAME, &map);
                entry = *(entry as *const usize);
                seen += 1;
            }
        }
    }
}

/// Replace the `UNICODE_STRING` at `at` with `map` of it, when `map` has an
/// answer. The new buffer is leaked: it names the image for the life of the
/// process.
///
/// # Safety
/// `at` is a live `UNICODE_STRING` of this process.
#[allow(unsafe_code)]
unsafe fn rename_string(at: usize, map: &dyn Fn(&str) -> Option<String>) {
    // SAFETY: as this function's contract.
    unsafe {
        let len = *(at as *const u16) as usize / 2;
        // Buffer follows the two u16 lengths at the pointer's alignment: 8 on x64, 4 on x86
        let buf = *((at + core::mem::size_of::<usize>()) as *const *const u16);
        if buf.is_null() || len == 0 {
            return;
        }
        let old = String::from_utf16_lossy(core::slice::from_raw_parts(buf, len));
        let Some(new) = map(&old) else {
            return;
        };
        let mut units: Vec<u16> = new.encode_utf16().collect();
        let bytes = units.len() * 2;
        if bytes + 2 > u16::MAX as usize {
            return;
        }
        units.push(0);
        let leaked: &'static mut [u16] = Box::leak(units.into_boxed_slice());
        *((at + core::mem::size_of::<usize>()) as *mut *const u16) = leaked.as_ptr();
        *((at + 2) as *mut u16) = (bytes + 2) as u16;
        *(at as *mut u16) = bytes as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(id: u32, p: &str) -> (RootId, String) {
        (RootId(id), p.to_string())
    }

    #[test]
    fn a_path_under_a_backing_moves_under_its_location() {
        let backings = [
            r(0, r"C:\data\state\roots\0"),
            r(2, r"C:\data\state\roots\2"),
        ];
        let locations = [r(0, r"C:\Haskill\list\game"), r(2, r"C:\Users\u\My Games")];
        assert_eq!(
            to_virtual(r"c:\DATA\state\roots\0\SkyrimSE.exe", &backings, &locations).as_deref(),
            Some(r"C:\Haskill\list\game\SkyrimSE.exe")
        );
        assert_eq!(
            to_virtual(r"C:\data\state\roots\2", &backings, &locations).as_deref(),
            Some(r"C:\Users\u\My Games")
        );
    }

    #[test]
    fn a_path_under_no_backing_is_left_alone() {
        let backings = [r(0, r"C:\data\state\roots\0")];
        let locations = [r(0, r"C:\Haskill\game")];
        // A sibling whose name only starts with the backing's.
        assert_eq!(
            to_virtual(r"C:\data\state\roots\00\x.exe", &backings, &locations),
            None
        );
        assert_eq!(
            to_virtual(r"C:\Windows\notepad.exe", &backings, &locations),
            None
        );
        // A backing with no declared location.
        assert_eq!(
            to_virtual(r"C:\data\state\roots\0\x.exe", &backings, &[]),
            None
        );
    }

    #[test]
    fn the_longest_backing_wins() {
        let backings = [r(4, r"C:\b"), r(1, r"C:\b\nested")];
        let locations = [r(4, r"C:\Users\u"), r(1, r"C:\Users\u\Documents\My Games")];
        assert_eq!(
            to_virtual(r"C:\b\nested\Skyrim.ini", &backings, &locations).as_deref(),
            Some(r"C:\Users\u\Documents\My Games\Skyrim.ini")
        );
    }
}
