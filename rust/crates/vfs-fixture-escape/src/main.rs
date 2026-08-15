//! The escape fixture. Given a target file's path, attempts to reach the same
//! file via each of the fourteen NT/Win32 path spellings the design doc's
//! vector table enumerates, and writes one machine-readable result line per
//! attempt. The constructions that need a helper artifact (a hardlink, a
//! junction, a `subst`'d drive) create it alongside the target and clean it up
//! before exiting.
//!
//! In its default **read** mode it never mutates the target's own content. In
//! **write** mode (`VFS_ESCAPE_ACCESS=write`, below) mutating it is the whole
//! point — that mode exists to establish the other half of the containment
//! claim, and a caller runs it against canaries it is prepared to see written.
//!
//! **Output format** (tab-separated, one line per attempt, written to
//! `args[2]` if given, else stdout):
//!
//! ```text
//! <vector-id>\t<spelling-attempted>\t<outcome>\t<note>
//! ```
//!
//! `<outcome>` is one of:
//! - `opened` — **read mode only.** The spelling opened the file, **and**,
//!   whenever the target's own bytes could be read at startup (see
//!   `EXPECTED_CONTENT`), the opened handle's content matched them
//!   byte-for-byte. `opened` never means "some handle came back"; it means
//!   the same file's real bytes came back.
//! - `written` — **write mode only.** The spelling opened the file for
//!   write, this vector's own payload was written to it, and re-opening
//!   *the same spelling* read that exact payload back. Like `opened`, it is
//!   never "the call returned success": a write whose bytes cannot be read
//!   back through the same name is reported as an error, not a pass.
//! - `not-found` — the OS reported the name did not resolve to anything.
//! - `error:<detail>` — any other failure (`win32:<code>`,
//!   `ntstatus:0x########`, `cmd-exit:<code>`, `content-mismatch:<detail>` —
//!   the spelling opened *something*, but its bytes did not match the real
//!   target's, which is a worse result than `not-found`, not a pass — and, in
//!   write mode, `write-failed:win32:<code>`, `readback-open:win32:<code>`,
//!   `readback-unreadable` and `readback-mismatch:<detail>`).
//! - `unbuildable:<reason>` — this environment could not even construct the
//!   spelling (no free drive letter, 8.3 disabled, wrong filesystem for a
//!   hardlink, missing privilege for the admin share, ...). Never blank,
//!   never silently skipped.
//!
//! **`VFS_ESCAPE_ACCESS`** selects which access the whole matrix exercises:
//! `read` (the default, and byte-for-byte the behaviour this fixture had
//! before write mode existed) or `write`. Every vector builds the *same*
//! spelling either way — only the call made against it changes, which is what
//! keeps the two matrices comparable line for line.
//!
//! In write mode each vector opens its spelling with `OPEN_ALWAYS`
//! (`FILE_OPEN_IF`) — create-if-absent, never truncate — writes its own
//! fixed-length payload (`write_payload`, which encodes the vector id, so a
//! read-back proves *this* vector's write landed and not a neighbour's), and
//! then re-opens the same spelling read-only to check the payload comes back.
//! The disposition is deliberate on both halves: it creates, so a spelling
//! that escapes containment leaves a real file behind for the harness to find
//! on disk; and it preserves, so it is the shape that asks the director for a
//! copy-up rather than sidestepping the question with a truncate.
//!
//! Write mode changes one vector's *target*, not its spelling: vector 14
//! (child process) writes to a sibling name (`<target><V14_WRITE_SUFFIX>`)
//! instead of to the target itself.
//!
//! That vector has always been described as reaching real disk by
//! construction, on the grounds that the child runs unhooked. **It does not**
//! — the shim detours `CreateProcessInternalW` and injects into children, so
//! under a session the child's write is answered by the director like any
//! other (measured in gate 4 task 8; see `rust/docs/escape-matrix.md`). The
//! sibling is used anyway, because that injection is explicitly best-effort:
//! it force-suspends, injects, and gives up on a timeout. On the run where it
//! does time out, a vector 14 aimed at the target would overwrite the very
//! bytes the caller's real-filesystem assertions read, turning a scheduling
//! hiccup into a false containment failure. Aimed at a sibling it shows
//! exactly what it always showed and costs the caller nothing.
//!
//! `<vector-id>` is `1`..`14` matching the design doc's table, with two
//! expansions:
//!
//! - Vector 5 (handle-relative open) additionally emits `5b`: the same
//!   mechanism against a root handle (an anonymous pipe) that
//!   `GetFinalPathNameByHandleW` cannot resolve — the documented case where
//!   the fix falls back to the pre-existing passthrough. `5b` is a caveat
//!   report, not a second pass of vector 5, and must not be read as one.
//!   Guarded independently of vector 5, so a panic constructing 5 can never
//!   suppress the logically unrelated attempt at 5b.
//! - Vectors 10 and 12 each split into their sub-cases — `10a`/`10b`/`10c`
//!   (case fold, trailing dot, trailing space) and `12a`/`12b`/`12c` (a `.`
//!   component, a `..` traversal, a doubled separator) — because each
//!   sub-case became independently meaningful once given a `\\?\` prefix
//!   (see below), and a combined line that passes says less than three that
//!   do.
//!
//! **Why 10/10b/10c and 12a/12b/12c are built with a `\\?\` prefix.** Without
//! one, the spelling goes through kernel32's `CreateFileW` ->
//! `RtlGetFullPathName_U`, which strips trailing dots/spaces and collapses
//! `.`/`..` *before* `NtCreateFile` is ever called — the shim hooks
//! `NtCreateFile`/`NtOpenFile`, not `CreateFileW`, so an unprefixed spelling
//! here would be normalised away by Win32 itself and `opened` would be
//! guaranteed by the OS regardless of whether the shim's canonicaliser does
//! anything at all. A `\\?\`-prefixed path is verbatim: Win32 does not touch
//! it, so the divergent spelling actually reaches `NtCreateFile`. That also
//! flips the correct standalone expectation: outside a session, nothing
//! collapses `Data.` or `..` for the OS either, so the literal (nonexistent)
//! component fails to resolve and `not-found` is the *correct* result, not a
//! failure. Under a session (Task 6), the shim receives the same raw
//! spelling and its canonicaliser is what is supposed to collapse it back to
//! the real file — so `opened` there is evidence the canonicaliser works,
//! and `not-found` there would be evidence it does not. Each of these lines'
//! `<note>` says explicitly which of these two standalone/session pairings
//! applies, so a standalone `not-found` on these ids is never misread as a
//! regression. The case-fold sub-case (`10a`) is the one exception: NTFS
//! itself resolves case-insensitively regardless of `\\?\`, so `opened`
//! standalone is the correct result there too, prefix or not.
//!
//! Vectors 13 and 14 are reported, not closed, in this gate — their `<note>`
//! field says so explicitly rather than leaving a reader to infer it from
//! silence.
//!
//! Every vector's construction and attempt is wrapped in `catch_unwind`: a
//! bug or an unexpected OS response on one vector must never stop the run
//! before the rest are attempted, because a fixture that dies partway
//! through tells you nothing about the vectors after it.
//!
//! **Vector `4m`: metadata-only variant of vector 4, opt-in only, not part of
//! the fourteen-vector matrix above.** Built to prove/document a gap the
//! final whole-branch review of Gate 3 found in `docs/escape-matrix.md`'s own
//! claim of metadata containment: `qattr_hook`/`qfull_hook`/`qibn_hook`
//! (`vfs-shim/src/hook.rs`) never call `RootMap::decide` at all. They consult
//! `fuse_path_attr`, which asks `fuse_client::vpath_under_root` — which was
//! the client's own string-prefix predicate, not the canonicaliser this whole
//! matrix is about — before falling through to the real filesystem. `4m`
//! reuses vector 4's own spelling (a volume-GUID path) but calls
//! `GetFileAttributesW` instead of `CreateFileW`, so it exercises that hook
//! family directly rather than the open path vectors 1-14 already cover.
//!
//! **That gap closed in stage 2b task 5**: `fuse_client::vpath_under_root` is
//! now a `RootMap` — the same canonicaliser — so `4m` is expected to report
//! `not-found`, not `found`, under a session whose providers do not serve the
//! target. The vector is kept (and still opt-in) as the standing evidence
//! that the hook family stays sealed; it is not obsolete just because it now
//! passes for the good reason.
//!
//! Never runs as part of the default (`VFS_ESCAPE_ONLY_VECTOR` unset) matrix
//! — it is dispatched only when that variable is set to exactly `"4m"`, so it
//! cannot change any existing matrix run's line count or output. See
//! `vector4_metadata_query`'s own doc comment and
//! `crates/vfs-directord/tests/e2e.rs`'s
//! `metadata_queries_are_sealed_for_canonicaliser_only_spellings` for the
//! test that uses it.
//!
//! **Vector `enum`: directory enumeration, opt-in only, not part of the
//! fourteen-vector matrix either.** Every other vector in this file asks
//! "can this spelling reach the target?". This one asks the question no
//! vector asked: **what does a listing of the directory the target sits in
//! show?** Enumeration is a separate mechanism from the open path — it runs
//! on an already-opened handle, through `NtQueryDirectoryFile(Ex)`, and its
//! own under-root predicate is the FUSE client's rather than
//! `RootMap::decide` — so the containment of one is not the containment of
//! the other. `rust/docs/escape-matrix.md` asserted for two gates that it
//! was, by argument rather than by test; gate 4 task 8b found a live
//! real-disk drain sitting behind that argument. This vector is the
//! measurement that replaced it.
//!
//! It lists the target's parent with `std::fs::read_dir`
//! (`FindFirstFileW`/`FindNextFileW` on Windows) and reports
//! `listed:<count>` with the sorted entry names packed into its `<note>`
//! field, separated by `|` — a character no Windows filename may contain, so
//! no name can forge a boundary. That shape lets a caller assert both
//! directions of the same listing: the provider-served name must be present
//! *and* a physically-present, unserved file must be absent. Asserting only
//! the second would pass on a listing that came back empty because
//! everything broke.
//!
//! Dispatched only when `VFS_ESCAPE_ONLY_VECTOR` is exactly `"enum"`. See
//! `crates/vfs-directord/tests/e2e.rs`'s
//! `directory_enumeration_under_a_managed_root_hides_an_unserved_real_file`.
//!
//! **`VFS_ESCAPE_ONLY_VECTOR`**: when set to one of the vector ids above,
//! every *other* vector is skipped entirely — not merely omitted from the
//! output, but never constructed or attempted, so the shim's own hook-stats
//! report for the run reflects that one open alone. A caller correlating
//! this fixture's own per-line outcome against the shim's *aggregate*
//! classification counters (which are not keyed by vector) cannot otherwise
//! tell "this vector's own attempt was classified" apart from "some *other*
//! vector sharing the same target filename was classified, and this one
//! quietly was not" — precisely the ambiguity that let an earlier version
//! of a fixture in this project report two vectors closed when they had
//! silently probed nothing. Unset, every vector runs, exactly as before
//! this existed.
mod ffi;

use std::io::Write;
use std::panic::UnwindSafe;
use std::path::{Path, PathBuf};

/// One result line: which vector, what exact spelling was attempted, what
/// happened, and any explanatory note (always present for `unbuildable` and
/// for the vectors this gate only reports on; empty otherwise).
struct Line {
    vector: &'static str,
    spelling: String,
    outcome: String,
    note: String,
}

impl Line {
    fn new(
        vector: &'static str,
        spelling: impl Into<String>,
        outcome: impl Into<String>,
        note: impl Into<String>,
    ) -> Self {
        Line { vector, spelling: spelling.into(), outcome: outcome.into(), note: note.into() }
    }

    fn render(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}",
            self.vector,
            sanitize(&self.spelling),
            sanitize(&self.outcome),
            sanitize(&self.note)
        )
    }
}

/// A result line never gets to be blank or missing: a construction that
/// cannot be attempted here reports `unbuildable` with its reason, in the
/// same shape as every other outcome.
fn unbuildable(vector: &'static str, spelling: impl Into<String>, reason: impl Into<String>) -> Line {
    Line::new(vector, spelling, format!("unbuildable:{}", reason.into()), "")
}

/// Tab/newline-safe for the one-line-per-attempt contract; attempted
/// spellings and OS error text are not expected to contain these, but a
/// reader must never have to guess whether a stray newline shifted a field.
fn sanitize(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

fn last_error() -> u32 {
    // SAFETY: FFI, no arguments, no preconditions.
    unsafe { ffi::GetLastError() }
}

/// Which access this whole run exercises against every spelling — see the
/// module doc's `VFS_ESCAPE_ACCESS` section.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Read,
    Write,
}

/// Resolved once in `main` from `VFS_ESCAPE_ACCESS`. Defaults to `Read`,
/// including when the variable holds something unrecognised: this fixture's
/// whole job is to be un-skippable, so an unreadable switch must fall back to
/// the mode that was here before write mode existed rather than silently
/// exercising neither.
static ACCESS: std::sync::OnceLock<Access> = std::sync::OnceLock::new();

fn access() -> Access {
    *ACCESS.get().unwrap_or(&Access::Read)
}

/// The bytes a write-mode vector writes, and the exact bytes its read-back
/// must return.
///
/// **Fixed length on purpose.** The disposition is `OPEN_ALWAYS`, which does
/// not truncate, so a shorter payload written over a longer file leaves the
/// tail of the previous content behind and the read-back would compare
/// unequal for a reason that has nothing to do with containment. Every id
/// this fixture emits is at most four characters, so right-aligning to four
/// makes every payload exactly 22 bytes whatever the vector — and the caller
/// only has to keep its canary seeds shorter than that for the first write to
/// fully overwrite them.
///
/// The id is *in* the payload so a read-back proves this vector's own write
/// landed. Every vector writes to the same target; a payload shared across
/// vectors would let a spelling that silently wrote nothing read back a
/// neighbour's bytes and report `written`.
fn write_payload(vector: &str) -> Vec<u8> {
    format!("vfs-escape-write[{vector:>4}]").into_bytes()
}

/// Appended to the target's own path to give vector 14 a sibling to write to
/// in write mode — see the module doc for why that vector, and only that
/// vector, moves off the target. The caller matches on this exact suffix.
const V14_WRITE_SUFFIX: &str = ".v14-child-write.txt";

/// The target's own bytes, read once at startup via the plain, literal
/// spelling (see `main`), before any of the fourteen vectors run. `None`
/// when that baseline read itself failed (the "missing target" regression
/// run this fixture is also exercised against) — every later `opened`
/// result is then reported as-is, with no content check, exactly as before
/// this check existed.
///
/// A `OnceLock` rather than a parameter threaded through every vector
/// function: every vector's own construction code stays exactly as
/// reviewed and unchanged; only the two outcome-classifying functions below
/// need to know it.
static EXPECTED_CONTENT: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();

/// Read `handle`'s content back and compare it against the target's own
/// baseline bytes (see `EXPECTED_CONTENT`). `Ok(())` when there is nothing
/// to compare against (no baseline) or the content matches; `Err(detail)`
/// on a mismatch, worded for direct use inside an `error:` outcome — a
/// spelling that opens *something* but not the same bytes as the real
/// target is a worse result than `not-found`, not a pass, and must not be
/// reported as plain `opened`.
fn check_content(handle: ffi::Handle) -> Result<(), String> {
    let Some(Some(expected)) = EXPECTED_CONTENT.get() else {
        return Ok(());
    };
    match ffi::read_all(handle) {
        None => Err("could not read back the opened handle's content to verify it".to_string()),
        Some(got) if &got == expected => Ok(()),
        Some(got) => Err(format!(
            "content mismatch: opened handle returned {} bytes, expected {} bytes matching the real target",
            got.len(),
            expected.len()
        )),
    }
}

fn win32_outcome(result: Result<ffi::Handle, u32>) -> String {
    match result {
        Ok(h) => {
            let verdict = check_content(h);
            ffi::close(h);
            match verdict {
                Ok(()) => "opened".to_string(),
                Err(detail) => format!("error:content-mismatch:{detail}"),
            }
        }
        Err(code) if code == ffi::ERROR_FILE_NOT_FOUND || code == ffi::ERROR_PATH_NOT_FOUND => {
            "not-found".to_string()
        }
        Err(code) => format!("error:win32:{code}"),
    }
}

/// Outcome of a name-based attribute query (`GetFileAttributesW`), used only
/// by vector `4m` (see the module doc comment). `"found"` — deliberately
/// distinct from `"opened"` above — means the query succeeded, i.e. the OS
/// reported real attributes for this spelling, without ever opening a
/// handle. `"not-found"` and `"error:win32:<code>"` mirror `win32_outcome`'s
/// own vocabulary for the same failure shapes.
fn attr_outcome(spelling: &str) -> String {
    // SAFETY: FFI. `spelling` is encoded to a NUL-terminated UTF-16 buffer
    // immediately before the call; no other pointer is dereferenced.
    let attrs = unsafe { ffi::GetFileAttributesW(ffi::wide(spelling).as_ptr()) };
    if attrs != ffi::INVALID_FILE_ATTRIBUTES {
        "found".to_string()
    } else {
        match last_error() {
            ffi::ERROR_FILE_NOT_FOUND | ffi::ERROR_PATH_NOT_FOUND => "not-found".to_string(),
            code => format!("error:win32:{code}"),
        }
    }
}

fn nt_outcome(result: Result<ffi::Handle, ffi::NtCreateError>) -> String {
    const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
    const STATUS_OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003Au32 as i32;
    match result {
        Ok(h) => {
            let verdict = check_content(h);
            ffi::close(h);
            match verdict {
                Ok(()) => "opened".to_string(),
                Err(detail) => format!("error:content-mismatch:{detail}"),
            }
        }
        Err(ffi::NtCreateError::Unresolved) => {
            "unbuildable:ntdll!NtCreateFile export could not be resolved".to_string()
        }
        Err(ffi::NtCreateError::Status(status))
            if status == STATUS_OBJECT_NAME_NOT_FOUND || status == STATUS_OBJECT_PATH_NOT_FOUND =>
        {
            "not-found".to_string()
        }
        Err(ffi::NtCreateError::Status(status)) => format!("error:ntstatus:0x{:08X}", status as u32),
    }
}

/// **The one place a vector's spelling actually meets the OS.** Every
/// name-based vector routes its constructed spelling through here rather than
/// calling `CreateFileW` itself, so read mode and write mode differ in the
/// call made and in nothing else — the spellings, their construction, and
/// their per-vector notes stay literally the same code in both.
fn attempt(vector: &str, spelling: &str) -> String {
    match access() {
        Access::Read => win32_outcome(ffi::create_file_read(&ffi::wide(spelling))),
        Access::Write => write_outcome(vector, spelling),
    }
}

/// [`attempt`] for the two handle-relative vectors, which name their target
/// through `OBJECT_ATTRIBUTES.RootDirectory` + a relative name rather than as
/// a single string, and so go through `NtCreateFile` directly.
fn attempt_nt(vector: &str, root_directory: ffi::Handle, relative_name: &str) -> String {
    match access() {
        Access::Read => nt_outcome(ffi::nt_create_relative(root_directory, relative_name)),
        Access::Write => nt_write_outcome(vector, root_directory, relative_name),
    }
}

/// Write `vector`'s payload through `spelling`, then read it back through the
/// *same* spelling.
///
/// The read-back is what makes `written` mean something. A write open that
/// succeeds and a write that is actually durable through the name the caller
/// used are different claims, and the second is the one the positive canary
/// is for ("visible on read-back"). It is deliberately re-opened rather than
/// rewound on the same handle: a same-handle read can be answered out of
/// state the write left behind, which is precisely what a caller checking
/// visibility must not accept as evidence.
fn write_outcome(vector: &str, spelling: &str) -> String {
    let payload = write_payload(vector);
    let wide = ffi::wide(spelling);
    let handle = match ffi::create_file_write(&wide) {
        Ok(h) => h,
        Err(code) if code == ffi::ERROR_FILE_NOT_FOUND || code == ffi::ERROR_PATH_NOT_FOUND => {
            return "not-found".to_string();
        }
        Err(code) => return format!("error:win32:{code}"),
    };
    let wrote = ffi::write_all(handle, &payload);
    ffi::close(handle);
    if let Err(code) = wrote {
        return format!("error:write-failed:win32:{code}");
    }
    read_back(&wide, &payload)
}

/// [`write_outcome`] for a handle-relative open.
fn nt_write_outcome(vector: &str, root_directory: ffi::Handle, relative_name: &str) -> String {
    let payload = write_payload(vector);
    let handle = match ffi::nt_create_relative_write(root_directory, relative_name) {
        Ok(h) => h,
        Err(e) => return nt_outcome(Err(e)),
    };
    let wrote = ffi::write_all(handle, &payload);
    ffi::close(handle);
    if let Err(code) = wrote {
        return format!("error:write-failed:win32:{code}");
    }
    match ffi::nt_create_relative(root_directory, relative_name) {
        Err(e) => format!("error:readback-open:{}", nt_outcome(Err(e))),
        Ok(h) => {
            let got = ffi::read_all(h);
            ffi::close(h);
            compare_read_back(got, &payload)
        }
    }
}

/// Re-open `wide` read-only and check it hands back exactly `payload`.
fn read_back(wide: &[u16], payload: &[u8]) -> String {
    match ffi::create_file_read(wide) {
        Err(code) => format!("error:readback-open:win32:{code}"),
        Ok(h) => {
            let got = ffi::read_all(h);
            ffi::close(h);
            compare_read_back(got, payload)
        }
    }
}

fn compare_read_back(got: Option<Vec<u8>>, payload: &[u8]) -> String {
    match got {
        Some(g) if g == payload => "written".to_string(),
        Some(g) => format!(
            "error:readback-mismatch:read back {} bytes, expected the {} this vector wrote",
            g.len(),
            payload.len()
        ),
        None => "error:readback-unreadable".to_string(),
    }
}

/// `path` split into `('C', "\rest\of\path")`, or `None` if it has no drive
/// letter (e.g. it is already a UNC or device-namespace spelling).
fn split_drive(path: &str) -> Option<(char, String)> {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some((bytes[0] as char, path[2..].to_string()))
    } else {
        None
    }
}

/// `path` split at its final separator into `(parent_dir, file_name)`, or
/// `None` if there is no separator to split at.
fn parent_dir_and_filename(path: &str) -> Option<(String, String)> {
    let idx = path.rfind('\\')?;
    let (dir, rest) = path.split_at(idx);
    let name = &rest[1..];
    if name.is_empty() {
        None
    } else {
        Some((dir.to_string(), name.to_string()))
    }
}

/// The CLI argument turned into an absolute, backslash-separated path. Does
/// not resolve symlinks/reparse points or add a `\\?\` prefix — this fixture
/// builds every alternate spelling itself from a plain drive-letter form.
fn normalize_target(input: &str) -> String {
    let p = Path::new(input);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map(|cwd| cwd.join(p)).unwrap_or_else(|_| p.to_path_buf())
    };
    abs.to_string_lossy().replace('/', "\\")
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Run one vector's construction+attempt, catching a panic rather than
/// letting it end the whole run — vector 7 blowing up must never cost the
/// matrix vectors 8-14.
fn guarded(vector: &'static str, f: impl FnOnce() -> Line + UnwindSafe) -> Line {
    match std::panic::catch_unwind(f) {
        Ok(line) => line,
        Err(payload) => unbuildable(
            vector,
            "<construction panicked>",
            format!(
                "vector construction panicked and was caught so the rest of the matrix could \
                 still run: {}",
                panic_message(&payload)
            ),
        ),
    }
}

// ---------------------------------------------------------------------
// Vector 1: 8.3 short name.
// ---------------------------------------------------------------------
fn vector1_short_name(abs: &str) -> Line {
    let wide_abs = ffi::wide(abs);
    let short = ffi::grow_to_fit(|buf| {
        // SAFETY: FFI. `wide_abs` is NUL-terminated UTF-16; `buf` is valid
        // for `buf.len()` `u16`s, matching `cchBuffer`.
        unsafe { ffi::GetShortPathNameW(wide_abs.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) }
    });
    match short {
        None => unbuildable("1", abs, format!("GetShortPathNameW failed: win32:{}", last_error())),
        Some(short) if short.eq_ignore_ascii_case(abs) => unbuildable(
            "1",
            short,
            "short path equals the long path (8.3 name generation is disabled on this volume, \
             or the name already fits within 8.3)",
        ),
        Some(short) => {
            let outcome = attempt("1", &short);
            Line::new("1", short, outcome, "")
        }
    }
}

// ---------------------------------------------------------------------
// Vector 2: extended-length prefix.
// ---------------------------------------------------------------------
fn vector2_extended_length(abs: &str) -> Line {
    let spelling = format!(r"\\?\{abs}");
    let outcome = attempt("2", &spelling);
    Line::new("2", spelling, outcome, "")
}

// ---------------------------------------------------------------------
// Vector 3: raw NT device path, via the `\\?\GLOBALROOT\...` trick that maps
// straight into the object manager namespace without going through a
// `\??\` (DosDevices) symlink — the same object name an NtCreateFile caller
// bypassing kernel32 entirely would present.
// ---------------------------------------------------------------------
fn vector3_device_path(abs: &str) -> Line {
    let Some((drive, rest)) = split_drive(abs) else {
        return unbuildable("3", abs, "target path has no drive letter to resolve a device for");
    };
    match ffi::query_dos_device(drive) {
        None => unbuildable("3", abs, format!("QueryDosDeviceW({drive}:) failed: win32:{}", last_error())),
        Some(device) if !device.to_ascii_lowercase().starts_with(r"\device\") => unbuildable(
            "3",
            abs,
            format!(
                "{drive}: is not backed by a real NT device (QueryDosDeviceW returned {device:?}) \
                 -- likely a subst alias, which this vector cannot honestly exercise"
            ),
        ),
        Some(device) => {
            let spelling = format!(r"\\?\GLOBALROOT{device}{rest}");
            let outcome = attempt("3", &spelling);
            Line::new("3", spelling, outcome, "")
        }
    }
}

// ---------------------------------------------------------------------
// Vector 4: volume-GUID path.
// ---------------------------------------------------------------------
fn vector4_volume_guid(abs: &str) -> Line {
    let Some((drive, rest)) = split_drive(abs) else {
        return unbuildable("4", abs, "target path has no drive letter to resolve a volume GUID for");
    };
    match ffi::volume_guid_for_drive(drive) {
        None => unbuildable(
            "4",
            abs,
            format!("GetVolumeNameForVolumeMountPointW({drive}:\\) failed: win32:{}", last_error()),
        ),
        Some(guid) => {
            let spelling = format!("{}{}", guid.trim_end_matches('\\'), rest);
            let outcome = attempt("4", &spelling);
            Line::new("4", spelling, outcome, "")
        }
    }
}

// ---------------------------------------------------------------------
// Vector 4m: metadata-only variant of vector 4 — see the module doc comment
// ("Vector `4m`") for why this exists and why it is opt-in only. Reuses
// vector 4's own spelling construction verbatim; the only difference is the
// Win32 call made against it (`GetFileAttributesW`, not `CreateFileW`).
// ---------------------------------------------------------------------
fn vector4_metadata_query(abs: &str) -> Line {
    let Some((drive, rest)) = split_drive(abs) else {
        return unbuildable("4m", abs, "target path has no drive letter to resolve a volume GUID for");
    };
    match ffi::volume_guid_for_drive(drive) {
        None => unbuildable(
            "4m",
            abs,
            format!("GetVolumeNameForVolumeMountPointW({drive}:\\) failed: win32:{}", last_error()),
        ),
        Some(guid) => {
            let spelling = format!("{}{}", guid.trim_end_matches('\\'), rest);
            let outcome = attr_outcome(&spelling);
            Line::new(
                "4m",
                spelling,
                outcome,
                "metadata-only variant of vector 4 (GetFileAttributesW, not CreateFileW): proves \
                 whether a name-based attribute query on this spelling reaches real disk, \
                 independent of Gate 3 Task 6's open-path (RootMap::decide) fix. qattr_hook/ \
                 qfull_hook/qibn_hook still do not go through RootMap::decide — they consult \
                 fuse_client::vpath_under_root — but since stage 2b task 5 that predicate IS a \
                 RootMap, so this spelling is recognised and routed rather than falling to disk. \
                 Expect not-found under a session whose providers do not serve the target",
            )
        }
    }
}

// ---------------------------------------------------------------------
// Vector `enum`: list the target's own parent directory. Opt-in only — see
// the module doc comment ("Vector `enum`").
// ---------------------------------------------------------------------

/// Separates the entry names packed into this vector's `<note>` field.
/// Mirrored by `crates/vfs-directord/tests/e2e.rs`, which splits on it.
/// A Windows filename cannot contain `|`, so no name can forge a boundary.
const ENUM_NAME_SEP: char = '|';

fn vector_enum_listing(abs: &str) -> Line {
    let Some((dir, _name)) = parent_dir_and_filename(abs) else {
        return unbuildable("enum", abs, "target path has no parent directory to enumerate");
    };
    // `std::fs::read_dir` is `FindFirstFileW`/`FindNextFileW` on Windows,
    // which reaches `NtOpenFile` on the directory plus
    // `NtQueryDirectoryFile(Ex)` with a `*` wildcard — the two hooks
    // `serve_dir_query` sits behind. Deliberately the ordinary API a game or
    // a mod manager would use, not a hand-rolled NT call: this vector is
    // about what an unremarkable listing shows, and the two entry points'
    // agreement is already covered by `vfs-shim`'s `hook_enum_parity`.
    match std::fs::read_dir(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Line::new("enum", dir, "not-found", ENUM_NOTE)
        }
        Err(e) => Line::new(
            "enum",
            dir,
            format!("error:read-dir:{}", e.raw_os_error().unwrap_or(-1)),
            ENUM_NOTE,
        ),
        Ok(rd) => {
            let mut names: Vec<String> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            // The count is in the outcome and the names are in the note, so a
            // caller can assert presence *and* absence, and a listing that
            // came back empty is never mistaken for one that was not
            // attempted.
            Line::new("enum", dir, format!("listed:{}", names.len()), names.join(&ENUM_NAME_SEP.to_string()))
        }
    }
}

const ENUM_NOTE: &str = "directory enumeration of the target's parent (std::fs::read_dir -> \
                         FindFirstFileW); on failure there are no names to report";

// ---------------------------------------------------------------------
// Vector 5: handle-relative open via OBJECT_ATTRIBUTES.RootDirectory.
// Guarded independently of 5b (below) — the two constructions share no
// state, and a panic in one must never suppress the other.
// ---------------------------------------------------------------------
fn vector5_handle_relative(abs: &str) -> Line {
    let Some((dir, name)) = parent_dir_and_filename(abs) else {
        return unbuildable("5", abs, "target path has no parent directory component");
    };
    match ffi::create_file_read(&ffi::wide(&dir)) {
        Err(code) => unbuildable(
            "5",
            format!("{name} (relative to a handle on {dir})"),
            format!("could not open a handle on the parent directory {dir}: win32:{code}"),
        ),
        Ok(dir_handle) => {
            let spelling = format!("{name} (relative to an open handle on {dir})");
            let outcome = attempt_nt("5", dir_handle, &name);
            ffi::close(dir_handle);
            Line::new("5", spelling, outcome, "")
        }
    }
}

// ---------------------------------------------------------------------
// Vector 5b: the caveat, not a second pass of vector 5. The fix in Task 4
// asks the OS what a RootDirectory handle it never saw actually points at
// via GetFinalPathNameByHandleW; an anonymous pipe handle is a real, valid
// handle that call cannot resolve to a path. Independently buildable (needs
// only the target's filename, not vector 5's directory handle), so it is
// guarded on its own rather than sharing a closure with vector 5.
// ---------------------------------------------------------------------
fn vector5b_unresolvable_handle(abs: &str) -> Line {
    let Some((_dir, name)) = parent_dir_and_filename(abs) else {
        return unbuildable("5b", abs, "target path has no parent directory component");
    };
    let mut read_handle: ffi::Handle = std::ptr::null_mut();
    let mut write_handle: ffi::Handle = std::ptr::null_mut();
    // SAFETY: FFI. Both out-pointers are valid locals; `nSize` 0 asks for
    // the system default buffer size.
    let created = unsafe { ffi::CreatePipe(&mut read_handle, &mut write_handle, std::ptr::null_mut(), 0) };
    if created == 0 {
        return unbuildable(
            "5b",
            "<relative to an anonymous pipe handle>",
            format!("CreatePipe failed: win32:{}", last_error()),
        );
    }
    let spelling = format!("{name} (relative to an anonymous pipe handle, not a directory)");
    let outcome = attempt_nt("5b", read_handle, &name);
    ffi::close(read_handle);
    ffi::close(write_handle);
    Line::new(
        "5b",
        spelling,
        outcome,
        "caveat, not a pass: a handle-relative open only classifies correctly when \
         GetFinalPathNameByHandleW can resolve the root handle. A pipe cannot be resolved \
         that way, so this line exercises the fallback edge Task 4 left open (falls back to \
         the pre-existing passthrough), and must not be read as vector 5 succeeding twice.",
    )
}

// ---------------------------------------------------------------------
// Vector 6: CWD-relative open.
// ---------------------------------------------------------------------
fn vector6_cwd_relative(abs: &str) -> Line {
    let Some((dir, name)) = parent_dir_and_filename(abs) else {
        return unbuildable("6", abs, "target path has no parent directory component");
    };
    let original_cwd = std::env::current_dir().ok();
    if let Err(e) = std::env::set_current_dir(&dir) {
        return unbuildable("6", &name, format!("could not chdir to {dir}: {e}"));
    }
    let outcome = attempt("6", &name);
    if let Some(cwd) = original_cwd {
        let _ = std::env::set_current_dir(cwd);
    }
    Line::new("6", name, outcome, "")
}

// ---------------------------------------------------------------------
// Vector 7: junction / reparse point. `mklink /J` needs no elevation.
// ---------------------------------------------------------------------
fn vector7_junction(abs: &str) -> Line {
    let Some((dir, name)) = parent_dir_and_filename(abs) else {
        return unbuildable("7", abs, "target path has no parent directory component");
    };

    // `VFS_ESCAPE_VECTOR7_LINK_DIR`: a junction the *caller* already created
    // before this process was even launched — see that name's own doc
    // comment in `vfs-env` for the full reasoning. In short: this
    // function's own `mklink /J` spawn below is itself real, hooked file
    // activity inside an injected process, and `vfs-redirect`'s
    // volume/junction alias table is resolved once, on the session's first
    // such activity — if that spawn (needed to bring the junction into
    // existence) is itself what triggers that first resolution, the table
    // gets built *before* the junction exists and never sees it. A
    // pre-existing junction removes the ordering question entirely, and is
    // exactly what a real mod manager's own junction looks like (already
    // there before the game process starts). Used only under a session;
    // unset for a standalone reproduction, which falls back to
    // self-construction below exactly as before.
    if let Ok(link_dir_str) = std::env::var("VFS_ESCAPE_VECTOR7_LINK_DIR") {
        let spelling = format!(r"{link_dir_str}\{name}");
        let outcome = attempt("7", &spelling);
        return Line::new("7", spelling, outcome, "pre-existing junction supplied by the caller");
    }

    let link_dir = std::env::temp_dir().join(format!("vfs-escape-junction-{}", std::process::id()));
    if link_dir.exists() {
        let _ = std::fs::remove_dir(&link_dir);
    }
    let link_dir_str = link_dir.to_string_lossy().into_owned();
    let mklink = std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&link_dir_str)
        .arg(&dir)
        .output();
    match mklink {
        Ok(out) if out.status.success() => {
            let spelling = format!(r"{link_dir_str}\{name}");
            let outcome = attempt("7", &spelling);
            let _ = std::fs::remove_dir(&link_dir);
            Line::new("7", spelling, outcome, "")
        }
        Ok(out) => {
            let mut detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.trim().is_empty() {
                detail.push_str(" / stderr: ");
                detail.push_str(stderr.trim());
            }
            unbuildable("7", format!(r"{link_dir_str}\{name}"), format!("mklink /J failed: {detail}"))
        }
        Err(e) => {
            unbuildable("7", format!(r"{link_dir_str}\{name}"), format!("could not spawn cmd for mklink: {e}"))
        }
    }
}

// ---------------------------------------------------------------------
// Vector 8: hardlink. Placed as a sibling of the target so it is always on
// the same volume — hardlinks cannot cross volumes, so anywhere else would
// make this vector spuriously unbuildable.
// ---------------------------------------------------------------------
fn vector8_hardlink(abs: &str) -> Line {
    let target = PathBuf::from(abs);
    let Some(parent) = target.parent() else {
        return unbuildable("8", abs, "target path has no parent directory");
    };
    let link_path = parent.join(format!(".vfs-escape-hardlink-{}", std::process::id()));
    let link_str = link_path.to_string_lossy().into_owned();
    match std::fs::hard_link(&target, &link_path) {
        Ok(()) => {
            let outcome = attempt("8", &link_str);
            let _ = std::fs::remove_file(&link_path);
            Line::new("8", link_str, outcome, "")
        }
        Err(e) => unbuildable("8", link_str, format!("std::fs::hard_link failed: {e}")),
    }
}

// ---------------------------------------------------------------------
// Vector 9: UNC / subst / mapped drive. Tries the administrative UNC share
// first (`\\localhost\<drive>$\...`, no setup step, may need privileges);
// falls back to `subst` on a free drive letter if that construction itself
// fails. `subst` and a network-mapped drive are the same underlying
// mechanism (a DOS-device alias) as far as this fixture is concerned, so
// only one local-alias fallback is exercised.
// ---------------------------------------------------------------------
fn vector9_alias_drive(abs: &str) -> Line {
    let Some((drive, rest)) = split_drive(abs) else {
        return unbuildable("9", abs, "target path has no drive letter");
    };
    let unc = format!(r"\\localhost\{drive}${rest}");
    let unc_outcome = attempt("9", &unc);
    // The subst fallback exists for a UNC *construction* that this environment
    // will not permit at all (no admin share, no privilege) — not for a share
    // that resolved and answered. So it fires only on a bare `error:win32:`,
    // which is what an unusable construction reports; `not-found`, a
    // content mismatch, and every write-mode outcome all mean the spelling
    // reached the OS and was answered, which is a result to report rather than
    // a reason to try a different construction.
    if !unc_outcome.starts_with("error:win32:") {
        return Line::new(
            "9",
            unc,
            unc_outcome,
            "constructed via the administrative UNC share (\\\\localhost\\<drive>$)",
        );
    }
    let unc_err = unc_outcome;
    let Some((dir, name)) = parent_dir_and_filename(abs) else {
        return unbuildable(
            "9",
            unc,
            format!(
                "the administrative UNC share attempt failed ({unc_err}) and the target has no \
                 parent directory for a subst fallback"
            ),
        );
    };
    match free_drive_letter() {
        None => unbuildable(
            "9",
            unc,
            format!(
                "the administrative UNC share attempt failed ({unc_err}) and no free drive \
                 letter is available for a subst fallback"
            ),
        ),
        Some(letter) => {
            let subst = std::process::Command::new("subst")
                .arg(format!("{letter}:"))
                .arg(&dir)
                .output();
            match subst {
                Ok(out) if out.status.success() => {
                    let spelling = format!("{letter}:\\{name}");
                    let outcome = attempt("9", &spelling);
                    let _ = std::process::Command::new("subst")
                        .arg(format!("{letter}:"))
                        .arg("/D")
                        .output();
                    Line::new(
                        "9",
                        spelling,
                        outcome,
                        format!(
                            "the administrative UNC share attempt failed ({unc_err}); fell back \
                             to `subst {letter}: {dir}`"
                        ),
                    )
                }
                Ok(out) => {
                    let detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    unbuildable(
                        "9",
                        unc,
                        format!(
                            "the administrative UNC share attempt failed ({unc_err}) and `subst \
                             {letter}: {dir}` also failed: {detail}"
                        ),
                    )
                }
                Err(e) => unbuildable(
                    "9",
                    unc,
                    format!(
                        "the administrative UNC share attempt failed ({unc_err}) and could not \
                         spawn subst: {e}"
                    ),
                ),
            }
        }
    }
}

fn free_drive_letter() -> Option<char> {
    // SAFETY: FFI, no arguments, no preconditions.
    let mask = unsafe { ffi::GetLogicalDrives() };
    (b'D'..=b'Z').rev().map(|b| b as char).find(|c| mask & (1u32 << (*c as u32 - 'A' as u32)) == 0)
}

fn case_flip(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else if c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c
            }
        })
        .collect()
}

// ---------------------------------------------------------------------
// Vector 10a: alternate Unicode case form. Built with a `\\?\` prefix for
// consistency with 10b/10c, though it does not depend on it: NTFS resolves
// names case-insensitively at the filesystem-driver level regardless of
// whether Win32's own path parsing ran, so `opened` is the correct result
// standalone, with or without a session.
// ---------------------------------------------------------------------
fn vector10a_case_fold(abs: &str) -> Line {
    let flipped = case_flip(abs);
    if flipped == abs {
        return unbuildable("10a", abs, "target path has no alphabetic characters to case-flip");
    }
    let spelling = format!(r"\\?\{flipped}");
    let outcome = attempt("10a", &spelling);
    Line::new(
        "10a",
        spelling,
        outcome,
        "case-flipped from the given spelling; NTFS resolves names case-insensitively \
         regardless of the \\\\?\\ prefix, so `opened` is expected standalone, session or not",
    )
}

// ---------------------------------------------------------------------
// Vector 10b: a trailing dot Win32 silently discards -- but only when it
// gets the chance to. Built with a `\\?\` prefix so the raw spelling reaches
// NtCreateFile unmodified: without one, kernel32's RtlGetFullPathName_U
// strips the trailing dot before the shim's hook ever sees it, and `opened`
// would be the OS's doing, not the canonicaliser's. Standalone (no session),
// nothing strips it either, so the OS looks for a component that literally
// ends in '.', which does not exist -- `not-found` is the correct result
// here, not a failure. Under a session with a working canonicaliser this
// should flip to `opened`; under one that does not strip trailing dots it
// would stay `not-found`.
// ---------------------------------------------------------------------
fn vector10b_trailing_dot(abs: &str) -> Line {
    let spelling = format!(r"\\?\{abs}.");
    let outcome = attempt("10b", &spelling);
    Line::new(
        "10b",
        spelling,
        outcome,
        "verbatim (\\\\?\\) path with a trailing '.' appended to the whole spelling; in read mode \
         `not-found` is the correct standalone result (Win32 never got a chance to strip it, and \
         no such literal name exists) -- a working canonicaliser under a session should flip this \
         to `opened`, not the other way around. In write mode the creating disposition means a \
         standalone run *creates* that literal name instead, which is precisely the escape a \
         session must not permit: under a session this must reach the canary's own vpath and \
         leave no such file on real disk",
    )
}

// ---------------------------------------------------------------------
// Vector 10c: a trailing space, same reasoning as 10b.
// ---------------------------------------------------------------------
fn vector10c_trailing_space(abs: &str) -> Line {
    let spelling = format!(r"\\?\{abs} ");
    let outcome = attempt("10c", &spelling);
    Line::new(
        "10c",
        spelling,
        outcome,
        "verbatim (\\\\?\\) path with a trailing space appended to the whole spelling; \
         `not-found` is the correct standalone read result for the same reason as 10b -- a \
         working canonicaliser under a session should flip this to `opened`. Write mode creates \
         the literal name standalone, same as 10b, and must not under a session",
    )
}

// ---------------------------------------------------------------------
// Vector 11: alternate data stream suffix. Read-only, OPEN_EXISTING — never
// creates the stream, so `not-found` here means only that this particular
// stream name is not already present on the target, not that ADS is
// unsupported.
// ---------------------------------------------------------------------
fn vector11_ads(abs: &str) -> Line {
    let spelling = format!("{abs}:vfs-escape-fixture-probe");
    let outcome = attempt("11", &spelling);
    Line::new(
        "11",
        spelling,
        outcome,
        "read-only OPEN_EXISTING against a stream that is not pre-created by this fixture; \
         not-found here means the stream doesn't already exist, not that streams are unsupported",
    )
}

// ---------------------------------------------------------------------
// Vector 12a/12b/12c: '.'/'..' components and a redundant separator, each
// its own spelling, each `\\?\`-prefixed for the same reason as 10b/10c:
// without the prefix, kernel32's RtlGetFullPathName_U collapses `.`/`..`
// lexically before NtCreateFile is ever reached, so the shim's own
// canonicaliser (which is supposed to do that collapsing) is never
// exercised and `opened` would be guaranteed by Win32 regardless of whether
// canonicalisation works. With the prefix, "." and ".." are literal,
// unresolved component names as far as Win32 is concerned (documented
// behaviour of the `\\?\` prefix), and NTFS stores no such directory
// entries, so `not-found` is the correct standalone result for 12a/12b, not
// a failure -- a working canonicaliser under a session should flip both to
// `opened`.
// ---------------------------------------------------------------------
fn vector12a_dot_component(abs: &str) -> Line {
    let Some((dir, name)) = parent_dir_and_filename(abs) else {
        return unbuildable("12a", abs, "target path has no parent directory component");
    };
    let spelling = format!(r"\\?\{dir}\.\{name}");
    let outcome = attempt("12a", &spelling);
    Line::new(
        "12a",
        spelling,
        outcome,
        "verbatim (\\\\?\\) path with a literal '.' component; `not-found` is the correct \
         standalone result (NTFS has no directory entry literally named '.') -- a working \
         canonicaliser under a session should flip this to `opened`",
    )
}

fn vector12b_dotdot_traversal(abs: &str) -> Line {
    let Some((dir, name)) = parent_dir_and_filename(abs) else {
        return unbuildable("12b", abs, "target path has no parent directory component");
    };
    let spelling = format!(r"\\?\{dir}\zz-vfs-escape-nonexistent-marker\..\{name}");
    let outcome = attempt("12b", &spelling);
    Line::new(
        "12b",
        spelling,
        outcome,
        "verbatim (\\\\?\\) path traversing '..' through a directory name that does not exist; \
         `not-found` is the correct standalone result -- the intermediate name is never really \
         walked, and even if it existed Win32 does not resolve '..' in a verbatim path. A \
         working canonicaliser under a session collapses this lexically (the intermediate name \
         need not exist for that) and should flip this to `opened`",
    )
}

fn vector12c_doubled_separator(abs: &str) -> Line {
    let Some((dir, name)) = parent_dir_and_filename(abs) else {
        return unbuildable("12c", abs, "target path has no parent directory component");
    };
    let spelling = format!(r"\\?\{dir}\\{name}");
    let outcome = attempt("12c", &spelling);
    Line::new(
        "12c",
        spelling,
        outcome,
        "verbatim (\\\\?\\) path with a doubled/redundant separator before the file name; \
         unlike 12a/12b this does not depend on '.'/'..' resolution, so the standalone result \
         here is whatever the NT object-manager namespace parser itself does with a doubled \
         separator -- reported as observed, not asserted in advance",
    )
}

// ---------------------------------------------------------------------
// Vector 13: a handle opened before the root registered. Reported, not
// closed, in this gate (gate 3's job). This standalone run has no
// session/root at all, so it cannot construct the actual timing — it only
// shows the ordinary reachability the real scenario builds on.
// ---------------------------------------------------------------------
fn vector13_preexisting_handle(abs: &str) -> Line {
    let outcome = attempt("13", abs);
    Line::new(
        "13",
        abs,
        outcome,
        "reported, not closed in this gate: a handle opened before the managed root was \
         registered bypasses canonicalisation entirely, because no further open call occurs for \
         the shim to intercept. Closing that is gate 3's job. This standalone run has no \
         session/root to open the handle 'before', so this line only shows ordinary \
         reachability -- the substrate the real scenario builds on, not a reproduction of the \
         timing itself; that needs Task 6's session-based harness.",
    )
}

// ---------------------------------------------------------------------
// Vector 14: a child process, spawned on the assumption that it runs without
// the shim. It does not — the shim detours CreateProcessInternalW and injects
// into children, so the child is hooked and the director answers it (measured
// in gate 4 task 8; see rust/docs/escape-matrix.md). Still reported, not
// closed: that inject is best-effort, so neither outcome here is assertable.
// ---------------------------------------------------------------------
fn vector14_child_without_shim(abs: &str) -> Line {
    match access() {
        Access::Read => {
            let note = "reported, not closed in this gate: this vector spawns a child process, \
                        on the assumption that it runs without the shim. MEASURED OTHERWISE in \
                        gate 4 task 8: the shim hooks CreateProcessInternalW and injects its own \
                        DLL into children, so under a session this child IS injected and its \
                        read is answered by the director like any other -- under the negative \
                        canary it reports error:cmd-exit:1, i.e. the real bytes were NOT \
                        reachable. Still not asserted, because that injection is best-effort \
                        (force-suspend, inject, give up on timeout), so a pass here would be a \
                        pass about scheduling. See rust/docs/escape-matrix.md, 'Gate 4, Task 8'.";
            let spelling = format!("cmd /C type {abs}");
            match std::process::Command::new("cmd").arg("/C").arg("type").arg(abs).output() {
                Ok(out) if out.status.success() => Line::new("14", spelling, "opened", note),
                Ok(out) => {
                    let code = out.status.code().unwrap_or(-1);
                    Line::new("14", spelling, format!("error:cmd-exit:{code}"), note)
                }
                Err(e) => {
                    Line::new("14", spelling, format!("unbuildable:could not spawn cmd: {e}"), note)
                }
            }
        }
        // A **sibling** of the target, not the target — see the module doc's
        // `VFS_ESCAPE_ACCESS` section. Not because this vector reaches real
        // disk by construction (it does not: the shim injects into children,
        // measured in gate 4 task 8), but because that injection is
        // best-effort. On the run where it times out, a vector 14 aimed at
        // the target would overwrite the very bytes the caller's containment
        // assertions read, turning a scheduling hiccup into a false
        // containment failure.
        Access::Write => {
            let note = "reported, not closed in this gate: this vector spawns a child process, \
                        on the assumption that it runs without the shim. MEASURED OTHERWISE in \
                        gate 4 task 8: the shim hooks CreateProcessInternalW and injects into \
                        children, so under a session this child's write is answered by the \
                        director too -- `written` against the positive canary (the bytes land in \
                        the provider store), error:cmd-exit:1 against the negative one. Not \
                        asserted, because that injection is best-effort. Targets a SIBLING of \
                        the canary (<target>.v14-child-write.txt) rather than the canary itself, \
                        so that on the one run where the inject does time out, the canary's own \
                        bytes stay a clean signal for the caller's real-filesystem assertions.";
            let sibling = format!("{abs}{V14_WRITE_SUFFIX}");
            let spelling = format!("cmd /C echo v14-child-write>{sibling}");
            // `raw_arg`, not `arg`: the payload is a *shell* command line
            // whose `>` is the redirection that does the writing. Rust's
            // ordinary argument quoting escapes the embedded quotes, which
            // cmd then reads as literal text and the whole thing fails with
            // exit 1 (found by running this standalone, which is why the
            // distinction is recorded here rather than left to be
            // rediscovered).
            use std::os::windows::process::CommandExt;
            let command = format!("echo v14-child-write>\"{sibling}\"");
            match std::process::Command::new("cmd").arg("/C").raw_arg(&command).output() {
                Ok(out) if out.status.success() => Line::new("14", spelling, "written", note),
                Ok(out) => {
                    let code = out.status.code().unwrap_or(-1);
                    Line::new("14", spelling, format!("error:cmd-exit:{code}"), note)
                }
                Err(e) => {
                    Line::new("14", spelling, format!("unbuildable:could not spawn cmd: {e}"), note)
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(target) = args.get(1) else {
        eprintln!("usage: vfs-fixture-escape <target-file-path> [output-file]");
        std::process::exit(1);
    };
    let abs = normalize_target(target);

    // See the module doc's `VFS_ESCAPE_ACCESS` section. Resolved before any
    // vector runs, so every vector in one run exercises the same access.
    let requested = std::env::var("VFS_ESCAPE_ACCESS").unwrap_or_default();
    let _ = ACCESS.set(if requested.eq_ignore_ascii_case("write") {
        Access::Write
    } else {
        if !requested.is_empty() && !requested.eq_ignore_ascii_case("read") {
            eprintln!(
                "vfs-fixture-escape: warning: VFS_ESCAPE_ACCESS={requested:?} is not `read` or \
                 `write`; running the read matrix"
            );
        }
        Access::Read
    });

    // See the module doc for `VFS_ESCAPE_ONLY_VECTOR`: when set, every other
    // vector below is skipped entirely (never constructed, never attempted)
    // rather than merely left out of the output, so a caller correlating
    // against the shim's own (not vector-keyed) hook-stats report sees
    // exactly one attempt's effect on it.
    let only_vector = std::env::var("VFS_ESCAPE_ONLY_VECTOR").ok();
    let wanted = |id: &str| match &only_vector {
        Some(o) => o == id,
        None => true,
    };

    // Existence check, purely informational (the warning below) — but
    // `std::fs::metadata` on Windows opens a real handle
    // (`CreateFileW`+`GetFileInformationByHandle`, not a lighter
    // attributes-only query), so under a session this is itself a real,
    // hooked, classifiable open of the *bare* target spelling. Skipped in
    // isolation mode for the same reason the baseline read below is: it
    // would contaminate the one signal isolation mode exists to produce,
    // making every isolated vector's classified-paths set contain this
    // unrelated bare-path entry regardless of that vector's own behaviour
    // — found by reproduction (an isolated run for a vector whose own
    // construction never touches the bare path at all still showed one
    // classified entry for it).
    if only_vector.is_none() && std::fs::metadata(&abs).is_err() {
        eprintln!(
            "vfs-fixture-escape: warning: target path {abs} does not currently exist; several \
             vectors will legitimately report not-found or unbuildable rather than opened"
        );
    }

    // Baseline: the target's own bytes, read via the plain literal spelling,
    // before any of the fourteen vectors run. Every vector that later
    // reports `opened` is checked against this, not merely trusted, so a
    // spelling that opens *something* other than the real target (wrong
    // file, stale/zero-length synthetic handle, ...) reports a mismatch
    // instead of a false `opened` — see `check_content`. `None` when the
    // target cannot be read at all here (the "missing target" regression
    // run this fixture is also exercised against), which turns the check
    // into a no-op rather than a spurious failure.
    //
    // Skipped entirely in `VFS_ESCAPE_ONLY_VECTOR` isolation mode: this open
    // is itself an ordinary, unmangled spelling of the target, so it would
    // land in the shim's classified-paths set exactly like the vector under
    // test — contaminating the one signal isolation mode exists to produce
    // (that a *specific* vector's own attempt, and nothing else, explains
    // whatever shows up classified). Content verification is not needed for
    // an isolated classification-only run either, so there is nothing this
    // skip costs a caller using isolation for that purpose.
    if only_vector.is_none() {
        let baseline = ffi::create_file_read(&ffi::wide(&abs)).ok().and_then(|h| {
            let data = ffi::read_all(h);
            ffi::close(h);
            data
        });
        let _ = EXPECTED_CONTENT.set(baseline);
    }

    let mut lines: Vec<Line> = Vec::new();
    if wanted("1") {
        lines.push(guarded("1", || vector1_short_name(&abs)));
    }
    if wanted("2") {
        lines.push(guarded("2", || vector2_extended_length(&abs)));
    }
    if wanted("3") {
        lines.push(guarded("3", || vector3_device_path(&abs)));
    }
    if wanted("4") {
        lines.push(guarded("4", || vector4_volume_guid(&abs)));
    }
    // Opt-in only, deliberately NOT gated by `wanted()`: `4m` must never run
    // as part of the default (`VFS_ESCAPE_ONLY_VECTOR` unset) matrix, so it
    // can never change an existing run's line count or output. See the
    // module doc comment ("Vector `4m`") for why it exists.
    if only_vector.as_deref() == Some("4m") {
        lines.push(guarded("4m", || vector4_metadata_query(&abs)));
    }
    // Opt-in only for the same reason `4m` is, and gated the same way: it
    // reports a *listing*, not an open outcome, so it belongs to neither
    // expectation table and must never change an existing matrix run's line
    // count. See the module doc comment ("Vector `enum`").
    if only_vector.as_deref() == Some("enum") {
        lines.push(guarded("enum", || vector_enum_listing(&abs)));
    }
    if wanted("5") {
        lines.push(guarded("5", || vector5_handle_relative(&abs)));
    }
    if wanted("5b") {
        lines.push(guarded("5b", || vector5b_unresolvable_handle(&abs)));
    }
    if wanted("6") {
        lines.push(guarded("6", || vector6_cwd_relative(&abs)));
    }
    if wanted("7") {
        lines.push(guarded("7", || vector7_junction(&abs)));
    }
    if wanted("8") {
        lines.push(guarded("8", || vector8_hardlink(&abs)));
    }
    if wanted("9") {
        lines.push(guarded("9", || vector9_alias_drive(&abs)));
    }
    if wanted("10a") {
        lines.push(guarded("10a", || vector10a_case_fold(&abs)));
    }
    if wanted("10b") {
        lines.push(guarded("10b", || vector10b_trailing_dot(&abs)));
    }
    if wanted("10c") {
        lines.push(guarded("10c", || vector10c_trailing_space(&abs)));
    }
    if wanted("11") {
        lines.push(guarded("11", || vector11_ads(&abs)));
    }
    if wanted("12a") {
        lines.push(guarded("12a", || vector12a_dot_component(&abs)));
    }
    if wanted("12b") {
        lines.push(guarded("12b", || vector12b_dotdot_traversal(&abs)));
    }
    if wanted("12c") {
        lines.push(guarded("12c", || vector12c_doubled_separator(&abs)));
    }
    if wanted("13") {
        lines.push(guarded("13", || vector13_preexisting_handle(&abs)));
    }
    if wanted("14") {
        lines.push(guarded("14", || vector14_child_without_shim(&abs)));
    }

    let mut out: Box<dyn Write> = match args.get(2) {
        Some(path) => match std::fs::File::create(path) {
            Ok(f) => Box::new(f),
            Err(e) => {
                eprintln!("vfs-fixture-escape: cannot create output file {path}: {e}");
                std::process::exit(1);
            }
        },
        None => Box::new(std::io::stdout()),
    };
    for line in &lines {
        let _ = writeln!(out, "{}", line.render());
    }
    let _ = out.flush();

    // Same reasoning as `vfs-fixture-writepath`'s own end-of-run sleep: the
    // shim's hook-stats reporter is a periodic sample (`VFS_SHIM_STATS_LOG`),
    // not an exit dump, and nothing flushes it when this process exits. This
    // fixture's own last few vectors (13, 14) can otherwise land inside the
    // same handful of milliseconds this whole run takes, well within one
    // tick's window — a caller reading the report for evidence that every
    // vector's open was classified must not see a report that simply never
    // ticked again after them. Derived from the same interval the caller may
    // have configured (`VFS_SHIM_STATS_INTERVAL_MS`) rather than a fixed
    // number, so this stays correct if that interval ever changes; a no-op
    // when stats logging is off (nothing to outlive).
    let interval_ms: u64 = std::env::var("VFS_SHIM_STATS_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(250);
    // Floored at 20ms regardless of how small `interval_ms` is: Windows'
    // default system timer resolution is coarser than either fast-tick
    // interval this project configures (5ms, or 1ms) — a thread that calls
    // `Sleep(N)` for `N` under roughly 15.6ms is not guaranteed to actually
    // wake up anywhere near `N`, only "no earlier than `N`, next tick or
    // later", so `interval_ms * 2` alone (10ms for a 5ms interval) is not
    // reliably enough margin to guarantee even one reporter tick landed —
    // found by reproduction during the vectors-7/9 closeout: an isolated
    // single-vector run occasionally exited before any tick fired, an
    // intermittent classification miss unrelated to that closeout's own
    // canonicalisation logic. 20ms comfortably clears the default ~15.6ms
    // granularity with margin, for either configured interval.
    let wait = std::time::Duration::from_millis(interval_ms.saturating_mul(2))
        .max(std::time::Duration::from_millis(20));
    std::thread::sleep(wait);

    std::process::exit(0);
}
