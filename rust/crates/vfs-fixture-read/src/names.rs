//! The names phase: what a handle says its final path is, and whether the
//! by-handle and by-path views of a file agree with it.
//!
//! This is what `std::filesystem::canonical`, `weakly_canonical`, `exists`,
//! `is_directory` and `equivalent` come down to — `CreateFileW` with
//! `FILE_FLAG_BACKUP_SEMANTICS`, then `GetFinalPathNameByHandleW`,
//! `GetFileAttributesW`, `GetFileInformationByHandle(Ex)` — called here
//! directly so every flag combination is asked, not only the one a given
//! runtime happens to use.
//!
//! Driven by three variables, each a `;`-separated list:
//!
//! - `VFS_FIXTURE_NAMES`: `kind|opened|final`, `kind` being `d` or `f`. The
//!   path `opened` must be of that kind and its final path must be exactly
//!   `final` (a DOS path, compared byte for byte), through every route.
//! - `VFS_FIXTURE_NAME_PREFIXES`: `dir|file`. `canonical(dir)` followed by a
//!   separator must be a byte prefix of `canonical(file)`.
//! - `VFS_FIXTURE_NAME_LISTS`: `dir|child,child…`. A listing of `dir` must
//!   contain each child, spelled exactly so.
//! - `VFS_FIXTURE_NAME_CREATES`: paths to create, in order, each spelled as
//!   it is to be stored; one ending in a separator is a directory. Done
//!   after `canonical` of every prefix directory has been taken and before
//!   anything else is checked, so the checks see the tree *after* writes and
//!   the prefix pairs compare a directory named before them with a file
//!   named after.
//! - `VFS_FIXTURE_NAME_RENAMES`: `from|to`, done after the creates. Each
//!   created or renamed name must then be listed, and named by `canonical`
//!   asked in another letter case, exactly as it was spelled.

use std::ffi::c_void;
use std::process::exit;

type Handle = *mut c_void;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_SHARE_ALL: u32 = 0x7;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

const VOLUME_NAME_DOS: u32 = 0x0;
const VOLUME_NAME_GUID: u32 = 0x1;
const VOLUME_NAME_NT: u32 = 0x2;
const VOLUME_NAME_NONE: u32 = 0x4;
const FILE_NAME_NORMALIZED: u32 = 0x0;
const FILE_NAME_OPENED: u32 = 0x8;

/// `FILE_INFO_BY_HANDLE_CLASS` values.
const FILE_NAME_INFO: u32 = 2;
const FILE_ATTRIBUTE_TAG_INFO: u32 = 9;
const FILE_ID_INFO: u32 = 18;

#[repr(C)]
#[derive(Default)]
struct ByHandleFileInformation {
    attributes: u32,
    creation: [u32; 2],
    access: [u32; 2],
    write: [u32; 2],
    volume_serial: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}

extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn CloseHandle(h: Handle) -> i32;
    fn GetFinalPathNameByHandleW(h: Handle, path: *mut u16, len: u32, flags: u32) -> u32;
    fn GetFileAttributesW(name: *const u16) -> u32;
    fn GetFileInformationByHandle(h: Handle, info: *mut ByHandleFileInformation) -> i32;
    fn GetFileInformationByHandleEx(h: Handle, class: u32, info: *mut c_void, len: u32) -> i32;
    fn GetLastError() -> u32;
}

fn fail(msg: String) -> ! {
    eprintln!("FIXTURE FAIL: names: {msg}");
    exit(1);
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A handle opened the way `std::filesystem` opens one to ask about a path:
/// attributes only, shared with everyone, directories allowed.
struct Opened(Handle);

impl Opened {
    fn at(path: &str) -> Opened {
        // SAFETY: the name is NUL-terminated and outlives the call.
        let h = unsafe {
            CreateFileW(
                wide(path).as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_ALL,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            // SAFETY: no arguments.
            fail(format!("open {path}: error {}", unsafe { GetLastError() }));
        }
        Opened(h)
    }

    /// `GetFinalPathNameByHandleW`, size-probed the way callers do it: ask
    /// with no buffer, allocate what it says, ask again.
    fn final_path(&self, flags: u32) -> Result<String, u32> {
        // SAFETY: a null buffer with length 0 is the documented size probe;
        // the second call is given a buffer of the length it is told.
        unsafe {
            let need = GetFinalPathNameByHandleW(self.0, std::ptr::null_mut(), 0, flags);
            if need == 0 {
                return Err(GetLastError());
            }
            let mut buf = vec![0u16; need as usize];
            let n = GetFinalPathNameByHandleW(self.0, buf.as_mut_ptr(), need, flags);
            if n == 0 || n >= need {
                return Err(GetLastError());
            }
            Ok(String::from_utf16_lossy(&buf[..n as usize]))
        }
    }

    fn by_handle(&self) -> ByHandleFileInformation {
        let mut info = ByHandleFileInformation::default();
        // SAFETY: `info` is a writable BY_HANDLE_FILE_INFORMATION.
        if unsafe { GetFileInformationByHandle(self.0, &mut info) } == 0 {
            // SAFETY: no arguments.
            fail(format!("GetFileInformationByHandle: error {}", unsafe {
                GetLastError()
            }));
        }
        info
    }

    /// `GetFileInformationByHandleEx` into `len` bytes, or the error.
    fn ex(&self, class: u32, len: usize) -> Result<Vec<u8>, u32> {
        let mut buf = vec![0xEEu8; len];
        // SAFETY: the buffer is writable for the length passed.
        let ok = unsafe {
            GetFileInformationByHandleEx(self.0, class, buf.as_mut_ptr().cast(), len as u32)
        };
        if ok == 0 {
            // SAFETY: no arguments.
            return Err(unsafe { GetLastError() });
        }
        Ok(buf)
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn drive_relative(dos: &str) -> &str {
    &dos[2..]
}

/// Flip the case of every ASCII letter after the drive: a spelling of the
/// same path that shares no letter case with the stored one.
fn other_case(path: &str) -> String {
    path.chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c.to_ascii_uppercase()
            }
        })
        .collect()
}

/// Everything asked of one path. `real` is a directory that exists on the
/// host's own filesystem, asked the same questions first: where the host
/// itself cannot answer a flag for a real directory, a virtual one is not
/// held to it.
fn check(kind: &str, opened: &str, want: &str, real: &Opened) {
    let is_dir = kind == "d";
    let h = Opened::at(opened);

    for (label, flags) in [
        ("DOS|NORMALIZED", VOLUME_NAME_DOS | FILE_NAME_NORMALIZED),
        ("DOS|OPENED", VOLUME_NAME_DOS | FILE_NAME_OPENED),
        ("NONE|NORMALIZED", VOLUME_NAME_NONE | FILE_NAME_NORMALIZED),
        ("NONE|OPENED", VOLUME_NAME_NONE | FILE_NAME_OPENED),
        ("NT|NORMALIZED", VOLUME_NAME_NT | FILE_NAME_NORMALIZED),
        ("NT|OPENED", VOLUME_NAME_NT | FILE_NAME_OPENED),
        ("GUID|NORMALIZED", VOLUME_NAME_GUID | FILE_NAME_NORMALIZED),
    ] {
        let got = h.final_path(flags);
        let volume = flags & 0x7;
        let rel = drive_relative(want);
        let right = match (&got, volume) {
            (Ok(p), VOLUME_NAME_DOS) => *p == format!(r"\\?\{want}"),
            (Ok(p), VOLUME_NAME_NONE) => p == rel,
            (Ok(p), VOLUME_NAME_NT) => p.starts_with(r"\Device\") && p.ends_with(rel),
            (Ok(p), _) => p.starts_with(r"\\?\Volume{") && p.ends_with(rel),
            // Refused: acceptable only where the host refuses a real
            // directory the same flag.
            (Err(_), _) => real.final_path(flags).is_err(),
        };
        if !right {
            fail(format!(
                "{opened}: GetFinalPathNameByHandleW({label}) gave {got:?}, want the final path \
                 {want} in that form (a real directory gives {:?})",
                real.final_path(flags)
            ));
        }
    }

    // The path std hands out, which is the DOS|NORMALIZED answer.
    match std::fs::canonicalize(opened) {
        Ok(p) if p.to_string_lossy() == format!(r"\\?\{want}") => {}
        other => fail(format!(
            "{opened}: canonicalize gave {other:?}, want {want}"
        )),
    }

    // By path.
    // SAFETY: the name is NUL-terminated and outlives the call.
    let attrs = unsafe { GetFileAttributesW(wide(opened).as_ptr()) };
    if attrs == INVALID_FILE_ATTRIBUTES || (attrs & FILE_ATTRIBUTE_DIRECTORY != 0) != is_dir {
        fail(format!("{opened}: GetFileAttributesW gave {attrs:#x}"));
    }
    match std::fs::metadata(opened) {
        Ok(m) if m.is_dir() == is_dir && m.is_file() != is_dir => {}
        other => fail(format!("{opened}: metadata gave {other:?}")),
    }

    // By handle.
    let info = h.by_handle();
    if (info.attributes & FILE_ATTRIBUTE_DIRECTORY != 0) != is_dir || info.links == 0 {
        fail(format!(
            "{opened}: GetFileInformationByHandle gave attributes {:#x}, links {}",
            info.attributes, info.links
        ));
    }
    match h.ex(FILE_ATTRIBUTE_TAG_INFO, 8) {
        Ok(b) => {
            let a = u32::from_le_bytes(b[0..4].try_into().unwrap());
            let tag = u32::from_le_bytes(b[4..8].try_into().unwrap());
            if (a & FILE_ATTRIBUTE_DIRECTORY != 0) != is_dir || tag != 0 {
                fail(format!(
                    "{opened}: FileAttributeTagInfo gave {a:#x}, tag {tag:#x}"
                ));
            }
        }
        Err(e) => fail(format!("{opened}: FileAttributeTagInfo: error {e}")),
    }
    // FILE_NAME_INFO: u32 byte length, then the volume-relative name.
    match h.ex(FILE_NAME_INFO, 4 + 2 * 1024) {
        Ok(b) => {
            let n = u32::from_le_bytes(b[0..4].try_into().unwrap()) as usize;
            let units: Vec<u16> = b[4..4 + n]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let name = String::from_utf16_lossy(&units);
            if name != drive_relative(want) {
                fail(format!(
                    "{opened}: FileNameInfo gave {name:?}, want {:?}",
                    drive_relative(want)
                ));
            }
        }
        Err(e) => fail(format!("{opened}: FileNameInfo: error {e}")),
    }

    // The same thing opened in another letter case is the same file: the
    // same final path, the same index, the same id.
    let flipped = format!("{}{}", &opened[..2], other_case(&opened[2..]));
    let other = Opened::at(&flipped);
    match other.final_path(VOLUME_NAME_DOS) {
        Ok(p) if p == format!(r"\\?\{want}") => {}
        got => fail(format!(
            "{flipped}: final path {got:?}, want {want}: the answer followed the caller's spelling"
        )),
    }
    let again = other.by_handle();
    if (again.index_high, again.index_low, again.volume_serial)
        != (info.index_high, info.index_low, info.volume_serial)
    {
        fail(format!(
            "{opened} and {flipped} report different file indexes: one file is not equal to itself"
        ));
    }
    match (h.ex(FILE_ID_INFO, 24), other.ex(FILE_ID_INFO, 24)) {
        (Ok(a), Ok(b)) if a == b => {
            // The volume in it is the volume the by-handle query names.
            let serial = u64::from_le_bytes(a[0..8].try_into().unwrap());
            if serial != info.volume_serial as u64 {
                fail(format!(
                    "{opened}: FileIdInfo says volume {serial:#x}, GetFileInformationByHandle {:#x}",
                    info.volume_serial
                ));
            }
        }
        (Ok(_), Ok(_)) => fail(format!(
            "{opened} and {flipped} report different FileIdInfo"
        )),
        (a, b) => fail(format!(
            "{opened}: FileIdInfo failed: {:?} / {:?}",
            a.err(),
            b.err()
        )),
    }
    println!("FIXTURE NAMES: {kind} {opened} -> {want}");
}

fn entries(var: &str) -> Vec<Vec<String>> {
    std::env::var(var)
        .unwrap_or_default()
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|e| e.split('|').map(str::to_string).collect())
        .collect()
}

/// Run the phase if any of its variables is set. Returns only if it passed.
pub fn run() {
    let names = entries("VFS_FIXTURE_NAMES");
    let prefixes = entries("VFS_FIXTURE_NAME_PREFIXES");
    let lists = entries("VFS_FIXTURE_NAME_LISTS");
    let creates = entries("VFS_FIXTURE_NAME_CREATES");
    let renames = entries("VFS_FIXTURE_NAME_RENAMES");
    if [&names, &prefixes, &lists, &creates, &renames]
        .iter()
        .all(|v| v.is_empty())
    {
        return;
    }
    // The directories' names, taken before anything is written.
    let dirs_before: Vec<String> = prefixes
        .iter()
        .map(|e| match std::fs::canonicalize(&e[0]) {
            Ok(d) => d.to_string_lossy().into_owned(),
            Err(err) => fail(format!("canonicalize {} before the writes: {err}", e[0])),
        })
        .collect();
    let mut made: Vec<String> = Vec::new();
    for e in &creates {
        let path = &e[0];
        let made_ok = if let Some(dir) = path.strip_suffix('\\') {
            made.push(dir.to_string());
            std::fs::create_dir(dir)
        } else {
            made.push(path.clone());
            std::fs::write(path, b"created")
        };
        if let Err(err) = made_ok {
            fail(format!("create {path}: {err}"));
        }
    }
    for e in &renames {
        let [from, to] = e.as_slice() else {
            fail(format!(
                "VFS_FIXTURE_NAME_RENAMES entry {e:?} is not from|to"
            ));
        };
        if let Err(err) = std::fs::rename(from, to) {
            fail(format!("rename {from} -> {to}: {err}"));
        }
        made.retain(|m| !m.eq_ignore_ascii_case(from));
        made.push(to.clone());
    }
    for path in &made {
        let p = std::path::Path::new(path);
        let (parent, name) = (
            p.parent().unwrap(),
            p.file_name().unwrap().to_string_lossy(),
        );
        let listed: Vec<String> = match std::fs::read_dir(parent) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(err) => fail(format!("read_dir {}: {err}", parent.display())),
        };
        if !listed.iter().any(|l| *l == name) {
            fail(format!(
                "{path} was created as {name:?}, but {} lists {listed:?}",
                parent.display()
            ));
        }
        // Named, whatever case it is asked in, as its directory is named and
        // then exactly as it was created. (The directories above it keep
        // their own stored spelling, which need not be how the creating path
        // spelled them.)
        let asked = format!("{}{}", &path[..2], other_case(&path[2..]));
        let want = match std::fs::canonicalize(parent) {
            Ok(dir) => format!(r"{}\{name}", dir.to_string_lossy()),
            Err(err) => fail(format!("canonicalize {}: {err}", parent.display())),
        };
        match std::fs::canonicalize(&asked) {
            Ok(c) if c.to_string_lossy() == want => {}
            other => fail(format!(
                "canonicalize({asked}) gave {other:?}, want {want}: {path} is not named as it was created"
            )),
        }
        println!("FIXTURE NAMES: created {path}, listed and named as such");
    }
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\windows".to_string());
    let real = Opened::at(&windir);
    // Said out loud, so a run shows which forms were actually compared and
    // which the host does not answer even for its own directories.
    for (label, flags) in [
        ("DOS", VOLUME_NAME_DOS),
        ("NONE", VOLUME_NAME_NONE),
        ("NT", VOLUME_NAME_NT),
        ("GUID", VOLUME_NAME_GUID),
    ] {
        println!(
            "FIXTURE NAMES: host, {windir}, VOLUME_NAME_{label}: {:?}",
            real.final_path(flags)
        );
    }

    let mut ids = Vec::new();
    for e in &names {
        let [kind, opened, want] = e.as_slice() else {
            fail(format!(
                "VFS_FIXTURE_NAMES entry {e:?} is not kind|opened|final"
            ));
        };
        check(kind, opened, want, &real);
        let info = Opened::at(opened).by_handle();
        ids.push((want.clone(), (info.index_high, info.index_low)));
    }
    // Different files have different indexes.
    for (i, (a, ida)) in ids.iter().enumerate() {
        for (b, idb) in &ids[i + 1..] {
            if a != b && ida == idb {
                fail(format!("{a} and {b} report the same file index"));
            }
        }
    }

    for (e, before) in prefixes.iter().zip(&dirs_before) {
        let [dir, file] = e.as_slice() else {
            fail(format!(
                "VFS_FIXTURE_NAME_PREFIXES entry {e:?} is not dir|file"
            ));
        };
        let (d, f) = match (std::fs::canonicalize(dir), std::fs::canonicalize(file)) {
            (Ok(d), Ok(f)) => (d, f),
            other => fail(format!("canonicalize {dir} / {file}: {other:?}")),
        };
        let (d, f) = (
            d.to_string_lossy().into_owned(),
            f.to_string_lossy().into_owned(),
        );
        // A plugin that takes its directory's name at start-up and checks a
        // file against it later: the writes in between must not respell it.
        if d != *before {
            fail(format!(
                "canonical({dir}) was {before} before the writes and is {d} after them"
            ));
        }
        // Byte for byte, and at a component boundary: what a containment
        // check does.
        if !f.starts_with(&format!(r"{d}\")) {
            fail(format!(
                "canonical({dir}) = {d} is not a prefix of canonical({file}) = {f}"
            ));
        }
        // And the component-wise view agrees.
        if !std::path::Path::new(&f).starts_with(&d) {
            fail(format!("{f} does not start with {d} component-wise"));
        }
        println!("FIXTURE NAMES: {d} contains {f}");
    }

    for e in &lists {
        let [dir, children] = e.as_slice() else {
            fail(format!("VFS_FIXTURE_NAME_LISTS entry {e:?} is not dir|a,b"));
        };
        let found: Vec<String> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(e) => fail(format!("read_dir {dir}: {e}")),
        };
        for child in children.split(',') {
            if !found.iter().any(|f| f == child) {
                fail(format!("{dir} lists {found:?}, which has no {child}"));
            }
        }
        println!("FIXTURE NAMES: {dir} lists {children}");
    }
    println!("FIXTURE NAMES OK: {} paths", names.len());
}
