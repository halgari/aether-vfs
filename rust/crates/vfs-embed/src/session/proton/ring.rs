//! The file-backed ring's home: a tmpfs under `$XDG_RUNTIME_DIR` when there is
//! a private one, so the ring's pages never reach disk.

use std::path::{Path, PathBuf};

/// Whether `dir` is on a filesystem whose pages are never written to disk.
///
/// Read from `/proc/self/mountinfo`: the mount whose mount point is the
/// longest prefix of `dir` is the one `dir` is on. A directory this cannot
/// place is reported as not in memory, which only costs the caller the
/// optimisation.
#[cfg(unix)]
fn is_memory_fs(dir: &Path) -> bool {
    let Ok(dir) = dir.canonicalize() else {
        return false;
    };
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    memory_fs_in(&mounts, &dir)
}

/// The parsing half of [`is_memory_fs`], over the text of a `mountinfo`.
#[cfg(unix)]
fn memory_fs_in(mountinfo: &str, dir: &Path) -> bool {
    let mut best: Option<(usize, bool)> = None;
    for line in mountinfo.lines() {
        // `id parent maj:min root MOUNTPOINT opts [optional…] - FSTYPE source superopts`
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let Some(point) = left.split(' ').nth(4) else {
            continue;
        };
        // The kernel writes a space in a path as `\040`.
        let point = point.replace("\\040", " ");
        if !dir.starts_with(&point) {
            continue;
        }
        let fstype = right.split(' ').next().unwrap_or("");
        let in_memory = matches!(fstype, "tmpfs" | "ramfs");
        // Later lines win a tie: a mount over the same point shadows the
        // earlier one.
        if best.is_none_or(|(len, _)| point.len() >= len) {
            best = Some((point.len(), in_memory));
        }
    }
    best.is_some_and(|(_, in_memory)| in_memory)
}

/// Whether `dir` is this user's alone: owned by the user this process runs
/// as, with no access for group or others.
///
/// The effective uid is read as the owner of `/proc/self`, which the kernel
/// reports as exactly that; this crate has no `libc` to ask with.
#[cfg(unix)]
fn is_private_dir(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(d), Ok(me)) = (
        std::fs::symlink_metadata(dir),
        std::fs::metadata("/proc/self"),
    ) else {
        return false;
    };
    d.is_dir() && d.uid() == me.uid() && d.mode() & 0o077 == 0
}

/// Give the ring a home in memory, and make `named` a symlink to it.
///
/// Returns the ring's real path — `$XDG_RUNTIME_DIR/aether-vfs/ring-<id>/` +
/// the same file name as `named`, so the name a Wine child is given does not
/// change — or `None` when the ring should be created at `named` itself:
/// there is no `$XDG_RUNTIME_DIR`, it is not a memory filesystem, it is not
/// private to this user, or anything about setting the file and the link up
/// failed.
///
/// **The directory is checked, not trusted.** `$XDG_RUNTIME_DIR` is the
/// user's own and mode 0700 by specification, but it is only an environment
/// variable: set to `/tmp` (a tmpfs on many systems, and what some sessions
/// without logind do) it is a place where another user can create
/// `aether-vfs` first, own the directory the ring is about to be made in,
/// and swap the file. So both it and `aether-vfs` must be owned by this
/// user with no access for anyone else, and the ring file is created
/// exclusively (never opened if it exists, never through a link) with mode
/// 0600. `/dev/shm` is not considered at all.
///
/// The directory is named for the *canonical* `state_dir` — two processes
/// that spell one state directory differently must share a ring name, and
/// two that spell different ones alike (the same relative path from
/// different working directories) must not. So a session that died without
/// [`Session::stop_serve`] has its file replaced by the next one on the
/// same state directory rather than left to accumulate.
///
/// The file is sparse: its pages are taken from the tmpfs as they are first
/// touched, as they were from the disk before. A runtime directory with less
/// than a ring's worth of room left (64 MiB; the usual quota is a tenth of
/// RAM) can therefore fault the director or the game when it fills. Reserving
/// the pages up front needs `posix_fallocate`, which this crate cannot call
/// without `libc`; not done.
#[cfg(unix)]
pub(super) fn ring_in_memory(state_dir: &Path, named: &Path) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    if !runtime.is_absolute() || !is_memory_fs(&runtime) || !is_private_dir(&runtime) {
        return None;
    }
    let ours = runtime.join("aether-vfs");
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&ours);
    if !is_private_dir(&ours) {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    state_dir.canonicalize().ok()?.hash(&mut h);
    let dir = ours.join(format!("ring-{:016x}", h.finish()));
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    if !is_private_dir(&dir) {
        return None;
    }
    let file = dir.join(named.file_name()?);
    // The same fresh-inode rule as the file in `state_dir`: see `serve`.
    let _ = std::fs::remove_file(&file);
    let made = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
        .is_ok()
        && std::os::unix::fs::symlink(&file, named).is_ok();
    if !made {
        remove_memory_ring(Some(&file), named);
        return None;
    }
    Some(file)
}

/// Undo [`ring_in_memory`]: the file, its directory, and the link at `named`
/// if it still points at that file. Best effort.
#[cfg(unix)]
pub(super) fn remove_memory_ring(backing: Option<&Path>, named: &Path) {
    let Some(file) = backing else {
        return;
    };
    let _ = std::fs::remove_file(file);
    if let Some(dir) = file.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    if std::fs::read_link(named).is_ok_and(|to| to == file) {
        let _ = std::fs::remove_file(named);
    }
}

#[cfg(all(test, unix))]
mod ring_location_tests {
    use super::super::RING_FILE;
    use super::*;
    use crate::Session;

    /// A trimmed `mountinfo`: a disk root, a tmpfs under it, a disk mount
    /// under that tmpfs, and a mount point with a space in its name.
    const MOUNTS: &str = "\
25 1 0:23 / / rw,relatime shared:1 - btrfs /dev/mapper/root rw,compress=zstd:3
30 25 0:27 / /run rw,nosuid shared:2 - tmpfs tmpfs rw,mode=755
61 30 0:52 / /run/user/1000 rw,nosuid,nodev shared:9 - tmpfs tmpfs rw,size=9646860k
70 61 8:1 / /run/user/1000/disk rw - ext4 /dev/sda1 rw
80 25 0:60 / /mnt/my\\040ram rw - ramfs none rw
";

    #[test]
    fn a_directory_is_in_memory_only_if_its_innermost_mount_is() {
        let on = |p: &str| memory_fs_in(MOUNTS, Path::new(p));
        assert!(on("/run/user/1000"));
        assert!(on("/run/user/1000/aether-vfs/ring-1"));
        assert!(on("/run/lock"));
        assert!(on("/mnt/my ram/x"), "an escaped space in a mount point");
        assert!(!on("/home/me/state"), "the disk root");
        assert!(
            !on("/run/user/1000/disk/x"),
            "a disk mounted inside a tmpfs is a disk"
        );
        // A sibling whose name merely starts the same is not under that
        // mount: it is on the tmpfs around it.
        assert!(on("/run/user/1000/disk2/x"));
        assert!(!memory_fs_in("", Path::new("/run")), "no table, no claim");
    }

    /// A runtime directory is used only if it is this user's and nobody
    /// else's. A directory anyone can write to — what `XDG_RUNTIME_DIR=/tmp`
    /// would be — is refused, and so is one with any group or other access.
    #[test]
    fn only_a_directory_private_to_this_user_may_hold_the_ring() {
        use std::os::unix::fs::PermissionsExt;
        let base = crate::test_scratch::scratch_created("ringpriv");
        let set = |mode: u32| {
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        set(0o700);
        assert!(is_private_dir(&base));
        for open in [0o1777, 0o755, 0o750, 0o701] {
            set(open);
            assert!(!is_private_dir(&base), "mode {open:o} is not private");
        }
        set(0o700);
        // Not ours: the root directory belongs to root (unless we are root).
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata("/proc/self").unwrap().uid() != 0 {
            assert!(!is_private_dir(Path::new("/")));
        }
        // A link to a private directory is not a directory of ours.
        let link = base.join("link");
        std::os::unix::fs::symlink(&base, &link).unwrap();
        assert!(!is_private_dir(&link));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The ring's directory is named for where the state directory *is*, not
    /// for how its path was spelled: one directory spelled two ways gets one
    /// ring name, so a second session there replaces the first's file.
    #[test]
    fn the_ring_directory_is_named_for_the_canonical_state_directory() {
        let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) else {
            return;
        };
        if !is_memory_fs(&runtime) || !is_private_dir(&runtime) {
            return;
        }
        let base = crate::test_scratch::scratch_dir("ringname");
        let state = base.join("state");
        std::fs::create_dir_all(state.join("sub")).unwrap();
        let direct = ring_in_memory(&state, &state.join("a.bin")).unwrap();
        let roundabout =
            ring_in_memory(&state.join("sub").join(".."), &state.join("b.bin")).unwrap();
        assert_eq!(direct.parent(), roundabout.parent());
        let other = ring_in_memory(&state.join("sub"), &state.join("c.bin")).unwrap();
        assert_ne!(direct.parent(), other.parent());
        remove_memory_ring(Some(&direct), &state.join("a.bin"));
        remove_memory_ring(Some(&roundabout), &state.join("b.bin"));
        remove_memory_ring(Some(&other), &state.join("c.bin"));
        assert!(!direct.parent().unwrap().exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `serve` on a host with a tmpfs `$XDG_RUNTIME_DIR` (any desktop Linux;
    /// skipped elsewhere): the ring's pages are in memory, the name in
    /// `state_dir` still opens it, and `stop_serve` leaves nothing behind.
    #[test]
    fn the_ring_is_created_in_memory_and_named_in_the_state_dir() {
        let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) else {
            return;
        };
        if !is_memory_fs(&runtime) {
            return;
        }
        let base = crate::test_scratch::scratch_dir("ringloc");
        let mut s = Session::new();
        s.set_root(base.join("root"));
        s.set_overlay(base.join("overlay"));
        s.set_state_dir(base.join("state"));
        s.serve().unwrap();

        let named = base.join("state").join(RING_FILE);
        let real = s.ipc().unwrap().ring_path().unwrap().to_path_buf();
        assert!(
            real.starts_with(&runtime),
            "{} is not in memory",
            real.display()
        );
        assert_eq!(
            real.file_name(),
            named.file_name(),
            "launch names the ring to the child by this file name under the state directory"
        );
        assert_eq!(std::fs::read_link(&named).unwrap(), real);
        // What the child does: open the name in the state directory and find
        // this ring there, whole.
        let len = std::fs::metadata(&named).unwrap().len() as usize;
        assert_eq!(len, s.ipc().unwrap().map_bytes);

        // Serving again while serving changes nothing.
        s.serve().unwrap();
        assert_eq!(s.ipc().unwrap().ring_path().unwrap(), real);

        // Nobody else can read the ring or put another in its place.
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&real).unwrap().mode() & 0o777, 0o600);
        assert!(is_private_dir(real.parent().unwrap()));
        assert!(is_private_dir(&runtime.join("aether-vfs")));

        s.stop_serve();
        assert!(!real.exists(), "the ring file must not outlive its session");
        assert!(!real.parent().unwrap().exists());
        assert!(std::fs::symlink_metadata(&named).is_err(), "nor its name");

        // And a second serve of the same session gets a ring again.
        s.serve().unwrap();
        assert!(named.exists());
        drop(s);
        assert!(!real.exists(), "dropping the session stops serving");
        let _ = std::fs::remove_dir_all(&base);
    }
}
