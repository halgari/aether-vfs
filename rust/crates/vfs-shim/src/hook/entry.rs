//! Hook entry points: the reentrancy guard, panic containment and the `extern "system"` wrappers.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{
    close_hook_body, compress_key_hook_body, cpiw_hook_body, create_hook_body,
    create_key_hook_body, create_key_tx_hook_body, create_section_hook_body, delete_hook_body,
    delete_key_hook_body, delete_value_key_hook_body, dup_hook_body, enum_key_hook_body,
    enum_value_hook_body, flush_hook_body, flush_key_hook_body, load_key2_hook_body,
    load_key3_hook_body, load_key_ex_hook_body, load_key_hook_body, lock_hook_body,
    lock_registry_key_hook_body, map_view_hook_body, notify_key_hook_body,
    notify_multiple_hook_body, open_hook_body, open_key_ex_hook_body, open_key_hook_body,
    open_key_tx_ex_hook_body, open_key_tx_hook_body, qattr_hook_body, qdir_hook_body,
    qdirex_hook_body, qfull_hook_body, qibn_hook_body, qif_hook_body, qobj_hook_body,
    query_key_hook_body, query_multiple_hook_body, query_security_hook_body, query_value_hook_body,
    qvol_hook_body, read_hook_body, rename_key_hook_body, replace_key_hook_body,
    restore_key_hook_body, save_key_ex_hook_body, save_key_hook_body, save_merged_hook_body,
    set_info_key_hook_body, set_info_object_hook_body, set_security_hook_body,
    set_value_key_hook_body, setinfo_hook_body, unload_key2_hook_body, unload_key_ex_hook_body,
    unload_key_hook_body, unlock_hook_body, unmap_view_hook_body, write_hook_body,
};
use crate::ntdef::{
    FileBasicInformation, FileNetworkOpenInformation, ObjectAttributes, UnicodeString,
    STATUS_UNSUCCESSFUL,
};
use core::cell::Cell;
use core::ffi::c_void;
use windows_sys::Win32::Foundation::{ERROR_INTERNAL_ERROR, HANDLE, NTSTATUS};
use windows_sys::Win32::System::Threading::{PROCESS_INFORMATION, STARTUPINFOW};

// Host-side metadata probes from inside hooks must not re-enter detours
// (`Path::is_file` → NtCreateFile → try_fuse_create → … → stack overflow).
thread_local! {
    static HOOK_REENTER: Cell<u32> = const { Cell::new(0) };
}

pub(super) fn in_hook_reenter() -> bool {
    HOOK_REENTER.with(|c| c.get() > 0)
}

/// RAII form of [`HOOK_REENTER`] for shim-initiated file I/O. `enter()` returns
/// `None` when the guard is already held on this thread — the caller's signal
/// that it is already running *inside* the shim's own I/O and must not start
/// more. While it is held, every NT file call this thread makes takes
/// `create_hook`/`open_hook`'s `in_hook_reenter` fast path straight to the real
/// ntdll — which is the point: the shim's own file I/O (the stats report, the
/// panic log, the diagnostic traces) must reach the real filesystem, not be
/// re-decided by the hooks it runs inside.
///
/// This is the only way to raise the counter, deliberately: a raw begin/end pair
/// skipped `end` when a panic fell between the two, and a counter stuck at 1
/// silently stops virtualizing that thread for good. Here `Drop` lowers it,
/// unwinding included.
/// See docs/shim-invariants.md, "Panic containment".
pub(crate) struct ShimIoGuard(());

impl ShimIoGuard {
    pub(crate) fn enter() -> Option<Self> {
        HOOK_REENTER.with(|c| {
            let d = c.get();
            if d > 0 {
                return None;
            }
            c.set(d + 1);
            Some(ShimIoGuard(()))
        })
    }
}

impl Drop for ShimIoGuard {
    fn drop(&mut self) {
        HOOK_REENTER.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// What every hook returns when [`contain_panic`] catches a panic in its body.
///
/// It is a failure (severity bits set), so no caller mistakes it for a completed
/// operation or reads an output buffer the hook never filled. It is the generic
/// `STATUS_UNSUCCESSFUL` and nothing more specific: a panic means the shim does not
/// know what happened, and a specific status is a claim it cannot make
/// (`NAME_NOT_FOUND` gets baked into a load order, `NO_MORE_FILES` truncates a
/// listing). `cpiw_hook` returns `FALSE` instead, since `CreateProcessInternalW`
/// returns a `BOOL` and this constant's bit pattern reads as `TRUE` there; see the
/// `on_panic` column of its row in `detour_table!`.
///
/// Several hooks return this same status when their trampoline is missing, so the
/// value alone does not say a panic happened: `hookstats::note_hook_panic` and the
/// shim panic log do.
/// See docs/shim-invariants.md, "Panic containment".
pub(super) const STATUS_HOOK_PANICKED: NTSTATUS = STATUS_UNSUCCESSFUL;

/// Run one hook body with its panic contained at the `extern "system"`
/// boundary, and report `on_panic`'s value to the caller if it faults.
///
/// A panic inside an `extern "system"` fn unwinds the Rust frames below the
/// boundary and then hits rustc's forced `panic_cannot_unwind`, killing the process
/// with `0xC0000409`. This wrapper changes only where the unwind stops: the same
/// destructors run, and the game gets a returned status instead of a dead process.
/// The inner destructors running is a requirement: [`ShimIoGuard`]'s `Drop` releases
/// the thread's reentrancy counter.
///
/// # `AssertUnwindSafe`
///
/// The closures capture raw pointers and touch statics, so none is `UnwindSafe`; the
/// assertion is sound because the `Err` arm observes nothing from the closure.
/// Process-wide tables sit behind `Mutex`es that poison on a panic taken while held,
/// and every lock site treats `Err` as "no entry". The cost: a panic under one of
/// those locks degrades its handle tracking to empty for the rest of the process,
/// which is why a caught panic is counted loudly.
///
/// # Visibility
///
/// `pub` because `vfs-shim-dll` owns entry points (`DllMain`,
/// `vfs_shim_sync_bootstrap`) that must contain their panics by the same route;
/// `no_extern_hook_bypasses_the_panic_containment_macro` checks both crates against
/// this one marker.
/// See docs/shim-invariants.md, "Panic containment".
pub fn contain_panic<R>(
    name: &'static str,
    body: impl FnOnce() -> R,
    on_panic: impl FnOnce() -> R,
) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(v) => v,
        Err(_) => {
            // The panic's message, location and thread were already written by
            // `install_panic_hook`, which runs before any unwinding. This adds
            // the aggregate the log cannot give: how many, and where.
            crate::hookstats::note_hook_panic(name);
            on_panic()
        }
    }
}

/// Generates the `extern "system"` entry point for each detour: a wrapper that
/// runs the real body inside [`contain_panic`].
///
/// Every hook goes through this one macro rather than each wrapping its own
/// body, so "does this entry point contain its panics" cannot be answered
/// differently for different hooks — and so a hook added later cannot quietly
/// skip it. `no_extern_hook_bypasses_the_panic_containment_macro` in this
/// module's tests enforces that by scanning the source: this macro must be the
/// only place in `hook/` that defines an `extern "system"` function. The real detours reach
/// it through [`entry_points_from_table`], so their export names come from `detour_table!`.
macro_rules! hook_entry_points {
    ($(
        $(#[$attr:meta])*
        fn $wrapper:ident = $body:ident($($arg:ident: $ty:ty),* $(,)?) -> $ret:ty
            as $name:literal, on_panic $fallback:expr;
    )*) => {$(
        #[doc = concat!(
            "Panic-contained `extern \"system\"` entry point for `", $name,
            "`. The body is [`", stringify!($body), "`]; this wrapper exists so a \
             panic in it returns a failure status instead of aborting the game — \
             see [`contain_panic`]."
        )]
        $(#[$attr])*
        #[allow(clippy::too_many_arguments)]
        pub(super) unsafe extern "system" fn $wrapper($($arg: $ty),*) -> $ret {
            contain_panic($name, || unsafe { $body($($arg),*) }, || $fallback)
        }
    )*};
}

/// Feeds `detour_table!`'s rows to [`hook_entry_points!`]: one wrapper per row.
macro_rules! entry_points_from_table {
    ($(
        {
            export: $export:literal,
            stat: [$($stat:tt)*],
            tramp: $tramp:ident: $fnty:ty,
            install: $kind:ident, group: $group:ident, flags: [$($flag:ident),*],
            $(#[$attr:meta])*
            hook: $wrapper:ident = $body:ident($($arg:ident: $ty:ty),* $(,)?) -> $ret:ty,
            on_panic: $fallback:expr,
            $($rest:tt)*
        }
    )*) => {
        hook_entry_points! {
            $(
                $(#[$attr])*
                fn $wrapper = $body($($arg: $ty),*) -> $ret
                    as $export, on_panic $fallback;
            )*
        }
    };
}

detour_table!(entry_points_from_table);

/// Record shim panics: message, location and thread go to `VFS_SHIM_PANIC_LOG`,
/// else `<state dir>/shim-panic.log`, else a fixed fallback (a panic here must
/// never be silent for want of a path).
///
/// A contained panic is invisible from outside the process, so this log is the only
/// place its message survives; `hookstats`' counters are the aggregate. The hook runs
/// at panic time, before any unwinding, so a logged message does not imply the
/// process died. For the panics that still abort (inside this hook, or outside the
/// containment) it also attributes the `0xC0000409` exit.
/// See docs/shim-invariants.md, "Panic containment".
pub(super) fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let default = vfs_env::text(vfs_env::STATE_DIR).map(|d| format!("{d}\\shim-panic.log"));
        let path = vfs_env::text(vfs_env::SHIM_PANIC_LOG)
            .or(default)
            .unwrap_or_else(|| r"C:\tmp\skyrim-data\vfs-state\shim-panic.log".to_string());
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let loc = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown location>".into());
            let msg = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string payload>".into());
            let line = format!(
                "pid={} tid={:?} at {loc}\n  {msg}\n",
                std::process::id(),
                std::thread::current().id()
            );
            // Guard the write: this file I/O re-enters our own NtCreateFile
            // hooks, and a panic raised *inside* a hook would otherwise recurse.
            if let Some(_io) = ShimIoGuard::enter() {
                if let Some(parent) = std::path::Path::new(&path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let _ = f.write_all(line.as_bytes());
                    let _ = f.flush();
                }
            }
            // Best-effort stderr too; harmless when the child has no console.
            eprintln!("vfs-shim PANIC {line}");
            prev(info);
        }));
    });
}

/// Run `f` as the shim's own work on this thread (a [`ShimIoGuard`] held), so every hook it
/// reaches is bypassed. For tests of the bypass paths only.
#[doc(hidden)]
pub fn as_shim_io_for_tests<R>(f: impl FnOnce() -> R) -> R {
    let _io = ShimIoGuard::enter();
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntdef::{STATUS_ACCESS_DENIED, STATUS_SUCCESS};

    // ---------------------------------------------------------------------
    // Panic containment at the `extern "system"` boundary.
    //
    // These drive test-only bodies registered through the *same*
    // `hook_entry_points!` macro every real detour uses, so what they exercise
    // is the generated wrapper and `contain_panic`, not a hand-written
    // stand-in. `no_extern_hook_bypasses_the_panic_containment_macro` is the
    // other half: it pins that no real hook can be defined any other way.
    //
    // A real hook cannot be made to panic from a unit test — every one of them
    // reaches its interesting code only through a live trampoline into ntdll,
    // and a trampoline is itself `extern "system"`, so a panic planted there
    // would abort at *its* boundary before ever reaching ours.
    // ---------------------------------------------------------------------

    use std::thread::LocalKey;

    thread_local! {
        /// Set by a frame standing in for the game code below the detour.
        static CALLER_FRAME_DROPPED: Cell<bool> = const { Cell::new(false) };
        /// Set by a frame *inside* the hook body, above the catch.
        static HOOK_FRAME_DROPPED: Cell<bool> = const { Cell::new(false) };
    }

    struct DropFlag(&'static LocalKey<Cell<bool>>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.with(|c| c.set(true));
        }
    }

    unsafe fn panicking_hook_body(reached: *mut u32) -> NTSTATUS {
        let _frame = DropFlag(&HOOK_FRAME_DROPPED);
        if !reached.is_null() {
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            unsafe {
                *reached = 1;
            }
        }
        panic!("deliberate test panic inside a hook body");
    }

    unsafe fn counted_panicking_hook_body() -> NTSTATUS {
        panic!("deliberate test panic, counted by name");
    }

    /// Panics with the thread's reentrancy guard held, which is the case
    /// requirement 2 is about: [`ShimIoGuard`] is the only thing that lowers
    /// `HOOK_REENTER` again.
    unsafe fn guarded_panicking_hook_body() -> NTSTATUS {
        let Some(_io) = ShimIoGuard::enter() else {
            return STATUS_ACCESS_DENIED;
        };
        panic!("deliberate test panic with shim I/O in flight");
    }

    /// Reports what a *real* hook's first branch would see: `in_hook_reenter`
    /// decides between serving the call and handing it to real ntdll.
    unsafe fn reentrancy_probe_body() -> NTSTATUS {
        if in_hook_reenter() {
            STATUS_ACCESS_DENIED
        } else {
            STATUS_SUCCESS
        }
    }

    unsafe fn panicking_bool_hook_body() -> i32 {
        panic!("deliberate test panic in a BOOL-returning hook");
    }

    hook_entry_points! {
        fn panicking_hook = panicking_hook_body(reached: *mut u32) -> NTSTATUS
            as "NtTestPanic", on_panic STATUS_HOOK_PANICKED;

        fn counted_panicking_hook = counted_panicking_hook_body() -> NTSTATUS
            as "NtTestCounted", on_panic STATUS_HOOK_PANICKED;

        fn guarded_panicking_hook = guarded_panicking_hook_body() -> NTSTATUS
            as "NtTestPanicUnderShimIo", on_panic STATUS_HOOK_PANICKED;

        fn reentrancy_probe = reentrancy_probe_body() -> NTSTATUS
            as "NtTestReentrancyProbe", on_panic STATUS_HOOK_PANICKED;

        fn panicking_bool_hook = panicking_bool_hook_body() -> i32
            as "TestPanicBool", on_panic {
            unsafe { windows_sys::Win32::Foundation::SetLastError(ERROR_INTERNAL_ERROR) };
            0
        };
    }

    /// The caller gets a status back at all — and it is a *failure* status.
    /// Both halves matter: without the first the process is gone, and without
    /// the second the game reads an output buffer the panicking body never
    /// filled in.
    #[test]
    fn a_panicking_hook_returns_a_failure_status_instead_of_aborting() {
        let mut reached = 0u32;
        let st = unsafe { panicking_hook(&mut reached) };
        assert_eq!(reached, 1, "the body must have run far enough to panic");
        assert_eq!(st, STATUS_UNSUCCESSFUL);
        assert!(
            st < 0,
            "NTSTATUS {st:#x} has no severity bits — a caller reads it as success"
        );
    }

    /// The unwind stops at the `extern "system"` boundary and does not run the
    /// caller's destructors on its way out.
    ///
    /// The caller's frame here stands in for the game's. Measured 2026-08-16,
    /// an uncontained panic did not unwind into it either — rustc's forced
    /// `panic_cannot_unwind` sits at *our* boundary, so the game's frames were
    /// never at risk and the process simply died instead. What this pins is
    /// that adding the catch did not move the boundary outwards: the frame
    /// below the hook is untouched when the hook returns, and drops normally
    /// afterwards like any other.
    ///
    /// The second assertion is the deliberate other side of it. The hook's own
    /// frame **is** unwound, and must be: that is the mechanism releasing
    /// `ShimIoGuard`, and a containment that skipped it would trade a crash for
    /// a thread that silently stops being virtualized.
    #[test]
    fn a_panicking_hook_does_not_run_its_callers_destructors() {
        CALLER_FRAME_DROPPED.with(|c| c.set(false));
        HOOK_FRAME_DROPPED.with(|c| c.set(false));
        {
            let _caller = DropFlag(&CALLER_FRAME_DROPPED);
            let mut reached = 0u32;
            let st = unsafe { panicking_hook(&mut reached) };
            assert_eq!(st, STATUS_UNSUCCESSFUL);
            assert!(
                !CALLER_FRAME_DROPPED.with(Cell::get),
                "the unwind escaped the hook and ran a caller frame's Drop"
            );
            assert!(
                HOOK_FRAME_DROPPED.with(Cell::get),
                "the hook body's own frame was not unwound — nothing would release ShimIoGuard"
            );
        }
        assert!(
            CALLER_FRAME_DROPPED.with(Cell::get),
            "the caller frame must still drop normally once it goes out of scope"
        );
    }

    /// After a caught panic the thread is still usable. A panic taken while the
    /// reentrancy guard was held used to be unrecoverable in the worst possible
    /// way — `HOOK_REENTER` stuck at 1 means every later hook call on that
    /// thread takes the fast path to real ntdll, so the process quietly stops
    /// being virtualized while every counter keeps reporting ordinary activity.
    #[test]
    fn a_caught_panic_leaves_no_reentrancy_state_held() {
        assert!(!in_hook_reenter(), "test thread started inside the guard");
        assert_eq!(unsafe { reentrancy_probe() }, STATUS_SUCCESS);

        assert_eq!(unsafe { guarded_panicking_hook() }, STATUS_UNSUCCESSFUL);

        assert!(
            !in_hook_reenter(),
            "HOOK_REENTER stayed raised after the panic"
        );
        assert_eq!(
            unsafe { reentrancy_probe() },
            STATUS_SUCCESS,
            "the next call on this thread took the reentrant fast path to real ntdll"
        );
        assert!(
            ShimIoGuard::enter().is_some(),
            "no further shim-initiated I/O could be started on this thread"
        );
    }

    /// A caught panic is a bug that just happened; it has to be countable.
    #[test]
    fn every_caught_panic_is_counted_under_its_own_entry_point() {
        let before_total = crate::hookstats::hook_panics_total();
        assert_eq!(crate::hookstats::hook_panic_count("NtTestCounted"), 0);
        assert_eq!(unsafe { counted_panicking_hook() }, STATUS_UNSUCCESSFUL);
        assert_eq!(
            crate::hookstats::hook_panic_count("NtTestCounted"),
            1,
            "the panic was not attributed to the entry point it happened in"
        );
        // Other panic tests may run concurrently, so the total is a lower bound.
        assert!(crate::hookstats::hook_panics_total() > before_total);
    }

    /// `CreateProcessInternalW` returns a Win32 `BOOL`, and the uniform
    /// `STATUS_UNSUCCESSFUL` is non-zero in that ABI — i.e. `TRUE`. A caller
    /// told the process was created goes on to use a `PROCESS_INFORMATION`
    /// nothing filled in.
    #[test]
    fn a_panicking_bool_hook_reports_false_rather_than_a_status() {
        assert_ne!(
            STATUS_HOOK_PANICKED, 0,
            "the trap this test exists for is gone"
        );
        let r = unsafe { panicking_bool_hook() };
        assert_eq!(r, 0, "a BOOL-returning hook must fail with FALSE");
        assert_eq!(
            unsafe { windows_sys::Win32::Foundation::GetLastError() },
            ERROR_INTERNAL_ERROR,
            "a failing BOOL Win32 function must set the last error, not leave a stale one"
        );
    }

    /// Structural: **every** `extern "system"` function in the injected DLL must
    /// contain its own panic, by calling [`contain_panic`] in its body.
    ///
    /// Requirement 1 of task 1 is that *every* entry point contains its panic,
    /// and the behaviour tests above can only ever demonstrate that for the hooks
    /// they drive. This is what makes the claim total, and what stops the next hook
    /// from being written the old way — which would compile, install,
    /// and abort the game exactly as before, with nothing in any test to say so.
    ///
    /// ## Its first version had a hole, and the hole was real
    ///
    /// The check used to be an `include_str!` of the one `hook.rs` source file (before it
    /// became the `hook/` modules) and an assertion that the
    /// only definition found was the macro's `$wrapper`. That is a *hand-written
    /// file list of one*, and it missed `lazy_section.rs`'s `veh_handler` — an
    /// uncontained `extern "system"` entry point which additionally carried a raw
    /// `IN_VEH.set(true)` / `set(false)` pair, the exact latch defect task 1
    /// removed everywhere else. It went unseen through the whole of stage 4
    /// *because the guard was believed*.
    ///
    /// So the enumeration is derived, not written:
    ///
    ///  * the **directories** are the two crates that compose the injected DLL —
    ///    this one and `vfs-shim-dll`. `vfs-payload` is deliberately absent: it is
    ///    `#![no_std]` with `panic = "abort"` and its own workspace, so it has no
    ///    unwind to contain and could not call this function if it wanted to;
    ///  * the **files** are read off those directories recursively at test time,
    ///    so a new module cannot be added outside the check;
    ///  * the **rule** is per-definition rather than a name list, so a new entry
    ///    point is checked the moment it is written and no allowlist has to be
    ///    edited to admit a legitimate one.
    #[test]
    fn no_extern_hook_bypasses_the_panic_containment_macro() {
        const MARKER: &str = "extern \"system\" fn ";
        // Spelled with `concat!` so the needle does not appear verbatim in the
        // files it is searching (this file is one of them).
        let containment = concat!("contain", "_panic");

        let this_crate = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dirs = [
            this_crate.join("src"),
            this_crate
                .parent()
                .expect("crates/")
                .join("vfs-shim-dll")
                .join("src"),
        ];

        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let entries =
                std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        for d in &dirs {
            assert!(d.is_dir(), "{} is not a directory", d.display());
            walk(d, &mut files);
        }
        files.sort();
        // A broken enumeration must fail loudly rather than pass vacuously.
        assert!(
            files.iter().any(|p| p.ends_with("hook/entry.rs"))
                && files.iter().any(|p| p.ends_with("lazy_section.rs"))
                && files.len() >= 14,
            "the enumeration must have found both crates' sources — got {files:?}"
        );

        /// From the byte after `fn NAME`, the text of the function's body,
        /// found by matching braces from the first `{`. Naive about braces in
        /// strings and comments, which is sound in the conservative direction:
        /// a mismatch makes the body *longer*, never shorter, and the check
        /// only ever asks whether a needle is present.
        fn body_after(src: &str, from: usize) -> &str {
            let Some(open) = src[from..].find('{').map(|i| from + i) else {
                return "";
            };
            let bytes = src.as_bytes();
            let mut depth = 0usize;
            for i in open..bytes.len() {
                match bytes[i] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            return &src[open..=i];
                        }
                    }
                    _ => {}
                }
            }
            &src[open..]
        }

        let mut checked = 0usize;
        for path in &files {
            let src = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let rel = path
                .strip_prefix(this_crate.parent().expect("crates/"))
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            for (i, _) in src.match_indices(MARKER) {
                let rest = &src[i + MARKER.len()..];
                let end = rest
                    .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                    .unwrap_or(rest.len());
                // A zero-length name is prose or a bare fn-pointer type, not a
                // definition (`fn(` in a type alias has no space before the
                // paren).
                if end == 0 {
                    continue;
                }
                let name = &rest[..end];
                let body = body_after(rest, end);
                checked += 1;
                assert!(
                    body.contains(containment),
                    "{rel}: `extern \"system\" fn {name}` does not call `{containment}` in \
                     its body, so a panic in it unwinds out of an `extern` frame and \
                     aborts the game process (0xC0000409) instead of returning a failure. \
                     Wrap the body: `contain_panic(\"{name}\", || …, || <failure value>)`, \
                     the same containment all the ntdll detours use. If it is an ntdll \
                     detour, add a row to `detour_table!` and get the wrapper for free."
                );
            }
        }
        // The detours (via `detour_table!`) and test hooks in this file's `hook_entry_points!`
        // are all one generated `$wrapper`, so the count is small on purpose: the macro,
        // `veh_handler`, `DllMain`, `vfs_shim_sync_bootstrap`.
        assert!(
            checked >= 4,
            "only {checked} `extern \"system\"` definitions were examined; the scan is \
             not finding them"
        );

        // And nothing may register a raw body as a detour: the bodies are not
        // `extern "system"`, so installing one is both an ABI error and a way
        // around the containment.
        // Every file of the hook module is scanned, not only this one.
        let hook_dir = this_crate.join("src").join("hook");
        let mut hook_files: Vec<std::path::PathBuf> = Vec::new();
        walk(&hook_dir, &mut hook_files);
        assert!(
            hook_files.len() >= 15,
            "expected the hook module's sources under {}, found {hook_files:?}",
            hook_dir.display()
        );
        for path in &hook_files {
            let src = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            assert!(
                !src.contains(concat!("_hook_body", " as *const ()")),
                "{}: a hook body was installed as a detour, bypassing its wrapper",
                path.display()
            );
        }
    }
}
