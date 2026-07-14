//! Acceptance exerciser. Run *injected* against a managed VFS root, it drives
//! every read-path feature through real Win32/CRT/std calls and writes a report
//! (`check=PASS` / `check=FAIL: detail` per line) to argv[2], exiting 0 iff all
//! checks pass. The host test (`tests/acceptance.rs`) builds the matching disk
//! layout + snapshot and asserts every line is PASS.
//!
//! Expected layout under the root (argv[1]); backing files live outside it:
//!   mod_added.txt   virtual file  -> "MOD-ADDED-BYTES"      (not on disk)
//!   override.txt    real "REAL-OVERRIDE" shadowed by mod    -> "MOD-OVERRIDE-BYTES"
//!   real_only.txt   real file, not virtualized             -> "REAL-ONLY-BYTES"
//!   deleted.txt     real file, tombstoned                  -> hidden
//!   virtual_dir     virtual directory                      (not on disk)
//!   real_dir        real directory
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    GetFileAttributesW, GetFinalPathNameByHandleW, FILE_ATTRIBUTE_DIRECTORY,
    INVALID_FILE_ATTRIBUTES,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn attrs(path: &Path) -> u32 {
    unsafe { GetFileAttributesW(wide(path.to_str().unwrap()).as_ptr()) }
}

fn final_path(path: &Path) -> Result<String, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let h = f.as_raw_handle() as HANDLE;
    let mut buf = vec![0u16; 2048];
    let n = unsafe { GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, 0) };
    if n == 0 {
        return Err("GetFinalPathNameByHandleW failed".into());
    }
    Ok(String::from_utf16_lossy(&buf[..n as usize]))
}

/// Run one check: `Ok(())` -> PASS, `Err(detail)` -> FAIL.
fn check(name: &str, out: &mut String, all_ok: &mut bool, f: impl FnOnce() -> Result<(), String>) {
    match f() {
        Ok(()) => out.push_str(&format!("{name}=PASS\n")),
        Err(d) => {
            out.push_str(&format!("{name}=FAIL: {d}\n"));
            *all_ok = false;
        }
    }
}

fn expect_eq(got: &[u8], want: &[u8]) -> Result<(), String> {
    if got == want {
        Ok(())
    } else {
        Err(format!("got {:?} want {:?}", String::from_utf8_lossy(got), String::from_utf8_lossy(want)))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        std::process::exit(2);
    }

    // Child mode: `--child <root> <output>`. Spawned by the acceptance run to
    // prove the VFS propagated into this child process. Read a VIRTUAL file
    // (only resolvable if this child's shim is active) and echo it to <output>.
    if args[1] == "--child" {
        if args.len() < 4 {
            std::process::exit(2);
        }
        let content = std::fs::read(Path::new(&args[2]).join("mod_added.txt")).unwrap_or_default();
        std::fs::write(&args[3], &content).expect("child write output");
        std::process::exit(0);
    }

    let root = Path::new(&args[1]);
    let report = &args[2];

    let mut out = String::new();
    let mut all_ok = true;

    // 1. Read a mod-added virtual file -> backing bytes.
    check("read_added", &mut out, &mut all_ok, || {
        let got = std::fs::read(root.join("mod_added.txt")).map_err(|e| e.to_string())?;
        expect_eq(&got, b"MOD-ADDED-BYTES")
    });

    // 2. Read an overridden file -> the MOD bytes win over the real file.
    check("read_override", &mut out, &mut all_ok, || {
        let got = std::fs::read(root.join("override.txt")).map_err(|e| e.to_string())?;
        expect_eq(&got, b"MOD-OVERRIDE-BYTES")
    });

    // 3. Read a real, non-virtualized file -> passes through.
    check("read_real_only", &mut out, &mut all_ok, || {
        let got = std::fs::read(root.join("real_only.txt")).map_err(|e| e.to_string())?;
        expect_eq(&got, b"REAL-ONLY-BYTES")
    });

    // 4. Open a tombstoned (mod-deleted) real file -> must fail.
    check("tombstone_read", &mut out, &mut all_ok, || {
        match std::fs::read(root.join("deleted.txt")) {
            Err(_) => Ok(()),
            Ok(b) => Err(format!("tombstoned file was readable: {} bytes", b.len())),
        }
    });

    // 5. Attributes of a virtual file -> valid, not a directory.
    check("attr_added", &mut out, &mut all_ok, || {
        let a = attrs(&root.join("mod_added.txt"));
        if a == INVALID_FILE_ATTRIBUTES {
            return Err("virtual file has no attributes".into());
        }
        if a & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Err("virtual file wrongly flagged directory".into());
        }
        Ok(())
    });

    // 6. Attributes of a virtual directory -> valid, directory bit set.
    check("attr_vdir", &mut out, &mut all_ok, || {
        let a = attrs(&root.join("virtual_dir"));
        if a == INVALID_FILE_ATTRIBUTES {
            return Err("virtual dir has no attributes".into());
        }
        if a & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err("virtual dir missing directory bit".into());
        }
        Ok(())
    });

    // 7. Attributes of a tombstoned file -> hidden (INVALID).
    check("attr_tombstone", &mut out, &mut all_ok, || {
        let a = attrs(&root.join("deleted.txt"));
        if a == INVALID_FILE_ATTRIBUTES {
            Ok(())
        } else {
            Err(format!("tombstoned file still has attributes: {a:#x}"))
        }
    });

    // 8. Directory enumeration -> merged: adds present, tombstone hidden.
    check("enum_merge", &mut out, &mut all_ok, || {
        let mut names: Vec<String> = std::fs::read_dir(root)
            .map_err(|e| e.to_string())?
            .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        names.sort();
        let want_present =
            ["mod_added.txt", "override.txt", "real_only.txt", "virtual_dir", "real_dir"];
        for w in want_present {
            if !names.iter().any(|n| n == w) {
                return Err(format!("missing {w} in {names:?}"));
            }
        }
        if names.iter().any(|n| n == "deleted.txt") {
            return Err(format!("tombstone leaked into listing: {names:?}"));
        }
        Ok(())
    });

    // 9. Handle identity -> a redirected virtual file reports its virtual path.
    check("identity", &mut out, &mut all_ok, || {
        let p = final_path(&root.join("mod_added.txt"))?;
        let lp = p.to_lowercase();
        if !lp.contains("mod_added.txt") {
            return Err(format!("final path lost virtual name: {p}"));
        }
        Ok(())
    });

    // 10. Load a VIRTUAL DLL (mod plugin) -> maps from the backing image and its
    // export resolves. Proves DLL/image loading is virtualized (open-redirect +
    // attr hooks; the loader builds the section from the redirected handle).
    check("dll_load", &mut out, &mut all_ok, || {
        let dll = root.join("plugin.dll");
        let wide: Vec<u16> =
            dll.to_str().unwrap().encode_utf16().chain(std::iter::once(0)).collect();
        let module = unsafe { LoadLibraryW(wide.as_ptr()) };
        if module.is_null() {
            return Err("LoadLibraryW(virtual plugin.dll) returned null".into());
        }
        // The backing is version.dll; this export must be present.
        let export = b"GetFileVersionInfoSizeW\0";
        if unsafe { GetProcAddress(module, export.as_ptr()) }.is_none() {
            return Err("export GetFileVersionInfoSizeW not found in loaded module".into());
        }
        Ok(())
    });

    // 11. Child-process propagation -> a spawned child inherits the VFS and can
    // read a virtual file (only possible if the shim injected itself into it).
    check("child_process", &mut out, &mut all_ok, || {
        let child_out = format!("{report}.child");
        let _ = std::fs::remove_file(&child_out);
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let status = std::process::Command::new(exe)
            .args(["--child", root.to_str().unwrap(), &child_out])
            .status()
            .map_err(|e| format!("spawn child: {e}"))?;
        if !status.success() {
            return Err(format!("child exit: {status:?}"));
        }
        let got = std::fs::read(&child_out).map_err(|e| format!("read child output: {e}"))?;
        expect_eq(&got, b"MOD-ADDED-BYTES")
    });

    std::fs::write(report, out.as_bytes()).expect("write report");
    std::process::exit(if all_ok { 0 } else { 1 });
}
