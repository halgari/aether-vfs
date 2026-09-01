//! `vfs-proton` — acquire, list and locate GE-Proton runtimes.
//!
//! # Output discipline
//!
//! This is the one thing about this binary that is not negotiable. A host or a
//! shell script sets `PROTONPATH` from it:
//!
//! ```text
//! PROTONPATH="$(vfs-proton path)"
//! ```
//!
//! so **stdout carries the directory and nothing else**. Every resolved-tag
//! notice, every progress line and every diagnostic goes to stderr. That holds
//! for all three subcommands, not just `path`, which is why `install` also
//! ends by printing its install directory alone on stdout — `PROTONPATH="$(
//! vfs-proton install)"` is then a single correct line in a setup script.
//!
//! `list` is the exception in shape only: it prints one `TAG<TAB>PATH` line per
//! installed runtime on stdout, newest first, and prints nothing at all (exit
//! 0) when none are installed.
//!
//! # Progress
//!
//! The real tarball is 533,700,853 bytes. A fetch that size with no output is
//! indistinguishable from a hang, so progress is reported — on stderr, at most
//! once a second, as a percentage of [`Release::size`]. There is no callback in
//! the download path (it is a [`std::io::copy`] straight into a `.partial`), so
//! progress is measured the only honest way available: by polling the length of
//! that file, whose name comes from [`vfs_proton::partial_path`].

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use vfs_proton::{
    fetch_releases, install_release, installed_dirs, partial_path, pick, verify_ge, Release, Root,
};

/// The major GE-Proton series `install` resolves within when no `--version` is
/// given. Eleven because that is the series this project runs; picking "newest
/// overall" instead would silently jump a major the day GE tags 12.0, which is
/// not a decision a runtime installer should make on a user's behalf.
const DEFAULT_MAJOR: u32 = 11;

#[derive(Parser, Debug)]
#[command(
    name = "vfs-proton",
    about = "Install and locate GE-Proton runtimes under aether-vfs's own directory",
    long_about = "Install and locate GE-Proton runtimes.\n\n\
        Everything is written under aether-vfs's own base directory (--dir, else \
        $VFS_HOME, else $XDG_DATA_HOME/aether-vfs, else $HOME/.local/share/aether-vfs); \
        nothing is ever written to a Steam or system path.\n\n\
        stdout carries a directory path and nothing else, so a host can do \
        PROTONPATH=\"$(vfs-proton path)\". Progress and diagnostics go to stderr."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Download, verify and install a GE-Proton runtime.
    Install {
        /// Install exactly this tag (e.g. GE-Proton11-6). Without it, the
        /// newest release in the --major series is resolved.
        #[arg(long, value_name = "TAG")]
        version: Option<String>,
        /// Major series to resolve within. Ignored when --version is given.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_MAJOR)]
        major: u32,
        /// Base directory for runtimes and downloads.
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
        /// Re-download and reinstall even if the tag is already present.
        #[arg(long)]
        force: bool,
    },
    /// List installed, GE-verified runtimes, newest first, as TAG<TAB>PATH.
    List {
        /// Base directory for runtimes and downloads.
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
    },
    /// Print the directory of the newest (or named) installed runtime.
    Path {
        /// Print this tag's directory instead of the newest one's.
        #[arg(long, value_name = "TAG")]
        version: Option<String>,
        /// Base directory for runtimes and downloads.
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Install {
            version,
            major,
            dir,
            force,
        } => cmd_install(version, major, dir, force),
        Command::List { dir } => cmd_list(dir),
        Command::Path { version, dir } => cmd_path(version, dir),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            // Diagnostics never touch stdout: a caller may be capturing it into
            // PROTONPATH, and a variable set to an error message is worse than
            // one left unset.
            eprintln!("vfs-proton: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Resolves the base directory: an explicit `--dir` wins, otherwise the
/// environment. `--dir` is taken as given rather than validated, because the
/// user named it; only *tags* are untrusted here, and those go through
/// [`Root::try_runtime_dir`].
fn root_for(dir: Option<PathBuf>) -> Result<Root, String> {
    match dir {
        Some(dir) => Ok(Root::at(dir)),
        None => Root::from_env().map_err(|e| format!("could not resolve a base directory: {e}")),
    }
}

fn cmd_install(
    version: Option<String>,
    major: u32,
    dir: Option<PathBuf>,
    force: bool,
) -> Result<(), String> {
    let root = root_for(dir)?;

    // A named tag that is already installed and verified needs no network at
    // all — not even the releases listing. `install_release` is idempotent on
    // its own, but only *after* it has resolved a Release, and resolving one
    // costs a GitHub request. Short-circuiting here is what makes a repeat
    // `install --version X` work offline.
    if !force {
        if let Some(tag) = version.as_deref() {
            let dir = root.try_runtime_dir(tag).map_err(|e| e.to_string())?;
            if let Ok(found) = verify_ge(&dir) {
                eprintln!("{found} is already installed; nothing to download");
                println!("{}", dir.display());
                return Ok(());
            }
        }
    }

    let agent = ureq::Agent::new_with_defaults();
    let release = resolve(&agent, version.as_deref(), major)?;
    eprintln!(
        "resolved {} ({} MiB) from {}",
        release.tag,
        mib(release.size),
        release.tarball_url
    );

    let progress = Progress::start(&root, &release);
    let outcome = install_release(&root, &release, &agent, force);
    progress.stop();
    let installed = outcome.map_err(|e| e.to_string())?;

    if installed.fresh {
        eprintln!(
            "installed {}: downloaded, digest verified, confirmed GE-Proton",
            installed.tag
        );
    } else {
        eprintln!(
            "{} was already installed and verified; nothing was downloaded",
            installed.tag
        );
    }
    println!("{}", installed.dir.display());
    Ok(())
}

/// Turns `--version`/`--major` into a concrete [`Release`].
///
/// A named tag still needs the releases listing: the tarball and digest URLs
/// come from the API, and building them by string convention instead would
/// invent an asset name that upstream is free to change.
fn resolve(agent: &ureq::Agent, version: Option<&str>, major: u32) -> Result<Release, String> {
    let releases =
        fetch_releases(agent).map_err(|e| format!("could not list GE-Proton releases: {e}"))?;
    match version {
        Some(tag) => releases
            .into_iter()
            .find(|r| r.tag == tag)
            .ok_or_else(|| format!("no recent GE-Proton release is tagged {tag}")),
        None => pick(&releases, Some(major)).cloned().ok_or_else(|| {
            format!("no GE-Proton {major}.x release has an x86_64 asset pair in the recent listing")
        }),
    }
}

fn cmd_list(dir: Option<PathBuf>) -> Result<(), String> {
    let root = root_for(dir)?;
    let found = installed_dirs(&root)
        .map_err(|e| format!("could not read {}: {e}", root.runtimes().display()))?;
    let mut out = std::io::stdout().lock();
    for (tag, dir) in found {
        // A broken pipe (`vfs-proton list | head -1`) is not an error worth a
        // non-zero exit.
        if writeln!(out, "{tag}\t{}", dir.display()).is_err() {
            break;
        }
    }
    Ok(())
}

fn cmd_path(version: Option<String>, dir: Option<PathBuf>) -> Result<(), String> {
    let root = root_for(dir)?;
    let dir = match version {
        Some(tag) => {
            let dir = root.try_runtime_dir(&tag).map_err(|e| e.to_string())?;
            // The GE gate, not merely `is_dir`: a directory that exists but is
            // stock Proton (or half-extracted) must not become PROTONPATH.
            verify_ge(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            dir
        }
        None => installed_dirs(&root)
            .map_err(|e| format!("could not read {}: {e}", root.runtimes().display()))?
            .into_iter()
            .next()
            .map(|(_, dir)| dir)
            .ok_or_else(|| {
                format!(
                    "no GE-Proton runtime is installed under {}; run `vfs-proton install`",
                    root.runtimes().display()
                )
            })?,
    };
    println!("{}", dir.display());
    Ok(())
}

/// How often the progress thread wakes to check whether the install has
/// finished. Short enough that `stop()` returns promptly; the *reporting*
/// interval is a separate, longer one.
const POLL: Duration = Duration::from_millis(100);

/// Minimum gap between progress lines. "At most once a second" is the
/// requirement: this runs against a terminal a user is watching, and also
/// against a CI log or a redirected file, where a line per poll would be
/// thousands of lines of noise.
const REPORT_EVERY: Duration = Duration::from_secs(1);

/// A background thread reporting download progress on stderr.
struct Progress {
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Progress {
    fn start(root: &Root, release: &Release) -> Progress {
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let path = partial_path(root, &release.tag);
        let tag = release.tag.clone();
        let total = release.size;

        let handle = std::thread::spawn(move || {
            // Start the clock now, so the first line lands about a second in
            // rather than immediately — at which point the `.partial` does not
            // exist yet and there is nothing true to say.
            let mut last = Instant::now();
            // Once the whole body is on disk the length stops moving, and
            // repeating "100.0%" for the length of a 1.4 GB extraction is a
            // lie about what the process is doing. Say what it is doing, once.
            let mut announced_tail = false;
            loop {
                let finished = flag.load(Ordering::Relaxed);
                if last.elapsed() >= REPORT_EVERY {
                    last = Instant::now();
                    if let Ok(meta) = std::fs::metadata(&path) {
                        let got = meta.len();
                        let mut err = std::io::stderr().lock();
                        if total > 0 && got >= total {
                            if !announced_tail {
                                announced_tail = true;
                                let _ = writeln!(
                                    err,
                                    "  {tag}: download complete, verifying digest and extracting..."
                                );
                            }
                        } else if total > 0 {
                            let percent = got as f64 / total as f64 * 100.0;
                            let _ = writeln!(
                                err,
                                "  {tag}: {percent:.1}% ({} / {} MiB)",
                                mib(got),
                                mib(total)
                            );
                        } else {
                            // `size` is 0 when the API omitted it; report the
                            // absolute figure rather than dividing by zero.
                            let _ = writeln!(err, "  {tag}: {} MiB", mib(got));
                        }
                    }
                }
                if finished {
                    break;
                }
                std::thread::sleep(POLL);
            }
        });

        Progress {
            done,
            handle: Some(handle),
        }
    }

    fn stop(mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Bytes as whole mebibytes. Whole, because a tenth of a MiB on a 533 MiB
/// download is not information.
fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}
