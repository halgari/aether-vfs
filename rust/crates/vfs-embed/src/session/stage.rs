//! Staging: writing graph-only launch images and their import closure to real
//! disk, and resolving what a launch names.

#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use vfs_director::stage::{ImageSource, stage_launch_into};
use vfs_director::{Director, DiskProvider};
use vfs_provider::{Provider, RootId};

#[cfg(doc)]
use vfs_director::stage::StagedDir;

use super::read::read_whole;
use super::{LaunchOpts, Session, StageOpts};
use crate::image::{self, ImageTarget};

/// Reads whole files out of a session's own composed graph, for
/// [`vfs_director::stage`]. Root 0: staging always concerns the launched
/// image, which lives in the game-directory root.
/// The sole constructor is `launch`'s staging step, on both targets.
/// `stage_launch` stays portable — it takes any `&dyn ImageSource` a host
/// supplies.
struct KernelSource(Arc<Director>);

impl ImageSource for KernelSource {
    fn read(&self, vpath: &str) -> Option<Vec<u8>> {
        read_whole(&self.0, RootId::DEFAULT, vpath).ok()
    }
}

impl Session {
    /// Stage `opts.exe_vpath` out of this session's provider graph onto real
    /// disk, with its PE import closure, and mount the staging directory back
    /// into the graph **underneath** everything else. Returns the staged
    /// image's absolute path — what `CreateProcess` needs.
    ///
    /// [`Session::launch`] calls this for you when a relative image is graph
    /// content; call it directly only when you need to seed staging from
    /// something that is *not* this session's graph, or to stage extra images
    /// before a launch.
    ///
    /// Three things happen that a host would otherwise have to know to do:
    ///
    /// * The bytes land in `state_dir/stage`, under a per-launch tag, and the
    ///   resulting [`StagedDir`] is **held by the session** — its `Drop`
    ///   removes the directory, and Windows keeps the image mapped for as long
    ///   as the child runs, so a host-held handle is a race waiting to be lost.
    /// * The staging directory is mounted back, so the same file is answerable
    ///   through `getattr`/`open` at its vpath afterwards and not merely
    ///   reachable by the literal path `CreateProcess` used. Once the managed
    ///   root is fully virtual, a real file under it that no provider serves is
    ///   invisible.
    /// * It is mounted **below** the host's own mounts. See
    ///   `RootComposition::staging` — a staged copy outranking curated
    ///   content is a silent wrong answer on exactly the paths staging touches.
    ///
    /// Staging again replaces the previous directory (and deletes it), the same
    /// way a relaunch did in the daemon.
    pub fn stage_launch(
        &self,
        source: &dyn ImageSource,
        opts: &StageOpts,
    ) -> Result<PathBuf, String> {
        // Into the virtual root, at the image's own vpath — not a sibling
        // staging directory. A staged EXE outside the root drags everything
        // the process resolves relative to its own module path out of the
        // VFS with it, which is what stopped Cyberpunk 2077 and Stardew
        // Valley booting while Skyrim (EXE at the root, content found via
        // cwd) was unaffected.
        let staged = stage_launch_into(
            source,
            opts.exe_vpath,
            opts.also,
            &self.virtual_root,
            opts.fallback_dirs,
        )?;
        let disk: Arc<dyn Provider> = Arc::new(DiskProvider::new(staged.dir()));
        {
            let mut roots = self
                .roots
                .lock()
                .map_err(|_| "session roots lock poisoned".to_string())?;
            self.claim(&mut roots, RootId::DEFAULT)
                .map_err(|st| format!("mount staging: status {st}"))?
                .staging = Some(disk);
        }
        self.recompose(RootId::DEFAULT)
            .map_err(|st| format!("mount staging: status {st}"))?;

        let exe = staged.exe().to_path_buf();
        *self
            .staged
            .lock()
            .map_err(|_| "staged-dir lock poisoned".to_string())? = Some(staged);
        Ok(exe)
    }

    /// Proxy DLLs the last staging found beside its images
    /// ([`vfs_director::stage::PROXY_DLL_NAMES`], lower case) — empty when
    /// nothing has been staged.
    ///
    /// For a Wine/Proton host: Wine prefers its builtin for most of these
    /// names, so a staged native `dinput8.dll` or `version.dll` loads only if
    /// `WINEDLLOVERRIDES` names it `n,b`. Stage first
    /// ([`Session::stage_launch`]), read this, then build the child's
    /// environment.
    pub fn staged_proxies(&self) -> Vec<String> {
        self.staged
            .lock()
            .ok()
            .and_then(|s| s.as_ref().map(|d| d.proxies().to_vec()))
            .unwrap_or_default()
    }

    /// Where `opts.image` points, staged if need be: the one resolver both
    /// `launch` bodies share. The three forms are
    /// [`crate::image::classify_image`]'s, against [`Session::root_locations`]:
    ///
    /// - **In a root** at a vpath: a real file in that root's backing
    ///   directory is used as is; failing that, a vpath the root's provider
    ///   graph serves is **staged** into root 0's backing directory (root 0
    ///   only — another root's graph-only image is refused by name); failing
    ///   that, refused.
    /// - **Outside every root**: launched as given. On unix only a `C:\…`
    ///   form can be given — a host path has no drive in the prefix.
    ///
    /// On unix an absolute **host** path is first rewritten to its remainder
    /// under `virtual_root` (an accepted form before roots had locations), or
    /// refused if it is not under it.
    pub(super) fn resolve_launch_image(&self, opts: &LaunchOpts) -> Result<ResolvedImage, String> {
        #[cfg(unix)]
        let image: String = {
            let host = Path::new(&opts.image);
            if host.is_absolute() {
                let rel = host
                    .strip_prefix(&self.virtual_root)
                    .map_err(|_| no_drive_names(&opts.image))?;
                rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            } else {
                opts.image.clone()
            }
        };
        #[cfg(not(unix))]
        let image: String = opts.image.clone();

        match image::classify_image(&image, &self.root_locations())? {
            ImageTarget::Outside(p) => {
                #[cfg(unix)]
                if !image::is_windows_absolute(&p) {
                    return Err(no_drive_names(&p));
                }
                Ok(ResolvedImage::Outside(p))
            }
            ImageTarget::InRoot { root, vpath } => {
                // A component with a drive (`C:foo.exe`, drive-relative, which
                // `classify_image` calls relative) would make `Path::join`
                // replace the whole base on Windows and escape the root.
                if vpath.split('/').any(|c| c.contains(':')) {
                    return Err(format!(
                        "launch: {:?} has a path component containing ':' (a drive-relative \
                         name); name the image by a plain path inside a root, or give it as \
                         an absolute path",
                        opts.image
                    ));
                }
                let base = self
                    .root_backing_dir(root)
                    .ok_or_else(|| format!("launch: root {root} has no backing directory"))?;
                let host = vpath.split('/').fold(base, |p, c| p.join(c));
                if host.is_file() {
                    return Ok(ResolvedImage::in_root(root, vpath, host));
                }
                let served = self
                    .kernel
                    .getattr(RootId(root), &vpath)
                    .ok()
                    .flatten()
                    .is_some();
                if !served {
                    return Err(format!(
                        "launch: {:?} resolves to root {root} vpath {vpath:?}, which is neither \
                         a real file at {} nor served by that root, so there is nothing to \
                         stage",
                        opts.image,
                        host.display()
                    ));
                }
                if root != 0 {
                    return Err(format!(
                        "launch: {vpath:?} is served by root {root}'s provider graph but is not \
                         a real file, and staging is root 0 only — put the program in root 0 \
                         or on disk"
                    ));
                }
                // VFS content. Write it (and its import closure) out, mount
                // the staging directory back under the curated graph, and
                // launch the real file that produces.
                let also: Vec<&str> = opts.stage_also.iter().map(String::as_str).collect();
                let host = self
                    .stage_launch(
                        &KernelSource(Arc::clone(&self.kernel)),
                        &StageOpts {
                            exe_vpath: &vpath,
                            also: &also,
                            fallback_dirs: &opts.stage_fallback_dirs,
                        },
                    )
                    .map_err(|e| format!("launch: staging {vpath:?}: {e}"))?;
                Ok(ResolvedImage::in_root(root, vpath, host))
            }
        }
    }

    /// [`Session::resolve_launch_image`]'s host path, for unit tests of the
    /// in-root forms.
    #[cfg(test)]
    fn resolve_for_test(&self, opts: &LaunchOpts) -> Result<PathBuf, String> {
        match self.resolve_launch_image(opts)? {
            ResolvedImage::InRoot { host, .. } => Ok(host),
            ResolvedImage::Outside(p) => Ok(PathBuf::from(p)),
        }
    }
}

/// What [`Session::resolve_launch_image`] resolved an image to, with only what
/// each target launches from: unix names the child's image by `root`'s location
/// and `vpath`, Windows launches the real file `host`. (Tests read `host` on
/// both.)
#[derive(Debug)]
pub(super) enum ResolvedImage {
    /// Inside `root`'s location at `vpath`; `host` is the real file backing
    /// it — already there, or just staged into root 0's backing directory.
    InRoot {
        #[cfg(unix)]
        root: u32,
        #[cfg(unix)]
        vpath: String,
        #[cfg(any(windows, test))]
        host: PathBuf,
    },
    /// Outside every root: a real program, launched as given.
    Outside(String),
}

impl ResolvedImage {
    fn in_root(_root: u32, _vpath: String, _host: PathBuf) -> Self {
        ResolvedImage::InRoot {
            #[cfg(unix)]
            root: _root,
            #[cfg(unix)]
            vpath: _vpath,
            #[cfg(any(windows, test))]
            host: _host,
        }
    }
}

/// The refusal for a unix host path no Wine drive names.
#[cfg(unix)]
fn no_drive_names(p: &str) -> String {
    format!(
        "launch: {p} is a host path outside every root, so no drive in this session's Wine \
         prefix names it. Give it as the program sees it (C:\\...) or put it in a root."
    )
}

/// Protocol golden `empty-tree-snapshot`: a single empty root directory (the
/// "SSFV" header, version 1, and one empty root node), 128 bytes.
///
/// `shim.cfg` carries a tree snapshot, and `Engine::build` rejects zero-length
/// snapshot bytes, so both targets' `launch` write this one.
const EMPTY_TREE_SNAPSHOT: [u8; 128] = [
    0x53, 0x53, 0x46, 0x56, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x30, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

pub(super) fn empty_tree_snapshot() -> Vec<u8> {
    EMPTY_TREE_SNAPSHOT.to_vec()
}

// The golden is consumed by a `shim.cfg` write on both targets now, so the
// constant, the two helpers that decode it and this test are all portable.
#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn empty_tree_snapshot_is_valid_header() {
        let snap = empty_tree_snapshot();
        assert_eq!(snap.len(), 128);
        // MAGIC "SSFV" little-endian = 0x5646_5353
        assert_eq!(&snap[0..4], &[0x53, 0x53, 0x46, 0x56]);
        assert_eq!(u32::from_le_bytes(snap[4..8].try_into().unwrap()), 1);
    }
}

#[cfg(test)]
mod launch_image_tests {
    use super::*;
    use vfs_director::DiskProvider;

    /// Minimal PE32+ with no imports — staging parses the import table, so
    /// the bytes must be a real (if empty) PE. Same shape as
    /// `vfs-directord/tests/staging.rs`'s `bare_pe`.
    fn bare_pe() -> Vec<u8> {
        let mut pe = vec![0u8; 0x400];
        pe[0] = b'M';
        pe[1] = b'Z';
        pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        pe[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
        pe[0x98..0x9A].copy_from_slice(&0x20Bu16.to_le_bytes());
        pe
    }

    fn content(tag: &str) -> PathBuf {
        let p = crate::test_scratch::scratch_created(&format!("li-{tag}"));
        std::fs::write(p.join("game.exe"), bare_pe()).unwrap();
        p
    }

    #[test]
    fn a_graph_only_image_in_root_zero_is_staged_into_the_root() {
        let c = content("stage");
        let s = crate::test_scratch::session_in_scratch("stage-session");
        s.mount("", Arc::new(DiskProvider::new(&c))).unwrap();
        let loc0 = s.root_locations()[0].location.clone();
        let img = image::join_location(&loc0, "game.exe");
        let host = s
            .resolve_for_test(&LaunchOpts {
                image: img,
                ..Default::default()
            })
            .unwrap();
        assert!(
            host.is_file(),
            "staged image must exist on the host: {}",
            host.display()
        );
        assert!(
            host.starts_with(s.virtual_root()),
            "staged into root 0's backing dir"
        );
    }

    #[test]
    fn a_graph_only_image_in_another_root_is_refused_by_name() {
        let c = content("r1");
        let mut s = Session::new();
        let loc1 = if cfg!(windows) {
            std::env::temp_dir()
                .join(format!("vfs-li-r1loc-{}", std::process::id()))
                .to_string_lossy()
                .into_owned()
        } else {
            r"C:\users\steamuser\Saves".to_string()
        };
        s.declare_root(1, &loc1);
        s.mount_at(RootId(1), "", Arc::new(DiskProvider::new(&c)))
            .unwrap();
        let e = s
            .resolve_for_test(&LaunchOpts {
                image: image::join_location(&loc1, "game.exe"),
                ..Default::default()
            })
            .unwrap_err();
        assert!(e.contains("root 1") && e.contains("root 0"), "{e}");
    }

    #[test]
    fn an_image_no_root_serves_is_refused() {
        let s = Session::new();
        let e = s
            .resolve_for_test(&LaunchOpts {
                image: "missing.exe".into(),
                ..Default::default()
            })
            .unwrap_err();
        // "nothing to stage" is the Windows launch's refusal text, which
        // `embed_api.rs`'s Windows-only launch test asserts on.
        assert!(
            e.contains("missing.exe") && e.contains("nothing to stage"),
            "{e}"
        );
    }

    /// A drive-relative `C:foo.exe` is relative to `classify_image`, so it
    /// lands in root 0 — but on Windows `Path::join` of a component with a
    /// drive replaces the whole base. Refused on every target, by name, even
    /// where a file of that name really exists in the root (unix allows it).
    #[test]
    fn a_vpath_component_with_a_colon_is_refused_by_name() {
        let s = crate::test_scratch::session_in_scratch("colon");
        std::fs::create_dir_all(s.virtual_root().join("bin")).unwrap();
        if cfg!(unix) {
            std::fs::write(s.virtual_root().join("C:foo.exe"), bare_pe()).unwrap();
            std::fs::write(s.virtual_root().join("bin").join("D:x.exe"), bare_pe()).unwrap();
        }
        for img in ["C:foo.exe", r"bin\D:x.exe"] {
            let e = s
                .resolve_for_test(&LaunchOpts {
                    image: img.into(),
                    ..Default::default()
                })
                .unwrap_err();
            assert!(
                e.contains(&format!("{img:?}")) && e.contains("':'"),
                "{img}: {e}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_root_zero_location_defaults_and_can_be_declared() {
        let mut s = Session::new();
        assert_eq!(s.root_locations()[0].location, r"C:\vfs-session\root");
        s.declare_root(0, r"C:\Games\Fixture");
        assert_eq!(s.root_locations()[0].location, r"C:\Games\Fixture");
        assert_ne!(
            s.virtual_root(),
            Path::new(r"C:\Games\Fixture"),
            "virtual_root stays the host dir"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_host_path_outside_every_root_is_refused() {
        let s = Session::new();
        let e = s
            .resolve_for_test(&LaunchOpts {
                image: "/usr/bin/true".into(),
                ..Default::default()
            })
            .unwrap_err();
        assert!(e.contains("no drive"), "{e}");
    }

    /// The accepted pre-location form: an absolute host path inside the
    /// managed root is the same as its relative remainder.
    #[cfg(unix)]
    #[test]
    fn unix_host_path_inside_the_managed_root_is_root_zero() {
        let s = crate::test_scratch::session_in_scratch("inside-root");
        std::fs::create_dir_all(s.virtual_root().join("bin")).unwrap();
        let real = s.virtual_root().join("bin").join("real.exe");
        std::fs::write(&real, bare_pe()).unwrap();
        let host = s
            .resolve_for_test(&LaunchOpts {
                image: real.to_string_lossy().into_owned(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(host, real);
    }
}
