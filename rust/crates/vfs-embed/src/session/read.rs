//! Host-side reads out of a session's composed graph.

use vfs_provider::{DirEntry, OPEN_READ, RootId, Stat};

use vfs_director::Director;

use super::Session;

impl Session {
    /// Every write this host has refused because no `ReadWrite` provider
    /// served that path, as `(path, count)` — spec §7's discovery workflow:
    /// launch, ask what was rejected, add an overlay for those subtrees.
    ///
    /// **Process-wide, despite being a method.** `vfs_director::io_stats`
    /// keeps one global table with no session or root dimension, so with two
    /// live sessions in one host each reports the other's rejections. Left
    /// that way deliberately rather than faked per-session: the counters are
    /// recorded deep in the director's open path, and giving them a session
    /// dimension is a change to that path, not to this accessor. See the
    /// free-function form, [`crate::rejected_writes`].
    pub fn rejected_writes(&self) -> Vec<(String, u64)> {
        vfs_compose::rejected_writes()
    }

    /// Occasional host-side full-file read (not the primary API).
    ///
    /// Root 0's convenience form of [`Session::read_file_at`].
    pub fn read_file(&self, vpath: &str) -> Result<Vec<u8>, i32> {
        self.read_file_at(RootId::DEFAULT, vpath)
    }

    /// Occasional host-side full-file read out of `root`'s graph.
    ///
    /// This takes a root because the spec's own example needs one: §8 mounts the
    /// INI provider on **root 1** and finishes by reading back what the game
    /// wrote to it. Until this existed, `read_file` hardcoded
    /// [`RootId::DEFAULT`] while [`Director::readdir`] already took a root, so a
    /// host could *list* a second root's graph and never read a byte out of it —
    /// the round trip that `memory()` exists for was reachable only by launching
    /// something and having the child copy the file out to real disk.
    ///
    /// [`Director::readdir`]: vfs_director::Director::readdir
    pub fn read_file_at(&self, root: RootId, vpath: &str) -> Result<Vec<u8>, i32> {
        read_whole(&self.kernel, root, vpath)
    }

    /// List a directory in `root`'s graph, host-side.
    ///
    /// The companion to [`Session::read_file_at`], and the reason it is here
    /// rather than in each host is that a host should not have to reach past
    /// the seam for it: listing and stat-ing a graph through `session.kernel()`
    /// is the same two questions every host asks. This crate's own doc says
    /// "if a host has to reach past this crate, the fix belongs here".
    ///
    /// It is not a convenience. Two of spec §6's rules are statements about
    /// `readdir` and nothing else can check them from a host: `layered`
    /// **unions** its children's listings with top-wins per name, while
    /// `router`'s listing is **single-dispatch** rather than the union §6
    /// specifies, so a file served by a route is readable by name and absent
    /// from its own directory. A host that cannot list its graph cannot tell
    /// those apart, and the second is a silent wrong answer.
    ///
    /// Drives the graph on the calling thread, like `read_file_at`. For a host
    /// whose provider is serviced by that same thread's event loop that is what
    /// trips the binding's deadlock guard — deliberately, because the failure is
    /// then reported instead of hanging.
    pub fn readdir(&self, root: RootId, vpath: &str) -> Result<Vec<DirEntry>, i32> {
        self.kernel.readdir(root, vpath)
    }

    /// Stat one path in `root`'s graph, host-side. `Ok(None)` is "the graph does
    /// not serve it", which is not an error.
    ///
    /// Same reason as [`Session::readdir`]: it was reached for through
    /// `kernel()` by two hosts. It is the cheapest way to answer the question
    /// this project keeps needing answered — *does my graph actually serve the
    /// path I think it does* — without opening anything. A host can use it for
    /// precisely that before it stages a launch image.
    pub fn getattr(&self, root: RootId, vpath: &str) -> Result<Option<Stat>, i32> {
        self.kernel.getattr(root, vpath)
    }
}

/// Reads the whole file at `vpath` in `root` through the director. The one
/// open/read-loop/close, shared by [`Session::read_file_at`] and the staging
/// source. A directory is `is_dir`; the handle is closed on every path.
pub(super) fn read_whole(kernel: &Director, root: RootId, vpath: &str) -> Result<Vec<u8>, i32> {
    let (fh, size, is_dir) = kernel.open(root, vpath, OPEN_READ)?;
    if is_dir {
        let _ = kernel.close(fh);
        return Err(vfs_provider::is_dir());
    }
    let mut buf = vec![0u8; size as usize];
    let mut off = 0usize;
    while off < buf.len() {
        match kernel.read(fh, off as u64, &mut buf[off..]) {
            Ok(0) => break,
            Ok(n) => off += n,
            Err(st) => {
                let _ = kernel.close(fh);
                return Err(st);
            }
        }
    }
    let _ = kernel.close(fh);
    buf.truncate(off);
    Ok(buf)
}
