//! NT create dispositions and access masks, and the decisions made from them.

/// NtCreateFile create dispositions.
pub const FILE_SUPERSEDE: u32 = 0;
pub const FILE_OPEN: u32 = 1;
pub const FILE_CREATE: u32 = 2;
pub const FILE_OPEN_IF: u32 = 3;
pub const FILE_OVERWRITE: u32 = 4;
pub const FILE_OVERWRITE_IF: u32 = 5;

/// How an open intends to touch a file's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteIntent {
    /// The caller can modify content (has write/append/generic-write access).
    pub write: bool,
    /// The disposition keeps existing content (`OPEN`/`OPEN_IF`) rather than
    /// truncating or replacing it — the signal that a copy-on-write materialize
    /// must preserve the current bytes.
    pub preserves: bool,
}

/// `FILE_WRITE_DATA`: the specific right to write file content.
pub const FILE_WRITE_DATA: u32 = 0x0002;
/// `FILE_APPEND_DATA`: the specific right to append.
pub const FILE_APPEND_DATA: u32 = 0x0004;
/// `GENERIC_WRITE`, as the raw mask a hook observes (the kernel maps it later).
pub const GENERIC_WRITE: u32 = 0x4000_0000;
/// `GENERIC_ALL`, as the raw mask a hook observes.
pub const GENERIC_ALL: u32 = 0x1000_0000;
/// The access bits the shim's `is_write_open` treats as write access. `GENERIC_ALL` is not in
/// it, though it implies write: `classify_open` counts it and the shim's predicate has never
/// done so, and the two are kept as they were.
pub const WRITE_ACCESS: u32 = FILE_WRITE_DATA | FILE_APPEND_DATA | GENERIC_WRITE;

/// Classify an open from its desired-access mask and create disposition.
pub fn classify_open(access: u32, disposition: u32) -> WriteIntent {
    WriteIntent {
        write: access & (WRITE_ACCESS | GENERIC_ALL) != 0,
        preserves: matches!(disposition, FILE_OPEN | FILE_OPEN_IF),
    }
}

/// `IoStatusBlock.Information` of a successful create: an existing object was replaced.
pub const FILE_SUPERSEDED: usize = 0;
/// `IoStatusBlock.Information`: an existing object was opened.
pub const FILE_OPENED: usize = 1;
/// `IoStatusBlock.Information`: a fresh object was created.
pub const FILE_CREATED: usize = 2;
/// `IoStatusBlock.Information`: an existing object was truncated in place.
pub const FILE_OVERWRITTEN: usize = 3;

/// `FILE_OPEN_IF` (3) belongs in the disposition set alongside the other
/// creating dispositions: it may create the path (no different from
/// `FILE_CREATE`/`FILE_SUPERSEDE` in that respect), so a caller that asks
/// for it with only read access must still route through the write path —
/// otherwise a create-if-absent read open is treated as a plain read, which
/// reports `ST_NOT_FOUND` instead of creating the file, on an absent path.
/// (Before gate 4's Task 5 that miss also *fell through* to the shim-local
/// overlay, so the misclassification silently "worked"; now it is a sealed
/// failure, which is the same reason getting this predicate right matters
/// more, not less.)
pub fn is_write_open(access: u32, disposition: u32) -> bool {
    (access & WRITE_ACCESS) != 0 || matches!(disposition, 0 | 2 | 3 | 4 | 5)
}

/// True for NT's append-only access grant: `FILE_APPEND_DATA` without
/// `FILE_WRITE_DATA`. A real file object forces every write on such a handle
/// to the current end of file, ignoring any caller-supplied offset, because
/// the kernel enforces it at the file-object level. A synthetic handle has no
/// kernel object to do that for it, so the open path has to seed the tracked
/// position at the file's current size (`synth_file::open_fuse_at_ex`) and
/// `write_hook` has to keep pinning it there — see both for the other half.
///
/// `Rust`'s `OpenOptions::append(true)` without `.write(true)` — the fixture's
/// reopen-for-append step — requests exactly this access, which is how the
/// gap surfaced: an append reopen's first write landed at position 0 (the
/// hardcoded initial value) and silently overwrote the file's existing bytes
/// instead of extending it.
///
/// `GENERIC_WRITE` must count as full write access here too, same as
/// `is_write_open`'s `WRITE_ACCESS`: a caller requesting
/// `GENERIC_WRITE | FILE_APPEND_DATA` wants ordinary positional writes plus
/// append, not append-only — checking only the literal `FILE_WRITE_DATA` bit
/// missed that, because `GENERIC_WRITE` is a generic right that implies
/// `FILE_WRITE_DATA` without necessarily carrying its specific bit set in the
/// raw mask this hook observes.
pub fn is_append_only(access: u32) -> bool {
    use {FILE_APPEND_DATA, FILE_WRITE_DATA, GENERIC_WRITE};
    access & FILE_APPEND_DATA != 0 && access & (FILE_WRITE_DATA | GENERIC_WRITE) == 0
}

/// Map an NT create-disposition to the ring's `OPEN_CREATE`/`OPEN_EXCL`/
/// `OPEN_TRUNC` bits (`OPEN_WRITE` itself is added by the caller). Forwarding
/// this is what closes the gap Task 6 found: without it every brand-new file
/// gets `ST_NOT_FOUND` from the director regardless of disposition. That used
/// to fall through to the shim-local overlay redirect and so merely misplace
/// the bytes; since gate 4's Task 5 sealed that fall-through it would instead
/// fail every create outright, so a mistake in this mapping is now a game that
/// cannot write at all rather than one that writes to the wrong place.
///
/// Verified against NT `CreateDisposition` semantics one value at a time
/// (a prior draft of this mapping under-specified two of the six):
/// - `FILE_SUPERSEDE` (0): create if absent, replace if present -> needs
///   **both** `OPEN_CREATE` and `OPEN_TRUNC` — a `OPEN_TRUNC`-only mapping
///   fails `ST_NOT_FOUND` on an absent file, which is exactly the bug this
///   function exists to close.
/// - `FILE_OPEN` (1): open only, must fail if absent -> no flags.
/// - `FILE_CREATE` (2): create only, must fail if present -> `OPEN_CREATE |
///   OPEN_EXCL`.
/// - `FILE_OPEN_IF` (3): open if present (no data loss), create if absent ->
///   `OPEN_CREATE` alone (the provider's `OPEN_CREATE` is a no-op on an
///   existing file — it does not also truncate).
/// - `FILE_OVERWRITE` (4): must already exist, truncate -> `OPEN_TRUNC` alone
///   (no `OPEN_CREATE`, so an absent file still fails `ST_NOT_FOUND`, matching
///   "fail if it does not exist").
/// - `FILE_OVERWRITE_IF` (5): overwrite if present, create if absent -> needs
///   **both**, same as `FILE_SUPERSEDE` — this is the case the brief already
///   flagged for a re-check.
///
/// Cross-checked against `DiskProvider::open` (`disk.rs`), which folds these
/// straight into `OpenOptions::create/create_new/truncate`, and the
/// conformance fixture's `open`, which create-if-absent-then-truncate in that
/// order — both agree with the mapping above.
pub fn open_create_flags(disposition: u32) -> u32 {
    use vfs_protocol::{OPEN_CREATE, OPEN_EXCL, OPEN_TRUNC};
    match disposition {
        0 => OPEN_CREATE | OPEN_TRUNC,
        2 => OPEN_CREATE | OPEN_EXCL,
        3 => OPEN_CREATE,
        4 => OPEN_TRUNC,
        5 => OPEN_CREATE | OPEN_TRUNC,
        _ => 0, // FILE_OPEN (1), and anything unrecognized.
    }
}

/// True for the dispositions where a write-flavoured open that turns out to
/// name an existing **directory** is a legitimate directory open rather than
/// a failed file create.
///
/// `is_write_open`'s `WRITE_ACCESS` includes `0x0002 | 0x0004`, which on a
/// *directory* handle are `FILE_ADD_FILE` and `FILE_ADD_SUBDIRECTORY`, not
/// `FILE_WRITE_DATA`/`FILE_APPEND_DATA`. The bits are identical and nothing
/// in the mask distinguishes them, so every `FILE_FLAG_BACKUP_SEMANTICS` open
/// asking for write access on a directory arrives as a write, gets routed to
/// `Provider::open(OPEN_WRITE)`, and fails: `DiskProvider::open` opens
/// read+write, which a directory refuses. Since gate 4's Task 5 that failure
/// is no longer papered over by the fall-through — the caller now gets
/// `STATUS_UNSUCCESSFUL` (`ERROR_GEN_FAILURE`) for an operation NTFS answers
/// without complaint.
///
/// Only `FILE_OPEN` and `FILE_OPEN_IF` qualify. The other four
/// (`SUPERSEDE`/`CREATE`/`OVERWRITE`/`OVERWRITE_IF`) all intend to create or
/// replace, and NT answers those against an existing directory with a
/// collision or `STATUS_FILE_IS_A_DIRECTORY` — handing back a directory
/// handle there would turn a refused file create into a silent success.
/// Directory *creates* never reach this at all: `try_fuse_mkdir` runs first
/// and takes `FILE_DIRECTORY_FILE` with a creating disposition.
pub fn dir_open_downgrades(disposition: u32) -> bool {
    matches!(disposition, 1 | 3)
}

/// True for the three dispositions whose successful `IoStatusBlock`
/// `Information` depends on whether the path already existed
/// (`FILE_SUPERSEDE`/`FILE_OPEN_IF`/`FILE_OVERWRITE_IF`) — see
/// `disposition_information`. The other three have one fixed outcome and
/// need no probe.
pub fn disposition_needs_existence_probe(disposition: u32) -> bool {
    matches!(disposition, 0 | 3 | 5)
}

/// The correct `IoStatusBlock.Information` for a *successful* create/open,
/// given the NT create-disposition and (for the three dispositions where it
/// matters) whether the path existed before the call.
///
/// `create_hook` used to hardcode `FILE_OPENED` here unconditionally, which
/// was invisible while every write fell through to a real file (whose kernel
/// FCB reports this correctly on its own) — only reachable now that writes
/// succeed through the director. Kernel32's `ERROR_ALREADY_EXISTS` signalling
/// for `CREATE_ALWAYS` (`FILE_SUPERSEDE`) / `OPEN_ALWAYS` (`FILE_OPEN_IF`)
/// reads exactly this field, so getting it wrong is not cosmetic.
///
/// NT's own table:
/// - `FILE_OPEN` (1): always `FILE_OPENED` — must already exist.
/// - `FILE_CREATE` (2): always `FILE_CREATED` — must not have existed
///   (`OPEN_EXCL` already enforces this; success implies "created").
/// - `FILE_OVERWRITE` (4): always `FILE_OVERWRITTEN` — must already exist.
/// - `FILE_SUPERSEDE` (0), `FILE_OPEN_IF` (3), `FILE_OVERWRITE_IF` (5):
///   outcome depends on whether the path existed — this is exactly why
///   `disposition_needs_existence_probe` singles these three out.
pub fn disposition_information(disposition: u32, existed_before: bool) -> usize {
    match disposition {
        0 => {
            if existed_before {
                FILE_SUPERSEDED
            } else {
                FILE_CREATED
            }
        }
        2 => FILE_CREATED,
        3 => {
            if existed_before {
                FILE_OPENED
            } else {
                FILE_CREATED
            }
        }
        4 => FILE_OVERWRITTEN,
        5 => {
            if existed_before {
                FILE_OVERWRITTEN
            } else {
                FILE_CREATED
            }
        }
        _ => FILE_OPENED, // FILE_OPEN (1), and anything unrecognized.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared write-access constants: `classify_open` counts `GENERIC_ALL`, the bare
    /// `WRITE_ACCESS` mask the shim's `is_write_open` uses does not (kept as it was).
    #[test]
    fn write_access_masks_agree_with_classify_open() {
        assert_eq!(WRITE_ACCESS, 0x4000_0006);
        for bit in [
            FILE_WRITE_DATA,
            FILE_APPEND_DATA,
            GENERIC_WRITE,
            GENERIC_ALL,
        ] {
            assert!(classify_open(bit, FILE_OPEN).write, "{bit:#x}");
        }
        assert!(!classify_open(0x8000_0000, FILE_OPEN).write); // GENERIC_READ
        assert_eq!(WRITE_ACCESS & GENERIC_ALL, 0);
    }

    #[test]
    fn classify_open_reads_writes_and_preserves() {
        // Read: SYNCHRONIZE|READ_DATA, disp OPEN -> not a write.
        assert_eq!(
            classify_open(0x0010_0001, FILE_OPEN),
            WriteIntent {
                write: false,
                preserves: true
            }
        );
        // GENERIC_WRITE + OPEN_IF -> write, preserves (COW-materialize).
        assert_eq!(
            classify_open(0x4010_0080, FILE_OPEN_IF),
            WriteIntent {
                write: true,
                preserves: true
            }
        );
        // GENERIC_WRITE + OVERWRITE_IF -> write, does not preserve (truncate).
        assert_eq!(
            classify_open(0x4000_0000, FILE_OVERWRITE_IF),
            WriteIntent {
                write: true,
                preserves: false
            }
        );
        // APPEND_DATA + CREATE -> write, create (no preserve).
        assert_eq!(
            classify_open(0x4, FILE_CREATE),
            WriteIntent {
                write: true,
                preserves: false
            }
        );
    }

    // --- Fix 8: two disposition-classification bugs.

    /// `GENERIC_WRITE | FILE_APPEND_DATA` is ordinary write-plus-append
    /// access, not append-only: `GENERIC_WRITE` already grants full
    /// positional write. Before the fix, `is_append_only` checked only the
    /// literal `FILE_WRITE_DATA` bit (0x0002), which `GENERIC_WRITE`
    /// (0x4000_0000) does not itself set in the raw mask this hook observes
    /// — so this combination was misclassified as append-only, which would
    /// have pinned every write to EOF regardless of the caller's offset.
    #[test]
    fn generic_write_with_append_data_is_not_append_only() {
        assert!(!is_append_only(GENERIC_WRITE | FILE_APPEND_DATA));
    }

    /// The genuine append-only shape — `FILE_APPEND_DATA` with neither
    /// `FILE_WRITE_DATA` nor `GENERIC_WRITE` — must still be classified as
    /// append-only. `Rust`'s `OpenOptions::append(true)` (without
    /// `.write(true)`) requests exactly this.
    #[test]
    fn append_data_alone_is_append_only() {
        assert!(is_append_only(FILE_APPEND_DATA));
    }

    /// `FILE_WRITE_DATA` set explicitly alongside `FILE_APPEND_DATA` is full
    /// write access, not append-only — unchanged by the fix, kept here so a
    /// future edit cannot silently invert it.
    #[test]
    fn explicit_write_data_with_append_data_is_not_append_only() {
        assert!(!is_append_only(FILE_WRITE_DATA | FILE_APPEND_DATA));
    }

    /// `FILE_OPEN_IF` (3) may create the path, exactly like `FILE_CREATE`/
    /// `FILE_SUPERSEDE`/`FILE_OVERWRITE_IF` — so it must count as a write
    /// open even with only read access requested. Before the fix, disposition
    /// 3 was missing from `is_write_open`'s disposition set, so a
    /// create-if-absent read open (read access + `FILE_OPEN_IF`) was treated
    /// as a plain read and never reached the director's create path on an
    /// absent file.
    #[test]
    fn file_open_if_with_read_only_access_is_a_write_open() {
        const GENERIC_READ: u32 = 0x8000_0000;
        const FILE_OPEN_IF: u32 = 3;
        assert!(is_write_open(GENERIC_READ, FILE_OPEN_IF));
    }

    /// Every disposition NT itself can create through must be a write open
    /// regardless of the access mask; `FILE_OPEN` (1) is the sole disposition
    /// that depends on the access mask alone.
    #[test]
    fn every_creating_disposition_is_a_write_open_even_with_read_only_access() {
        const GENERIC_READ: u32 = 0x8000_0000;
        for disposition in [0u32, 2, 3, 4, 5] {
            assert!(
                is_write_open(GENERIC_READ, disposition),
                "disposition {disposition} must be a write open"
            );
        }
        const FILE_OPEN: u32 = 1;
        assert!(
            !is_write_open(GENERIC_READ, FILE_OPEN),
            "FILE_OPEN with only read access must not be a write open"
        );
    }

    /// Gate 4, Task 6. Only the two non-creating dispositions may hand back a
    /// directory handle when a write-flavoured open turns out to name a
    /// directory. Widening this to the creating four would turn "you cannot
    /// create a file where a directory already is" — which NT answers with a
    /// collision or `STATUS_FILE_IS_A_DIRECTORY` — into a silent success
    /// handing the caller a directory handle it never asked for.
    #[test]
    fn only_non_creating_dispositions_downgrade_a_directory_open() {
        assert!(
            dir_open_downgrades(1),
            "FILE_OPEN opens an existing directory"
        );
        assert!(
            dir_open_downgrades(3),
            "FILE_OPEN_IF opens an existing directory"
        );
        for disposition in [0u32, 2, 4, 5] {
            assert!(
                !dir_open_downgrades(disposition),
                "disposition {disposition} intends to create or replace a file; a directory \
                 handle is not an acceptable answer to it"
            );
        }
    }

    // --- Fix 7: per-disposition IoStatusBlock.Information.

    #[test]
    fn disposition_information_matches_nt_semantics() {
        // FILE_SUPERSEDE (0): existed -> SUPERSEDED, absent -> CREATED.
        assert_eq!(disposition_information(0, true), FILE_SUPERSEDED);
        assert_eq!(disposition_information(0, false), FILE_CREATED);
        // FILE_OPEN (1): always OPENED.
        assert_eq!(disposition_information(1, true), FILE_OPENED);
        assert_eq!(disposition_information(1, false), FILE_OPENED);
        // FILE_CREATE (2): always CREATED.
        assert_eq!(disposition_information(2, true), FILE_CREATED);
        assert_eq!(disposition_information(2, false), FILE_CREATED);
        // FILE_OPEN_IF (3): existed -> OPENED, absent -> CREATED.
        assert_eq!(disposition_information(3, true), FILE_OPENED);
        assert_eq!(disposition_information(3, false), FILE_CREATED);
        // FILE_OVERWRITE (4): always OVERWRITTEN.
        assert_eq!(disposition_information(4, true), FILE_OVERWRITTEN);
        assert_eq!(disposition_information(4, false), FILE_OVERWRITTEN);
        // FILE_OVERWRITE_IF (5): existed -> OVERWRITTEN, absent -> CREATED.
        assert_eq!(disposition_information(5, true), FILE_OVERWRITTEN);
        assert_eq!(disposition_information(5, false), FILE_CREATED);
    }

    #[test]
    fn only_the_three_conditional_dispositions_need_an_existence_probe() {
        for d in [0u32, 3, 5] {
            assert!(disposition_needs_existence_probe(d), "disposition {d}");
        }
        for d in [1u32, 2, 4] {
            assert!(!disposition_needs_existence_probe(d), "disposition {d}");
        }
    }
}
