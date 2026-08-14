//! The escape fixture. Given a target file's path, attempts to open the same
//! file via each of the fourteen NT/Win32 path spellings the design doc's
//! vector table enumerates, and writes one machine-readable result line per
//! attempt. Never mutates the target file's own content — the constructions
//! that need a helper artifact (a hardlink, a junction, a `subst`'d drive)
//! create it alongside the target and clean it up before exiting.
//!
//! **Output format** (tab-separated, one line per attempt, written to
//! `args[2]` if given, else stdout):
//!
//! ```text
//! <vector-id>\t<spelling-attempted>\t<outcome>\t<note>
//! ```
//!
//! `<outcome>` is one of:
//! - `opened` — the spelling opened the file.
//! - `not-found` — the OS reported the name did not resolve to anything.
//! - `error:<detail>` — any other failure (`win32:<code>`,
//!   `ntstatus:0x########`, `cmd-exit:<code>`).
//! - `unbuildable:<reason>` — this environment could not even construct the
//!   spelling (no free drive letter, 8.3 disabled, wrong filesystem for a
//!   hardlink, missing privilege for the admin share, ...). Never blank,
//!   never silently skipped.
//!
//! `<vector-id>` is `1`..`14` matching the design doc's table, except vector
//! 5 (handle-relative open) additionally emits `5b`: the same mechanism
//! against a root handle (an anonymous pipe) that `GetFinalPathNameByHandleW`
//! cannot resolve — the documented case where the fix falls back to the
//! pre-existing passthrough. `5b` is a caveat report, not a second pass of
//! vector 5, and must not be read as one.
//!
//! Vectors 13 and 14 are reported, not closed, in this gate — their `<note>`
//! field says so explicitly rather than leaving a reader to infer it from
//! silence.
//!
//! Every vector's construction and attempt is wrapped in `catch_unwind`: a
//! bug or an unexpected OS response on one vector must never stop the run
//! before the rest are attempted, because a fixture that dies partway
//! through tells you nothing about the vectors after it.
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

fn win32_outcome(result: Result<ffi::Handle, u32>) -> String {
    match result {
        Ok(h) => {
            ffi::close(h);
            "opened".to_string()
        }
        Err(code) if code == ffi::ERROR_FILE_NOT_FOUND || code == ffi::ERROR_PATH_NOT_FOUND => {
            "not-found".to_string()
        }
        Err(code) => format!("error:win32:{code}"),
    }
}

fn nt_outcome(result: Result<ffi::Handle, ffi::NtCreateError>) -> String {
    const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
    const STATUS_OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003Au32 as i32;
    match result {
        Ok(h) => {
            ffi::close(h);
            "opened".to_string()
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

fn guarded_pair(
    primary: &'static str,
    secondary: &'static str,
    f: impl FnOnce() -> (Line, Line) + UnwindSafe,
) -> (Line, Line) {
    match std::panic::catch_unwind(f) {
        Ok(pair) => pair,
        Err(payload) => {
            let msg = panic_message(&payload);
            (
                unbuildable(
                    primary,
                    "<construction panicked>",
                    format!("vector construction panicked and was caught: {msg}"),
                ),
                unbuildable(
                    secondary,
                    "<construction panicked>",
                    format!("skipped: {primary}'s construction panicked before reaching it: {msg}"),
                ),
            )
        }
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
            let outcome = win32_outcome(ffi::create_file_read(&ffi::wide(&short)));
            Line::new("1", short, outcome, "")
        }
    }
}

// ---------------------------------------------------------------------
// Vector 2: extended-length prefix.
// ---------------------------------------------------------------------
fn vector2_extended_length(abs: &str) -> Line {
    let spelling = format!(r"\\?\{abs}");
    let outcome = win32_outcome(ffi::create_file_read(&ffi::wide(&spelling)));
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
            let outcome = win32_outcome(ffi::create_file_read(&ffi::wide(&spelling)));
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
            let outcome = win32_outcome(ffi::create_file_read(&ffi::wide(&spelling)));
            Line::new("4", spelling, outcome, "")
        }
    }
}

// ---------------------------------------------------------------------
// Vector 5 (+5b): handle-relative open via OBJECT_ATTRIBUTES.RootDirectory.
// ---------------------------------------------------------------------
fn vector5_handle_relative(abs: &str) -> (Line, Line) {
    let Some((dir, name)) = parent_dir_and_filename(abs) else {
        let reason = "target path has no parent directory component";
        return (unbuildable("5", abs, reason), unbuildable("5b", abs, reason));
    };

    let line5 = match ffi::create_file_read(&ffi::wide(&dir)) {
        Err(code) => unbuildable(
            "5",
            format!("{name} (relative to a handle on {dir})"),
            format!("could not open a handle on the parent directory {dir}: win32:{code}"),
        ),
        Ok(dir_handle) => {
            let spelling = format!("{name} (relative to an open handle on {dir})");
            let outcome = nt_outcome(ffi::nt_create_relative(dir_handle, &name));
            ffi::close(dir_handle);
            Line::new("5", spelling, outcome, "")
        }
    };

    // 5b: the caveat. The fix in Task 4 asks the OS what a RootDirectory
    // handle it never saw actually points at via GetFinalPathNameByHandleW;
    // an anonymous pipe handle is a real, valid handle that call cannot
    // resolve to a path. This line demonstrates that edge, not a second
    // pass of vector 5.
    let mut read_handle: ffi::Handle = std::ptr::null_mut();
    let mut write_handle: ffi::Handle = std::ptr::null_mut();
    // SAFETY: FFI. Both out-pointers are valid locals; `nSize` 0 asks for
    // the system default buffer size.
    let created = unsafe { ffi::CreatePipe(&mut read_handle, &mut write_handle, std::ptr::null_mut(), 0) };
    let line5b = if created == 0 {
        unbuildable(
            "5b",
            "<relative to an anonymous pipe handle>",
            format!("CreatePipe failed: win32:{}", last_error()),
        )
    } else {
        let spelling = format!("{name} (relative to an anonymous pipe handle, not a directory)");
        let outcome = nt_outcome(ffi::nt_create_relative(read_handle, &name));
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
    };

    (line5, line5b)
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
    let outcome = win32_outcome(ffi::create_file_read(&ffi::wide(&name)));
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
            let outcome = win32_outcome(ffi::create_file_read(&ffi::wide(&spelling)));
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
            let outcome = win32_outcome(ffi::create_file_read(&ffi::wide(&link_str)));
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
    match ffi::create_file_read(&ffi::wide(&unc)) {
        Ok(h) => {
            ffi::close(h);
            Line::new("9", unc, "opened", "constructed via the administrative UNC share (\\\\localhost\\<drive>$)")
        }
        Err(code) if code == ffi::ERROR_FILE_NOT_FOUND || code == ffi::ERROR_PATH_NOT_FOUND => Line::new(
            "9",
            unc,
            "not-found",
            "constructed via the administrative UNC share (\\\\localhost\\<drive>$)",
        ),
        Err(unc_err) => {
            let Some((dir, name)) = parent_dir_and_filename(abs) else {
                return unbuildable(
                    "9",
                    unc,
                    format!(
                        "the administrative UNC share attempt failed (win32:{unc_err}) and the \
                         target has no parent directory for a subst fallback"
                    ),
                );
            };
            match free_drive_letter() {
                None => unbuildable(
                    "9",
                    unc,
                    format!(
                        "the administrative UNC share attempt failed (win32:{unc_err}) and no \
                         free drive letter is available for a subst fallback"
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
                            let outcome = win32_outcome(ffi::create_file_read(&ffi::wide(&spelling)));
                            let _ = std::process::Command::new("subst")
                                .arg(format!("{letter}:"))
                                .arg("/D")
                                .output();
                            Line::new(
                                "9",
                                spelling,
                                outcome,
                                format!(
                                    "the administrative UNC share attempt failed (win32:{unc_err}); \
                                     fell back to `subst {letter}: {dir}`"
                                ),
                            )
                        }
                        Ok(out) => {
                            let detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
                            unbuildable(
                                "9",
                                unc,
                                format!(
                                    "the administrative UNC share attempt failed (win32:{unc_err}) \
                                     and `subst {letter}: {dir}` also failed: {detail}"
                                ),
                            )
                        }
                        Err(e) => unbuildable(
                            "9",
                            unc,
                            format!(
                                "the administrative UNC share attempt failed (win32:{unc_err}) and \
                                 could not spawn subst: {e}"
                            ),
                        ),
                    }
                }
            }
        }
    }
}

fn free_drive_letter() -> Option<char> {
    // SAFETY: FFI, no arguments, no preconditions.
    let mask = unsafe { ffi::GetLogicalDrives() };
    (b'D'..=b'Z').rev().map(|b| b as char).find(|c| mask & (1u32 << (*c as u32 - 'A' as u32)) == 0)
}

// ---------------------------------------------------------------------
// Vector 10: alternate Unicode case form, plus a trailing dot and trailing
// space Win32 silently discards. Combined into one spelling.
// ---------------------------------------------------------------------
fn vector10_unicode_and_trailing(abs: &str) -> Line {
    let flipped: String = abs
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else if c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c
            }
        })
        .collect();
    if flipped == abs {
        return unbuildable("10", abs, "target path has no alphabetic characters to case-flip");
    }
    let spelling = format!("{flipped}. ");
    let outcome = win32_outcome(ffi::create_file_read(&ffi::wide(&spelling)));
    Line::new(
        "10",
        spelling,
        outcome,
        "case-flipped from the given spelling, with a trailing '.' and trailing space appended",
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
    let outcome = win32_outcome(ffi::create_file_read(&ffi::wide(&spelling)));
    Line::new(
        "11",
        spelling,
        outcome,
        "read-only OPEN_EXISTING against a stream that is not pre-created by this fixture; \
         not-found here means the stream doesn't already exist, not that streams are unsupported",
    )
}

// ---------------------------------------------------------------------
// Vector 12: '.'/'..' components and a redundant separator, combined into
// one spelling. The intervening directory name does not need to exist:
// Win32's non-verbatim path parsing collapses `.`/`..` lexically, not by
// walking the filesystem.
// ---------------------------------------------------------------------
fn vector12_dot_dotdot(abs: &str) -> Line {
    let Some((dir, name)) = parent_dir_and_filename(abs) else {
        return unbuildable("12", abs, "target path has no parent directory component");
    };
    let mut spelling = dir;
    spelling.push_str(r"\.\zz-vfs-escape-nonexistent-marker\..");
    spelling.push('\\');
    spelling.push('\\'); // redundant separator
    spelling.push_str(&name);
    let outcome = win32_outcome(ffi::create_file_read(&ffi::wide(&spelling)));
    Line::new(
        "12",
        spelling,
        outcome,
        "a '.' component, a '..' traversal through a directory name that need not exist, and a \
         doubled separator",
    )
}

// ---------------------------------------------------------------------
// Vector 13: a handle opened before the root registered. Reported, not
// closed, in this gate (gate 3's job). This standalone run has no
// session/root at all, so it cannot construct the actual timing — it only
// shows the ordinary reachability the real scenario builds on.
// ---------------------------------------------------------------------
fn vector13_preexisting_handle(abs: &str) -> Line {
    let outcome = win32_outcome(ffi::create_file_read(&ffi::wide(abs)));
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
// Vector 14: a child process without the shim. Reported, not closed, in
// this gate — may not be a shim fix at all.
// ---------------------------------------------------------------------
fn vector14_child_without_shim(abs: &str) -> Line {
    let note = "reported, not closed in this gate: a child process launched without the shim \
                injected reads the real filesystem directly, by construction -- there is no hook \
                in that process to intercept anything. Whether this is even a shim-layer fix at \
                all is an open question for a later gate, not settled here.";
    let spelling = format!("cmd /C type {abs}");
    match std::process::Command::new("cmd").arg("/C").arg("type").arg(abs).output() {
        Ok(out) if out.status.success() => Line::new("14", spelling, "opened", note),
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1);
            Line::new("14", spelling, format!("error:cmd-exit:{code}"), note)
        }
        Err(e) => Line::new("14", spelling, format!("unbuildable:could not spawn cmd: {e}"), note),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(target) = args.get(1) else {
        eprintln!("usage: vfs-fixture-escape <target-file-path> [output-file]");
        std::process::exit(1);
    };
    let abs = normalize_target(target);
    if std::fs::metadata(&abs).is_err() {
        eprintln!(
            "vfs-fixture-escape: warning: target path {abs} does not currently exist; several \
             vectors will legitimately report not-found or unbuildable rather than opened"
        );
    }

    let (line5, line5b) = guarded_pair("5", "5b", || vector5_handle_relative(&abs));
    let lines: Vec<Line> = vec![
        guarded("1", || vector1_short_name(&abs)),
        guarded("2", || vector2_extended_length(&abs)),
        guarded("3", || vector3_device_path(&abs)),
        guarded("4", || vector4_volume_guid(&abs)),
        line5,
        line5b,
        guarded("6", || vector6_cwd_relative(&abs)),
        guarded("7", || vector7_junction(&abs)),
        guarded("8", || vector8_hardlink(&abs)),
        guarded("9", || vector9_alias_drive(&abs)),
        guarded("10", || vector10_unicode_and_trailing(&abs)),
        guarded("11", || vector11_ads(&abs)),
        guarded("12", || vector12_dot_dotdot(&abs)),
        guarded("13", || vector13_preexisting_handle(&abs)),
        guarded("14", || vector14_child_without_shim(&abs)),
    ];

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
    std::process::exit(0);
}
