//! Installing the detours: the trampoline slots, the install passes and their errors.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{
    CreateProcessInternalWFn, ENGINE, SELF_DLL, close_hook, compress_key_hook, cpiw_hook,
    create_hook, create_key_hook, create_key_tx_hook, create_section_hook, delete_hook,
    delete_key_hook, delete_value_key_hook, dup_hook, enum_key_hook, enum_value_hook, flush_hook,
    flush_key_hook, host_name_convention, install_panic_hook, load_key_ex_hook, load_key_hook,
    load_key2_hook, load_key3_hook, lock_hook, lock_registry_key_hook, map_view_hook,
    notify_key_hook, notify_multiple_hook, open_hook, open_key_ex_hook, open_key_hook,
    open_key_tx_ex_hook, open_key_tx_hook, qattr_hook, qdir_hook, qdirex_hook, qfull_hook,
    qibn_hook, qif_hook, qobj_hook, query_key_hook, query_multiple_hook, query_security_hook,
    query_value_hook, qvol_hook, read_hook, rename_key_hook, replace_key_hook, restore_key_hook,
    save_key_ex_hook, save_key_hook, save_merged_hook, set_info_key_hook, set_info_object_hook,
    set_security_hook, set_value_key_hook, setinfo_hook, unload_key_ex_hook, unload_key_hook,
    unload_key2_hook, unlock_hook, unmap_view_hook, write_hook,
};
use crate::child::self_dll_path;
use crate::engine::Engine;
use crate::ntdef::{
    NtCloseFn, NtCreateFileFn, NtCreateKeyFn, NtCreateKeyTransactedFn, NtCreateSectionFn,
    NtDeleteFileFn, NtDeleteKeyFn, NtDeleteValueKeyFn, NtDuplicateObjectFn, NtEnumerateKeyFn,
    NtEnumerateValueKeyFn, NtFlushBuffersFileFn, NtFlushKeyFn, NtKeyOnlyFn, NtLoadKey2Fn,
    NtLoadKey8Fn, NtLoadKeyFn, NtLockFileFn, NtMapViewOfSectionFn, NtNotifyChangeKeyFn,
    NtNotifyChangeMultipleKeysFn, NtOpenFileFn, NtOpenKeyExFn, NtOpenKeyFn,
    NtOpenKeyTransactedExFn, NtOpenKeyTransactedFn, NtQueryAttributesFileFn,
    NtQueryDirectoryFileExFn, NtQueryDirectoryFileFn, NtQueryFullAttributesFileFn,
    NtQueryInformationByNameFn, NtQueryInformationFileFn, NtQueryKeyFn, NtQueryMultipleValueKeyFn,
    NtQueryObjectFn, NtQuerySecurityObjectFn, NtQueryValueKeyFn, NtQueryVolumeInformationFileFn,
    NtReadFileFn, NtRenameKeyFn, NtReplaceKeyFn, NtRestoreKeyFn, NtSaveKeyExFn, NtSaveKeyFn,
    NtSaveMergedKeysFn, NtSetInformationFileFn, NtSetInformationKeyFn, NtSetInformationObjectFn,
    NtSetSecurityObjectFn, NtSetValueKeyFn, NtUnloadKey2Fn, NtUnloadKeyFn, NtUnlockFileFn,
    NtUnmapViewOfSectionFn, NtWriteFileFn,
};
use crate::tramp::{RawTramp, Tramp};
use retour::RawDetour;
use std::collections::BTreeSet;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

/// Errors installing the hooks.
#[derive(Debug)]
pub enum InstallError {
    AlreadyInstalled,
    NtdllMissing,
    ProcMissing,
    Detour,
}

// The trampoline slots, one `Tramp` per row of `detour_table!`. Each is set once, before its
// detour is enabled, and only read from the hooks after.
macro_rules! tramp_statics_from_table {
    ($(
        {
            export: $export:literal,
            stat: [$($stat:tt)*],
            tramp: $tramp:ident: $fnty:ty,
            install: $kind:ident, group: $group:ident, flags: [$($flag:ident),*],
            $($rest:tt)*
        }
    )*) => {
        $(pub(super) static $tramp: Tramp<$fnty> = Tramp::new();)*
    };
}

detour_table!(tramp_statics_from_table);

/// What the install does with a detour whose export is missing or cannot be enabled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Install {
    /// Failure aborts the whole install ([`InstallError`]).
    Required,
    /// Skipped, and the export name is added to [`skipped_detours`].
    Optional,
    /// An export ntdll does not have is skipped silently (it cannot be called, so nothing goes
    /// unvirtualised without it); one it has that cannot be detoured is reported like `Optional`.
    IfPresent,
    /// Skipped silently, whatever the reason.
    BestEffort,
}

/// Which install pass a detour belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Group {
    File,
    /// Only with `VFS_REGISTRY` set; all or nothing (`regclient::detours_installed`).
    Registry,
    Process,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flag {
    /// Owned by the early payload in a dual-layer install: not detoured by `install_late`.
    Early,
    /// The slot holds ntdll's own export whenever the detour is not installed.
    RawFallback,
    /// The registry overlay cannot run without it: a miss is added to its `missing` set.
    NeededByRegistry,
}

/// One row of `detour_table!`, at run time.
struct Detour {
    export: &'static core::ffi::CStr,
    hook: *const (),
    tramp: &'static RawTramp,
    install: Install,
    group: Group,
    flags: &'static [Flag],
}

macro_rules! detour_row {
    ($export:literal, $hook:ident, $tramp:ident, $kind:ident, $group:ident,
     [$($flag:ident),*]) => {
        Detour {
            export: match core::ffi::CStr::from_bytes_with_nul(concat!($export, "\0").as_bytes()) {
                Ok(c) => c,
                Err(_) => panic!("export name has a NUL"),
            },
            hook: $hook as *const (),
            tramp: $tramp.raw(),
            install: Install::$kind,
            group: Group::$group,
            flags: &[$(Flag::$flag),*],
        }
    };
}

macro_rules! detours_from_table {
    ($(
        {
            export: $export:literal,
            stat: [$($stat:tt)*],
            tramp: $tramp:ident: $fnty:ty,
            install: $kind:ident, group: $group:ident, flags: [$($flag:ident),*],
            $(#[$attr:meta])*
            hook: $hook:ident = $($rest:tt)*
        }
    )*) => {
        /// Every detour, in install order.
        fn detour_rows() -> Vec<Detour> {
            vec![$(detour_row!($export, $hook, $tramp, $kind, $group, [$($flag),*])),*]
        }
    };
}

detour_table!(detours_from_table);

impl Detour {
    fn has(&self, flag: Flag) -> bool {
        self.flags.contains(&flag)
    }

    fn label(&self) -> &'static str {
        self.export.to_str().unwrap_or("?")
    }

    /// Build the detour and store its trampoline. **The store comes before the enable**, so no
    /// call can reach the hook while its trampoline is unset.
    unsafe fn prepare(&self, lib: HMODULE) -> Result<RawDetour, InstallError> {
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        let d = unsafe { make_detour(lib, self.export, self.hook) }?;
        // SAFETY: `d`'s trampoline calls this export, whose type the table gave the slot.
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        unsafe { self.tramp.store(Some(d.trampoline() as *const ())) };
        Ok(d)
    }

    /// Put the slot back to what it holds with no detour installed: ntdll's own export for a
    /// `RawFallback` row (`regkeys` reads key names through it), else empty.
    unsafe fn reset(&self, lib: HMODULE) {
        let raw = if self.has(Flag::RawFallback) {
            // SAFETY: FFI call with valid arguments.
            unsafe { GetProcAddress(lib, self.export.as_ptr().cast()) }.map(|p| p as *const ())
        } else {
            None
        };
        // SAFETY: ntdll's export has the signature the table gave the slot.
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        unsafe { self.tramp.store(raw) };
    }

    /// `prepare` + enable for a row that may be skipped: `true` when it is in. A failure resets
    /// the slot again (`BestEffort` leaves it, as the hand-written install did: a detour that is
    /// not enabled never calls its hook). `Optional` and `IfPresent` failures are noted in
    /// [`SKIPPED_DETOURS`].
    unsafe fn install_soft(&self, lib: HMODULE, detours: &mut Vec<RawDetour>) -> bool {
        if self.install == Install::IfPresent
            // SAFETY: FFI call with valid arguments.
            && unsafe { GetProcAddress(lib, self.export.as_ptr().cast()) }.is_none()
        {
            return true;
        }
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        if let Ok(d) = unsafe { self.prepare(lib) } {
            // SAFETY: FFI call with valid arguments.
            if unsafe { d.enable() }.is_ok() {
                detours.push(d);
                return true;
            }
            if self.install != Install::BestEffort {
                // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                unsafe { self.reset(lib) };
            }
        }
        if self.install != Install::BestEffort {
            note_skipped_detour(self.label());
        }
        false
    }
}

/// Keeps the detours alive; dropping it disables the hooks.
pub struct HookGuard {
    _detours: Vec<RawDetour>,
}

/// Detours `install_all_detours` passed over because the host's ntdll does not
/// export the function. `BTreeSet::new()` is `const`, so this needs no lazy init.
///
/// Empty on Windows. Non-empty under Wine, whose ntdll omits the newer
/// enumeration and query entry points. This exists so a skipped hook is a fact a
/// host can read rather than an invisible gap: an unhooked handle-taking NT API
/// does not error, it quietly serves the real directory instead of the composed
/// one, which reads exactly like a mod list that is simply empty.
static SKIPPED_DETOURS: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());

fn note_skipped_detour(name: &'static str) {
    if let Ok(mut s) = SKIPPED_DETOURS.lock() {
        s.insert(name);
    }
}

/// Names of hooks not installed because ntdll had no such export.
///
/// Empty means every hook this build knows about is live. A caller that requires
/// total interception should treat a non-empty result as fatal rather than
/// advisory — see [`SKIPPED_DETOURS`].
pub fn skipped_detours() -> Vec<&'static str> {
    SKIPPED_DETOURS
        .lock()
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default()
}

/// Resolve `name` in ntdll and build (not yet enabled) a detour to `hookfn`.
unsafe fn make_detour(
    ntdll: HMODULE,
    name: &core::ffi::CStr,
    hookfn: *const (),
) -> Result<RawDetour, InstallError> {
    // SAFETY: FFI call with valid arguments.
    let proc =
        unsafe { GetProcAddress(ntdll, name.as_ptr().cast()) }.ok_or(InstallError::ProcMissing)?;
    // SAFETY: FFI call with valid arguments.
    unsafe { RawDetour::new(proc as *const (), hookfn) }.map_err(|_| InstallError::Detour)
}

/// Install all detours backed by `engine` (in-process / no early payload).
/// Idempotent-guarded. Patches the four path/attr stubs itself.
pub fn install(engine: Engine) -> Result<HookGuard, InstallError> {
    install_panic_hook();
    // Before the detours go live: creating the breadcrumb file is real I/O, and
    // once hooks are installed that I/O re-enters them.
    crate::breadcrumb::init();
    crate::hookstats::start_reporter();
    ENGINE
        .set(engine)
        .map_err(|_| InstallError::AlreadyInstalled)?;
    // SAFETY: ntdll lookup + detour install; each hook matches its ABI.
    unsafe { install_all_detours(true) }
}

/// True when the `Early` rows of the detour table are exactly the four slots `install_late`
/// fills from the early payload: create, open, qattr and qfull.
fn early_rows_are_the_payload_slots() -> bool {
    let payload: [*const RawTramp; 4] = [
        TRAMP_CREATE.raw(),
        TRAMP_OPEN.raw(),
        TRAMP_QATTR.raw(),
        TRAMP_QFULL.raw(),
    ];
    let rows = detour_rows();
    let early: Vec<*const RawTramp> = rows
        .iter()
        .filter(|d| d.has(Flag::Early))
        .map(|d| d.tramp as *const RawTramp)
        .collect();
    early.len() == payload.len() && payload.iter().all(|p| early.contains(p))
}

/// Dual-layer install: early payload already owns open/create/qattr/qfull.
/// Wire trampolines to the early Config's tramp buffers, publish secondary
/// dispatch pointers into that Config, and detour only the remaining stubs.
///
/// `payload_cfg` is the reflectively-mapped early Config in this process.
///
/// # Safety
/// `payload_cfg` must point at a live [`PayloadConfig`](vfs_inject::PayloadConfig)
/// written by the injector into this process, and stay valid for the call.
pub unsafe fn install_late(
    engine: Engine,
    payload_cfg: *mut vfs_inject::PayloadConfig,
) -> Result<HookGuard, InstallError> {
    if payload_cfg.is_null() {
        return Err(InstallError::Detour);
    }
    install_panic_hook();
    // Before the detours go live: creating the breadcrumb file is real I/O, and
    // once hooks are installed that I/O re-enters them.
    crate::breadcrumb::init();
    crate::hookstats::start_reporter();
    ENGINE
        .set(engine)
        .map_err(|_| InstallError::AlreadyInstalled)?;

    // `install_all_detours(false)` below skips the `Early` rows, and the block below fills the
    // slots of exactly the four rows that are `Early`. A fifth `Early` row would be left with an
    // empty slot, and a hook that is not `Early` would lose its trampoline.
    debug_assert!(
        early_rows_are_the_payload_slots(),
        "the `Early` rows of detour_table! are not the four payload slots install_late sets"
    );

    // SAFETY: cfg is the live early Config in this process; tramp addresses
    // are RWX pages the injector allocated; secondary pointers are our hooks.
    unsafe {
        let cfg = &mut *payload_cfg;
        // Call originals via the early payload's trampolines (real ntdll tails).
        TRAMP_CREATE.set(Some(core::mem::transmute::<usize, NtCreateFileFn>(
            cfg.create_tramp,
        )));
        TRAMP_OPEN.set(Some(core::mem::transmute::<usize, NtOpenFileFn>(
            cfg.open_tramp,
        )));
        TRAMP_QATTR.set(Some(
            core::mem::transmute::<usize, NtQueryAttributesFileFn>(cfg.qattr_tramp),
        ));
        TRAMP_QFULL.set(Some(core::mem::transmute::<
            usize,
            NtQueryFullAttributesFileFn,
        >(cfg.qfull_tramp)));

        // Publish secondary last-ish: hooks become Engine-backed for non-table paths.
        core::ptr::write_volatile(&mut cfg.secondary_create, create_hook as *const () as usize);
        core::ptr::write_volatile(&mut cfg.secondary_open, open_hook as *const () as usize);
        core::ptr::write_volatile(&mut cfg.secondary_qattr, qattr_hook as *const () as usize);
        core::ptr::write_volatile(&mut cfg.secondary_qfull, qfull_hook as *const () as usize);

        // Do NOT patch the four early-owned stubs.
        install_all_detours(false)
    }
}

/// `patch_early_owned`: when true, also detour the four path/attr stubs
/// (standalone install). When false, only remainder detours (dual-layer).
///
/// The file detours are walked in `detour_table!` order, which is the install order:
///
///  1. the `Early` rows, if asked: every one is built and has its trampoline stored, then all
///     are enabled;
///  2. the other file rows in order. A `Required` one is built and stored but **not yet
///     enabled**; an `Optional` one is enabled at once (it may turn out absent);
///  3. `host_name_convention` is decided, then the `Required` rows from step 2 are enabled.
unsafe fn install_all_detours(patch_early_owned: bool) -> Result<HookGuard, InstallError> {
    // SAFETY: FFI call with valid arguments.
    let ntdll = unsafe { GetModuleHandleA(c"ntdll.dll".as_ptr().cast()) };
    if ntdll.is_null() {
        return Err(InstallError::NtdllMissing);
    }

    let rows = detour_rows();
    let mut detours: Vec<RawDetour> = Vec::new();

    if patch_early_owned {
        let mut early = Vec::new();
        for d in rows
            .iter()
            .filter(|d| d.group == Group::File && d.has(Flag::Early))
        {
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            early.push(unsafe { d.prepare(ntdll) }?);
        }
        for d in &early {
            // SAFETY: FFI call with valid arguments.
            unsafe { d.enable() }.map_err(|_| InstallError::Detour)?;
        }
        detours.extend(early);
    }

    // File detours the registry overlay depends on that are not in (`NeededByRegistry`).
    let mut registry_missing: Vec<&'static str> = Vec::new();
    let mut deferred = Vec::new();
    for d in rows
        .iter()
        .filter(|d| d.group == Group::File && !d.has(Flag::Early))
    {
        if d.install == Install::Required {
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            deferred.push(unsafe { d.prepare(ntdll) }?);
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        } else if !unsafe { d.install_soft(ntdll, &mut detours) } && d.has(Flag::NeededByRegistry) {
            registry_missing.push(d.label());
        }
    }
    // Decided here, once, rather than inside the first name query: its
    // fallback asks the loader for a module, and a hook that takes the loader
    // lock while holding this `OnceLock` can deadlock against a thread that
    // holds the loader lock and makes a name query of its own.
    let _ = host_name_convention();

    for d in &deferred {
        // SAFETY: FFI call with valid arguments.
        unsafe { d.enable() }.map_err(|_| InstallError::Detour)?;
    }
    // Every enabled detour must be kept alive here: dropping one silently
    // un-patches it, which reads exactly like "the process never calls this".
    detours.extend(deferred);

    // The registry overlay's detours go in only when the host asked for the overlay
    // (`VFS_REGISTRY`): with it unset this process's registry calls, and its process-wide
    // object calls (`NtDuplicateObject`, security, handle flags), are not detoured at all.
    // `NtClose` and `NtQueryObject` above are the file hooks'; with the overlay off they never
    // touch the registry tables (`regclient::enabled` is false).
    if vfs_env::opt_in(vfs_env::REGISTRY) {
        let before = detours.len();
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        unsafe { install_registry_detours(ntdll, &rows, &mut detours, registry_missing) };
        REG_DETOURS_INSTALLED.store(detours.len() - before, std::sync::atomic::Ordering::Relaxed);
    } else {
        crate::regclient::overlay_off();
    }

    // Best-effort child-process propagation + virtual image path spoof.
    if let Some(dll) = self_dll_path() {
        let _ = SELF_DLL.set(dll);
        // SAFETY: FFI call with valid arguments.
        let mut kb = unsafe { GetModuleHandleA(c"kernelbase.dll".as_ptr().cast()) };
        if kb.is_null() {
            // SAFETY: FFI call with valid arguments.
            kb = unsafe { GetModuleHandleA(c"kernel32.dll".as_ptr().cast()) };
        }
        if !kb.is_null() {
            // `BestEffort`: a failure here costs only child-process propagation, silently.
            for d in rows.iter().filter(|d| d.group == Group::Process) {
                // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                unsafe { d.install_soft(kb, &mut detours) };
            }
        }
    }

    Ok(HookGuard { _detours: detours })
}

/// Registry detours the install put in (0 with `VFS_REGISTRY` unset). For tests and diagnostics.
static REG_DETOURS_INSTALLED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// How many registry overlay detours this process's install put in: 0 when the overlay is off
/// (`VFS_REGISTRY` unset). For tests and diagnostics.
pub fn registry_detours_installed() -> usize {
    REG_DETOURS_INSTALLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The registry overlay's detours (spec section 3.1: open, create, duplicate; `NtClose` and
/// `NtQueryObject` are the file hooks'). Installed only when the host turned the overlay on
/// (`VFS_REGISTRY`, see `install_all_detours`); each still checks `regclient::enabled()` first
/// and goes straight to its trampoline when it is off (a missing detour turns it off).
/// `Optional` in the style of `NtQueryObject`, so a host without one still gets the file VFS.
///
/// Registry virtualisation is all or nothing (`regclient::enabled`): every detour that could
/// not be installed is collected, and the outcome is recorded once, after the last one.
/// `missing` arrives holding the file detours the overlay depends on that are not in (the file
/// hooks' `NtQueryObject`: names and types of synthetic keys, the access of pre-hook handles),
/// so their absence turns the overlay off like a missing registry detour.
unsafe fn install_registry_detours(
    ntdll: HMODULE,
    rows: &[Detour],
    detours: &mut Vec<RawDetour>,
    mut missing: Vec<&'static str>,
) {
    let registry = || rows.iter().filter(|d| d.group == Group::Registry);
    // `regkeys` reads key names through the `RawFallback` slot even when its detour is not in.
    for d in registry().filter(|d| d.has(Flag::RawFallback)) {
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        unsafe { d.reset(ntdll) };
    }
    // Each trampoline is stored before its detour is enabled.
    for d in registry() {
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        if !unsafe { d.install_soft(ntdll, detours) } {
            missing.push(d.label());
        }
    }
    crate::regclient::detours_installed(&missing);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_early_rows_are_the_four_payload_slots() {
        assert!(early_rows_are_the_payload_slots());
    }
}
