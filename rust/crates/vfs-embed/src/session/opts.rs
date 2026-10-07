//! Launch and staging options.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Options for [`Session::launch`].
#[derive(Clone, Debug)]
pub struct LaunchOpts {
    /// Path to the image to launch.
    ///
    /// Resolved against the session's root **locations**
    /// ([`Session::root_locations`], [`crate::image::classify_image`]): a
    /// **relative** name is shorthand for root 0; an **absolute** path inside
    /// a root's location is that root's vpath — a real file there is launched
    /// as is, one only root 0's provider graph serves is staged first (see
    /// [`Session::launch`]); an absolute path outside every root is a real
    /// program, launched as given. Anything else is refused by name rather
    /// than handed to `CreateProcess`.
    pub image: String,
    pub args: Vec<String>,
    /// Wait for process exit (false = detach; session must stay alive).
    ///
    /// On the Proton path a detached launch is held by the session: stop it
    /// with [`Session::stop_launch`] (dropping the session also stops it). A
    /// host that wants the handle itself calls [`Session::launch_detached`].
    pub wait: bool,
    /// Extra images to stage beside a graph-resolved `image`, by vpath, each
    /// with its own PE import closure. Ignored when nothing is staged.
    ///
    /// A launcher that spawns the real game (SKSE's `skse64_loader.exe` starts
    /// `SkyrimSE.exe`) needs its target on disk beside it: the child's own
    /// `CreateProcess` needs a real image just as much as the first one did,
    /// and nothing intercepts it. Naming it here is how a host says so.
    pub stage_also: Vec<String>,
    /// Real-disk directories searched for imports the provider graph does not
    /// carry. Ignored when nothing is staged.
    ///
    /// Redistributables (`d3dx9_42.dll` and friends) are static imports of the
    /// game but ship with a runtime rather than in the game archive, so
    /// without a fallback the loader fails them during process init — before
    /// any hook of ours exists to help.
    pub stage_fallback_dirs: Vec<PathBuf>,
    /// Absolute paths to `vfs_shim_dll.dll` and `vfs_payload.dll`.
    ///
    /// Left `None`, they are searched for **next to `std::env::current_exe()`**
    /// — and that is only the right answer when the host process *is* one of
    /// this workspace's binaries. For a language binding it is not: inside a
    /// Node addon `current_exe()` is `node.exe`, wherever the user's Node
    /// happens to be installed, and inside a Python extension it is
    /// `python.exe`. The DLLs live beside the addon, which nothing here can
    /// find from the executable.
    ///
    /// **So for any embedding host these are mandatory, not optional.** A
    /// binding should resolve them from its own module path (Node:
    /// `__dirname`) and set both. The symptom otherwise is
    /// "`vfs_shim_dll.dll` not found" from a host that shipped the DLL, with
    /// nothing pointing at why the search looked where it did.
    ///
    /// **On the Proton path these two fields answer for three files.** A Wine
    /// launch also needs `vfs-injector.exe`, and it has no field of its own:
    /// it is looked for **beside `shim_dll`** when that is set, and otherwise
    /// beside `current_exe()` — the one directory `cargo build` puts all three
    /// in. On Linux they are a separate Windows cross-build
    /// (`bin/build-windows`), so a missing one is reported by name (see `locate_wine_artifacts`) rather than surfacing as
    /// a path error out of `wine`.
    pub shim_dll: Option<String>,
    pub payload_dll: Option<String>,
    /// Extra environment variables for the child.
    ///
    /// **Proton path: child-only.** They go into the environment block the
    /// `wine` child is spawned with and nowhere else; `WINEDLLOVERRIDES` is
    /// merged with the launch's own (the caller wins per DLL,
    /// `vfs_proton::launch::merge_dll_overrides`), `WINEDEBUG` replaces the
    /// default `-all`, and a name the launch's handshake uses is refused.
    ///
    /// **Windows: not child-only.** `CreateProcessW` is called with a null environment
    /// block — inheritance *is* the mechanism — so [`Session::launch`] writes
    /// each one into **this process's** environment with `std::env::set_var`,
    /// launches, and restores the previous value. [`LAUNCH_ENV_LOCK`] serializes
    /// that against every other env write this crate performs, so two sessions
    /// cannot interleave; it cannot serialize a host's *own* threads, and
    /// `set_var` in a multi-threaded process races anything else reading the
    /// environment. See [`Session::launch`]'s "Process-global environment"
    /// section for the costed fix.
    pub env: BTreeMap<String, String>,
    /// **Proton path only**: the working directory the program starts in, as
    /// it sees it — a `C:\…` path, or a path relative to root 0's location.
    /// `None` is the image's own directory. On Windows the child starts in
    /// root 0's directory, as before, whatever this says.
    pub cwd: Option<String>,
    /// How long the injector waits for the shim to report ready. `None`:
    /// `VFS_READY_TIMEOUT_SECS` from this process's environment, else 180 s.
    pub ready_timeout: Option<std::time::Duration>,
    /// **Proton path only**: a host file that receives the `wine` child's
    /// stdout and stderr — and so those of everything it starts, which
    /// inherit them: `wineserver` (when this launch starts it), the injector,
    /// the game and its own children. Created with its parent directories and
    /// truncated at launch. Wine writes nothing useful there under the
    /// default `WINEDEBUG=-all`; set `WINEDEBUG` in [`env`](Self::env) (for
    /// example `err+all,warn+seh,fixme-all`) to get its error channels and
    /// unhandled-exception reports. The `wineserver -w` a launch uses to
    /// learn the prefix is quiet keeps its output discarded — it prints
    /// nothing worth keeping. `None`: the child inherits this process's
    /// stdout and stderr, as before. Ignored on Windows.
    pub log_file: Option<PathBuf>,
    /// **Proton path only**: NVIDIA NVAPI and NGX (DLSS) the way the `proton`
    /// script sets them up (`vfs_proton::nvapi`). `true` (the default): when
    /// an NVIDIA driver is loaded, the runtime's DXVK-NVAPI and the driver's
    /// Wine NGX DLLs are copied into the prefix (only those that changed) and
    /// the launch gets `DXVK_ENABLE_NVAPI=1`, `NVIDIA_WINE_DLL_DIR` and the
    /// `nvapi*` overrides, under [`env`](Self::env)'s; on any other machine
    /// nothing changes. `false`, or `PROTON_DISABLE_NVAPI=1` in
    /// [`env`](Self::env) or this process's environment: none of that, and
    /// `nvapi64.dll`/`nvapi.dll` are removed from the prefix, as the script
    /// does with NVAPI disabled. A DLL that cannot be copied or removed is a
    /// [`LaunchHandle::notes`] line, not a failed launch. Ignored on Windows.
    pub nvapi: bool,
}

impl Default for LaunchOpts {
    fn default() -> Self {
        LaunchOpts {
            // Deliberately empty rather than a plausible-looking game exe.
            // This used to default to `"SkyrimSE.exe"`, which is both
            // scenario-specific in a general API and the exact relative-image
            // case that cannot work (see the field's doc): a host that wrote
            // `..Default::default()` and forgot `image` got a launch attempt
            // for a file nobody named. `launch` refuses an empty image by
            // name instead.
            image: String::new(),
            args: Vec::new(),
            wait: true,
            stage_also: Vec::new(),
            stage_fallback_dirs: Vec::new(),
            shim_dll: None,
            payload_dll: None,
            env: BTreeMap::new(),
            cwd: None,
            ready_timeout: None,
            log_file: None,
            nvapi: true,
        }
    }
}

/// What [`Session::stage_launch`] writes to disk, beyond the image itself.
///
/// The staging *directory* and its tag are deliberately not here: they are the
/// session's (`state_dir/stage`, one tag per staged launch), because the
/// session is also what has to hold the resulting [`StagedDir`] alive and mount
/// it back into the graph. A caller choosing its own directory could hand the
/// same one to two sessions.
pub struct StageOpts<'a> {
    /// The image's vpath in root 0's provider graph.
    pub exe_vpath: &'a str,
    /// Additional images to stage into the same directory, each with its own
    /// import closure — see [`LaunchOpts::stage_also`].
    pub also: &'a [&'a str],
    /// Real-disk fallbacks for imports the graph does not carry — see
    /// [`LaunchOpts::stage_fallback_dirs`].
    pub fallback_dirs: &'a [PathBuf],
}
