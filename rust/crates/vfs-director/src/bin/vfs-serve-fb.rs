//! `vfs-serve-fb`: a native **Linux** Director serving one file over a
//! file-backed ring, for a Windows shim running under Wine.
//!
//! Usage: `vfs-serve-fb <ring-path> <root-dir> <backing-file>`
//!
//! This is the server half of the increment's definition of done. The client
//! half is `shim-ring-harness`'s `shim-ring-client`, which runs under Proton's
//! Wine with the **real** `vfs-shim` hooks installed and reads
//! `C:\<root>\data\hello.txt` — a path that exists on no filesystem the Wine
//! process can see. The bytes come from `<backing-file>`, which only this
//! process can open.
//!
//! **The two numbers this prints are load-bearing, not decoration.** The shim
//! client maps `VFS_RING_BYTES` bytes of the ring file and defaults that to
//! 2 MiB, while this server sizes the file at whatever `start_file_backed`
//! computes (~34 MiB with the default arena). A file *shorter* than the
//! requested mapping errors cleanly; mapping **too little is silent** —
//! `ring::open` only needs the header, which is at offset 0, so the client
//! attaches happily and the bulk arena then falls outside its view. So the
//! values are printed rather than assumed, and the driver exports exactly
//! these to the Wine side.
//!
//! Every worker spins (see `IpcServe::start_file_backed`), so this process
//! burns four cores for as long as it runs. Bound it with `timeout` and kill
//! it when the client is done.

#[cfg(unix)]
mod imp {
    use std::collections::HashMap;
    use std::fs::File;
    use std::io::Write as _;
    use std::os::unix::fs::FileExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use vfs_director::ops::{
        map_io_err, not_found, Capabilities, DirEntry, Handle, Provider, SetAttr, Stat, VPath,
        KIND_DIR, KIND_FILE,
    };
    use vfs_director::{Director, DiskProvider, IpcServe, RootId};

    /// The one virtual entry, and the vpath the Wine-side shim will ask for
    /// once it has folded `C:\<root>\data\hello.txt` against its root map.
    const VPATH: &str = "data/hello.txt";
    /// Ring payload capacity. Small on purpose: the fixture's file is a few
    /// dozen bytes, so a 4 KiB cap keeps the read inline and legible, and the
    /// arena is still fully mapped by the client because `VFS_RING_BYTES` is
    /// exported from `map_bytes` below.
    const PAYLOAD_CAP: u32 = 4096;

    /// Handles this provider issued itself carry the top bit; everything else
    /// belongs to the wrapped [`DiskProvider`], whose ids start at 1. Without
    /// the split, a `close`/`read_at` on a disk handle could be answered from
    /// the wrong table.
    const VIRT_BIT: u64 = 1 << 63;

    fn log(line: &str) {
        println!("SERVE: {line}");
        let _ = std::io::stdout().flush();
    }

    fn fail(msg: &str) -> ! {
        eprintln!("SERVE FAIL: {msg}");
        let _ = std::io::stderr().flush();
        std::process::exit(2);
    }

    /// One virtual file over an arbitrary backing path, everything else
    /// delegated to a [`DiskProvider`] rooted at `root-dir`.
    ///
    /// The delegation is what makes `<root-dir>` mean something: the shim
    /// probes the root directory and `data\` on its way to the file, and those
    /// answers come from real disk. Only `data/hello.txt` is synthetic, and it
    /// resolves to a file *outside* the served tree — which is the point. A
    /// `DiskProvider` alone could not express that, since it maps a vpath
    /// straight onto `root/<vpath>`.
    struct OneEntry {
        disk: DiskProvider,
        backing: PathBuf,
        opens: Mutex<HashMap<u64, File>>,
        next: AtomicU64,
    }

    impl OneEntry {
        fn new(root: &Path, backing: PathBuf) -> Self {
            OneEntry {
                disk: DiskProvider::new(root),
                backing,
                opens: Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
            }
        }

        /// The shim folds a vpath before it reaches the ring, and a host-side
        /// caller does not — so compare with `vfs_core::fold`, the workspace's
        /// one definition of fold-equal, rather than `to_ascii_lowercase`.
        fn is_virtual(rel: &str) -> bool {
            vfs_core::fold(rel) == VPATH
        }

        fn is_data_dir(rel: &str) -> bool {
            vfs_core::fold(rel) == "data"
        }

        fn backing_stat(&self) -> Result<Stat, i32> {
            let md = std::fs::metadata(&self.backing).map_err(|_| not_found())?;
            Ok(Stat {
                kind: KIND_FILE,
                size: md.len(),
                mtime: 0,
            })
        }
    }

    const DIR_STAT: Stat = Stat {
        kind: KIND_DIR,
        size: 0,
        mtime: 0,
    };

    impl Provider for OneEntry {
        fn capabilities(&self) -> Capabilities {
            Capabilities::read_only()
        }

        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            if Self::is_virtual(p.rel) {
                let st = self.backing_stat()?;
                log(&format!("GETATTR {} -> file size={}", p.rel, st.size));
                return Ok(Some(st));
            }
            // The parent directory of the virtual entry exists whether or not
            // `root-dir/data` does: a shim that cannot stat `data\` may never
            // ask about the file under it.
            if p.rel.is_empty() || Self::is_data_dir(p.rel) {
                log(&format!("GETATTR {:?} -> dir", p.rel));
                return Ok(Some(DIR_STAT));
            }
            let r = self.disk.getattr(p);
            log(&format!(
                "GETATTR {} -> disk {}",
                p.rel,
                match &r {
                    Ok(Some(s)) => format!("found kind={}", s.kind),
                    Ok(None) => "absent".to_string(),
                    Err(e) => format!("err {e}"),
                }
            ));
            r
        }

        fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
            let mut out = self.disk.readdir(p).unwrap_or_default();
            if p.rel.is_empty() && !out.iter().any(|e| Self::is_data_dir(&e.name)) {
                out.push(DirEntry {
                    name: "data".to_string(),
                    stat: DIR_STAT,
                });
            }
            if Self::is_data_dir(p.rel) {
                out.push(DirEntry {
                    name: "hello.txt".to_string(),
                    stat: self.backing_stat()?,
                });
            }
            log(&format!("READDIR {:?} -> {} entries", p.rel, out.len()));
            Ok(out)
        }

        fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
            if !Self::is_virtual(p.rel) {
                let r = self.disk.open(p, flags);
                log(&format!("OPEN {} -> disk {:?}", p.rel, r.as_ref().map(|x| x.0)));
                return r;
            }
            let f = File::open(&self.backing).map_err(|_| not_found())?;
            let size = f.metadata().map_err(|_| map_io_err())?.len();
            let h = self.next.fetch_add(1, Ordering::Relaxed) | VIRT_BIT;
            self.opens.lock().map_err(|_| map_io_err())?.insert(h, f);
            log(&format!(
                "OPEN {} -> virtual fh={h:#x} size={size} from {}",
                p.rel,
                self.backing.display()
            ));
            Ok((h, size, false))
        }

        fn close(&self, h: Handle) -> Result<(), i32> {
            if h & VIRT_BIT == 0 {
                return self.disk.close(h);
            }
            self.opens.lock().map_err(|_| map_io_err())?.remove(&h);
            log(&format!("CLOSE fh={h:#x}"));
            Ok(())
        }

        fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            if h & VIRT_BIT == 0 {
                return self.disk.read_at(h, offset, buf);
            }
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let f = g.get(&h).ok_or_else(not_found)?;
            let n = f.read_at(buf, offset).map_err(|_| map_io_err())?;
            log(&format!(
                "READ fh={h:#x} offset={offset} want={} -> {n} bytes",
                buf.len()
            ));
            Ok(n)
        }

        fn set_attr(&self, _p: VPath, _a: SetAttr) -> Result<(), i32> {
            Err(vfs_director::ops::not_supported())
        }
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        if args.len() != 4 {
            fail("usage: vfs-serve-fb <ring-path> <root-dir> <backing-file>");
        }
        let ring = PathBuf::from(&args[1]);
        let root = PathBuf::from(&args[2]);
        let backing = PathBuf::from(&args[3]);
        if !backing.is_file() {
            fail(&format!("backing file {} does not exist", backing.display()));
        }
        if !root.is_dir() {
            fail(&format!("root dir {} does not exist", root.display()));
        }

        let kernel = Arc::new(Director::new());
        kernel
            .mount(RootId::DEFAULT, Arc::new(OneEntry::new(&root, backing.clone())))
            .unwrap_or_else(|e| fail(&format!("mount: {e}")));

        let serve = IpcServe::start_file_backed(kernel, &ring, PAYLOAD_CAP)
            .unwrap_or_else(|e| fail(&format!("start_file_backed: {e}")));

        // **Print, do not assume.** A client that maps fewer bytes than
        // `map_bytes` still opens the ring — the header is at offset 0 — and
        // then reads its bulk arena outside the mapping. So the driver exports
        // these exact numbers, and the client's own log echoes them back.
        log(&format!("ready {}", ring.display()));
        log(&format!(
            "VFS_RING_BYTES={} VFS_ARENA_LEN={} VFS_ARENA_OFFSET={} VFS_RING_PAYLOAD_CAP={}",
            serve.map_bytes, serve.arena_len, serve.arena_offset, serve.payload_cap
        ));
        log(&format!(
            "file size on disk = {} bytes",
            std::fs::metadata(&ring).map(|m| m.len()).unwrap_or(0)
        ));
        log(&format!("serving {VPATH} from {}", backing.display()));

        // Run until killed. The workers spin in their own threads; parking
        // here costs nothing and keeps `serve` alive.
        loop {
            std::thread::park();
        }
    }
}

#[cfg(unix)]
fn main() {
    imp::main();
}

// The file-backed ring is `#[cfg(unix)]` in `vfs-director` (it is `mmap` over a
// real file), so there is nothing for this binary to do on Windows. A `main`
// that exits nonzero rather than an empty one: a Windows caller running this by
// mistake should be told, not silently succeed.
#[cfg(not(unix))]
fn main() {
    eprintln!("vfs-serve-fb: the file-backed ring server is unix-only");
    std::process::exit(2);
}
