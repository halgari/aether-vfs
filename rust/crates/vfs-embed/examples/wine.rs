//! A session's Wine prefix, by hand.
//!
//! ```text
//! wine prefix <name>                     set up (or find) the named prefix; print its path
//! wine run <name> <program> [args...]    run a Windows program in it, nothing injected
//! wine launch <name> <game-dir> <exe> [args...]
//!                                        serve <game-dir> through aether-vfs and launch
//!                                        <exe> (relative to it) in the prefix
//! ```
//!
//! `launch` mounts `<game-dir>` read-only as root 0 at `C:\Games\<dir name>`,
//! with a write layer that persists under the home's `launches/<dir name>/`
//! (what the game writes beside itself lands there), and waits for the game
//! and whatever it started to exit. The prefix is shared
//! (`Session::set_prefix_shared`): a Steam client running there is left
//! running. A `steam_appid.txt` in the
//! game directory tells a SteamStub-wrapped exe its app id; the Steam client
//! it checks with must already be running and signed in, in the same prefix
//! (`wine run <name> 'C:\Program Files (x86)\Steam\steam.exe'`).
//!
//! The prefix is the one a [`Session`] with `set_prefix_name(<name>)` launches
//! into, under the aether-vfs home (`VFS_HOME`, else the platform default), on
//! this platform's Wine host (CrossOver on macOS, GE-Proton elsewhere). `run`
//! is for what a launch does not do: installers, and clients such as Steam
//! that a game expects to find running in its prefix. A `<program>` that is a
//! host path is passed as one; Wine resolves it through `Z:`.

#[cfg(unix)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["prefix", name] => prefix(name).map(|p| {
            println!("{}", p.display());
            0
        }),
        ["run", name, program, rest @ ..] => run(name, program, rest),
        ["launch", name, dir, exe, rest @ ..] => launch(name, dir, exe, rest),
        _ => {
            eprintln!(
                "usage: wine prefix <name>\n       wine run <name> <program> [args...]\n       \
                 wine launch <name> <game-dir> <exe> [args...]"
            );
            Ok(2)
        }
    };
    match code {
        Ok(c) => std::process::exit(c),
        Err(e) => {
            eprintln!("wine: {e}");
            std::process::exit(1)
        }
    }
}

#[cfg(unix)]
fn prefix(name: &str) -> Result<std::path::PathBuf, String> {
    let mut s = vfs_embed::Session::new();
    s.set_prefix_name(name)?;
    s.prepare_prefix()
}

#[cfg(unix)]
fn run(name: &str, program: &str, args: &[&str]) -> Result<i32, String> {
    let prefix = prefix(name)?;
    let root = vfs_embed::proton::Root::from_env().map_err(|e| e.to_string())?;
    let host = vfs_proton::host::default_host(&root)?;
    eprintln!("wine: {} in {}", host.describe(), prefix.display());
    let status = host
        .wine_command(&prefix)
        .arg(program)
        .args(args)
        .status()
        .map_err(|e| format!("{}: {e}", host.wine().display()))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(unix)]
fn launch(name: &str, dir: &str, exe: &str, args: &[&str]) -> Result<i32, String> {
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use vfs_embed::{DiskProvider, LaunchOpts, Provider, ReadOnlyProvider, RootId, Session};

    let game = std::fs::canonicalize(dir).map_err(|e| format!("{dir}: {e}"))?;
    let title = game
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "game".into());
    let root = vfs_embed::proton::Root::from_env().map_err(|e| e.to_string())?;
    let home = root.sessions();
    let home = home.parent().unwrap_or(&home);
    let work = home.join("launches").join(&title);
    let fresh = |d: &str| -> Result<std::path::PathBuf, String> {
        let p = work.join(d);
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        Ok(p)
    };
    let upper = work.join("upper");
    std::fs::create_dir_all(&upper).map_err(|e| format!("{}: {e}", upper.display()))?;
    let artifacts = artifacts_dir()?;

    let mut s = Session::new();
    s.set_prefix_name(name)?;
    // Steam runs in this prefix too: the launch is the game's processes,
    // and ending it leaves Steam running.
    s.set_prefix_shared(true);
    s.set_root(fresh("root")?);
    s.set_state_dir(fresh("state")?);
    s.set_overlay(fresh("overlay")?);
    s.declare_root(0, format!(r"C:\Games\{title}"));
    s.set_io_workers(8);
    let base: Arc<dyn Provider> =
        Arc::new(ReadOnlyProvider::new(Arc::new(DiskProvider::new(&game))));
    s.mount_at(RootId(0), "", base)
        .map_err(|e| format!("mount: status {e}"))?;
    s.set_write_layer_at(RootId(0), Arc::new(DiskProvider::new(&upper)))
        .map_err(|e| format!("write layer: status {e}"))?;
    s.serve()?;

    let log = work.join("wine.log");
    eprintln!(
        "wine: launching {exe} from {} in prefix {name}; Wine's output goes to {}",
        game.display(),
        log.display()
    );
    let started = Instant::now();
    let code = s.launch(&LaunchOpts {
        image: exe.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        wait: true,
        stage_also: vec![exe.to_string()],
        shim_dll: Some(artifacts.join(vfs_embed::proton::artifacts::SHIM_DLL)),
        injector: Some(artifacts.join(vfs_embed::proton::artifacts::INJECTOR)),
        log_file: Some(log),
        ready_timeout: Some(Duration::from_secs(300)),
        // The launch silences Wine (`WINEDEBUG=-all`); this process's own
        // setting, when there is one, is passed through instead.
        env: std::env::var("WINEDEBUG")
            .map(|v| [("WINEDEBUG".to_string(), v)].into())
            .unwrap_or_default(),
        ..Default::default()
    })?;
    eprintln!(
        "wine: {exe} and everything it started exited after {:.0?}, code {code}",
        started.elapsed()
    );
    Ok(code)
}

/// Where the Windows artifacts are: `VFS_WINDOWS_ARTIFACTS`, else beside this
/// binary, else its parent (`target/<profile>` for `target/<profile>/examples`).
#[cfg(unix)]
fn artifacts_dir() -> Result<std::path::PathBuf, String> {
    let shim = vfs_embed::proton::artifacts::SHIM_DLL;
    if let Some(d) = std::env::var_os("VFS_WINDOWS_ARTIFACTS") {
        return Ok(d.into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    exe.ancestors()
        .skip(1)
        .take(2)
        .find(|d| d.join(shim).is_file())
        .map(|d| d.to_path_buf())
        .ok_or_else(|| format!("no {shim} beside {}: run bin/build-windows", exe.display()))
}

#[cfg(not(unix))]
fn main() {
    eprintln!("wine: unix only (on Windows a launch needs no Wine)");
    std::process::exit(2);
}
