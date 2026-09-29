//! `vfs` — the director daemon and its reference client, in one binary.
//!
//! * `vfs daemon` runs the daemon in the foreground (tests / debugging).
//! * every other subcommand is a client; it discovers a running daemon (or
//!   auto-spawns `vfs daemon`) and drives it over gRPC.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use vfs_control::pb::{Empty, HealthReq, LayerNameReq, LayerPathReq, TeardownReq};
use vfs_directord::{
    apply_session_config, connect_or_spawn, default_discovery_path, launch_one_shot,
    open_daemon_storage, parse_source_flag, root_flag_entries, run_launch, serve_daemon,
    storage_dir_from, DEFAULT_BIND,
};

/// The `vfs` control CLI + daemon.
#[derive(Parser, Debug)]
#[command(name = "vfs", version, about = "VFS director daemon + control CLI")]
struct Cli {
    /// Override daemon endpoint discovery (e.g. `127.0.0.1:7000`).
    #[arg(long, global = true)]
    endpoint: Option<String>,

    /// Override discovery file path (default: per-user; or `$VFS_DISCOVERY_PATH`).
    #[arg(long, global = true)]
    discovery: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the director daemon in the foreground.
    Daemon {
        /// Bind address (`host:port`). Port 0 = ephemeral (written to discovery).
        #[arg(long, default_value = DEFAULT_BIND)]
        bind: String,
        /// Directory of the daemon's storage (named layers and the cache).
        /// Default: `$VFS_STORAGE_DIR`, else `$VFS_HOME/storage`.
        #[arg(long = "storage-dir")]
        storage_dir: Option<PathBuf>,
        /// Budget for cached source data, in GiB (default 32; at least 1).
        #[arg(long = "cache-max-gib")]
        cache_max_gib: Option<u64>,
    },
    /// Check daemon health.
    Health,
    /// Bring a whole scenario up from a config file (`--config scenario.toml`).
    Up {
        #[arg(long)]
        config: String,
    },
    /// Tear a scenario / session down.
    Down {
        #[arg(long)]
        session: String,
    },
    /// Launch a program in a live session (`vfs up` without `[launch]`).
    /// PATH is `{RootName}\rel`, an absolute path, or root-0-relative.
    Exec {
        /// The live session, by id or by name.
        #[arg(long)]
        session: String,
        path: String,
        /// `KEY=VALUE` child environment entries, repeatable.
        #[arg(long = "env")]
        env: Vec<String>,
        /// Return as soon as the child starts instead of waiting for it to
        /// exit. (Refused on Linux, where a Proton launch always waits.)
        #[arg(long = "no-wait")]
        no_wait: bool,
        /// Arguments for the program, after `--`.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Launch an executable in a fresh session from `--source` flags.
    Launch {
        /// `TYPE:PATH@MOUNT`, repeatable. Precedence is declaration order
        /// (later flag wins on a shared path), same as the config file's
        /// flat `[[source]]` list.
        #[arg(long = "source")]
        sources: Vec<String>,
        /// Where the session's writes land, copied up from whatever the
        /// sources hold (gate 4): a directory, or `layer:NAME` for a named,
        /// persistent layer in the daemon's storage (see `vfs layer`).
        /// Without it every source composes as read-only content and an
        /// in-place edit of it is refused.
        #[arg(long = "write-layer")]
        write_layer: Option<String>,
        /// `ID=NAME=LOCATION`, repeatable: a root, its name (for `{NAME}\…`
        /// paths) and where the program sees it. Once any is given, root 0
        /// must be one of them.
        #[arg(long = "root")]
        roots: Vec<String>,
        /// Session name; on Linux it also names a persistent Wine prefix.
        #[arg(long)]
        name: Option<String>,
        /// `{RootName}\rel`, an absolute path, or root-0-relative.
        #[arg(long)]
        exec: String,
        #[arg(long)]
        args: Vec<String>,
        /// Return as soon as the child starts instead of waiting for it to
        /// exit. (Refused on Linux, where a Proton launch always waits.)
        #[arg(long = "no-wait")]
        no_wait: bool,
        /// `KEY=VALUE` child environment entries, repeatable.
        #[arg(long = "env")]
        env: Vec<String>,
    },
    /// List active sessions.
    Sessions,
    /// Cache / storage / daemon stats.
    Stats,
    /// Manage the named layers in the daemon's storage.
    Layer {
        #[command(subcommand)]
        command: LayerCommand,
    },
}

#[derive(Subcommand, Debug)]
enum LayerCommand {
    /// List every layer with its file count and size.
    List,
    /// Write layer NAME into DIR as plain files (DIR must be empty or absent).
    Export { name: String, dir: PathBuf },
    /// Create layer NAME from the tree under DIR (NAME must not exist).
    Import { dir: PathBuf, name: String },
    /// Delete layer NAME (refused while a live session writes into it).
    Delete { name: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let discovery = cli
        .discovery
        .clone()
        .or_else(|| vfs_env::path(vfs_env::DISCOVERY_PATH));

    match cli.command {
        Command::Daemon {
            bind,
            storage_dir,
            cache_max_gib,
        } => {
            let addr: SocketAddr = bind
                .parse()
                .map_err(|e| format!("bad --bind {bind}: {e}"))?;
            let dir = storage_dir_from(storage_dir.as_deref(), &|k| std::env::var_os(k))
                .ok_or_else(|| {
                    format!(
                        "no storage directory: pass --storage-dir, or set {} or {}",
                        vfs_env::STORAGE_DIR,
                        vfs_env::HOME
                    )
                })?;
            // Opened before the discovery file is written, so a daemon that
            // cannot have its storage never advertises itself.
            let storage = open_daemon_storage(&dir, cache_max_gib)?;
            eprintln!("vfs daemon: storage at {}", dir.display());
            let path = discovery.unwrap_or_else(default_discovery_path);
            if let Some(p) = discovery_path_for_env(&path) {
                std::env::set_var(vfs_env::DISCOVERY_PATH, p);
            }
            serve_daemon(addr, path, storage)
                .await
                .map_err(|e| format!("daemon: {e}"))?;
            Ok(ExitCode::SUCCESS)
        }
        other => {
            let exe = std::env::current_exe()?;
            let mut client =
                connect_or_spawn(cli.endpoint.as_deref(), discovery.clone(), exe).await?;

            match other {
                Command::Daemon { .. } => unreachable!(),
                Command::Health => {
                    let resp = client.health(HealthReq {}).await?.into_inner();
                    println!("ok version={} sessions={}", resp.version, resp.sessions);
                    Ok(ExitCode::SUCCESS)
                }
                Command::Up { config } => {
                    let cfg = vfs_control::load(&config)?;
                    let (session_id, exit) = apply_session_config(&mut client, &cfg).await?;
                    println!("session {session_id}");
                    Ok(exit_code(exit))
                }
                Command::Exec {
                    session,
                    path,
                    env,
                    no_wait,
                    args,
                } => {
                    let launch = vfs_control::LaunchConfig {
                        exec: path,
                        args,
                        wait: !no_wait,
                        env: parse_env(&env)?,
                    };
                    let exit = run_launch(&mut client, &session, &launch).await?;
                    Ok(exit_code(exit))
                }
                Command::Down { session } => {
                    client
                        .teardown_session(TeardownReq {
                            session_id: session,
                        })
                        .await?;
                    println!("torn down");
                    Ok(ExitCode::SUCCESS)
                }
                Command::Launch {
                    sources,
                    write_layer,
                    roots,
                    name,
                    exec,
                    args,
                    no_wait,
                    env,
                } => {
                    let mut entries = Vec::new();
                    for s in &sources {
                        entries.push(parse_source_flag(s)?);
                    }
                    if let Some(path) = &write_layer {
                        entries.push(vfs_directord::write_layer_flag_entry(path)?);
                    }
                    let cfg = vfs_control::SessionConfig {
                        session: vfs_control::SessionMeta { name },
                        roots: root_flag_entries(&roots)?,
                        sources: entries,
                        launch: Some(vfs_control::LaunchConfig {
                            exec,
                            args,
                            wait: !no_wait,
                            env: parse_env(&env)?,
                        }),
                        cache: None,
                    };
                    let (session_id, exit) = launch_one_shot(&mut client, &cfg).await?;
                    println!("session {session_id}");
                    Ok(exit_code(exit))
                }
                Command::Sessions => {
                    let list = client.list_sessions(Empty {}).await?.into_inner();
                    if list.sessions.is_empty() {
                        println!("(no sessions)");
                    } else {
                        for s in list.sessions {
                            println!("{}\t{}\t{}", s.id, s.name, s.root);
                        }
                    }
                    Ok(ExitCode::SUCCESS)
                }
                Command::Stats => {
                    let s = client.stats(Empty {}).await?.into_inner();
                    println!(
                        "sessions={} hits={} misses={} evicts={} disk_hits={} ram_bytes={} from_cache={} from_source={}",
                        s.sessions,
                        s.cache_hits,
                        s.cache_misses,
                        s.cache_evicts,
                        s.cache_disk_hits,
                        s.cache_ram_bytes,
                        s.cache_bytes_from_cache,
                        s.cache_bytes_from_source
                    );
                    println!(
                        "layers={} pack_bytes={} live_bytes={} cache_logical_bytes={}",
                        s.layers, s.store_pack_bytes, s.store_live_bytes, s.cache_logical_bytes
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Command::Layer { command } => {
                    match command {
                        LayerCommand::List => {
                            let list = client.list_layers(Empty {}).await?.into_inner();
                            if list.layers.is_empty() {
                                println!("(no layers)");
                            }
                            for l in list.layers {
                                println!(
                                    "{}\t{} files\t{} bytes",
                                    l.name, l.files, l.logical_bytes
                                );
                            }
                        }
                        LayerCommand::Export { name, dir } => {
                            let n = client
                                .export_layer(LayerPathReq {
                                    name: name.clone(),
                                    dir: absolute(&dir)?,
                                })
                                .await?
                                .into_inner()
                                .files;
                            println!(
                                "exported {n} file(s) from layer {name} to {}",
                                dir.display()
                            );
                        }
                        LayerCommand::Import { dir, name } => {
                            let n = client
                                .import_layer(LayerPathReq {
                                    name: name.clone(),
                                    dir: absolute(&dir)?,
                                })
                                .await?
                                .into_inner()
                                .files;
                            println!(
                                "imported {n} file(s) from {} into layer {name}",
                                dir.display()
                            );
                        }
                        LayerCommand::Delete { name } => {
                            client
                                .delete_layer(LayerNameReq { name: name.clone() })
                                .await?;
                            println!("deleted layer {name}");
                        }
                    }
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
    }
}

/// `dir` as an absolute path string: the daemon resolves a relative one
/// against its own working directory, not this command's.
fn absolute(dir: &std::path::Path) -> Result<String, String> {
    std::path::absolute(dir)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// `--env KEY=VALUE` flags as the child's environment map.
fn parse_env(flags: &[String]) -> Result<BTreeMap<String, String>, String> {
    flags
        .iter()
        .map(|e| {
            e.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| format!("--env expects KEY=VALUE, got {e:?}"))
        })
        .collect()
}

/// The CLI's exit status for a launch: the child's code, or success when
/// there was none to report. See [`exit_byte`].
fn exit_code(exit: Option<i32>) -> ExitCode {
    ExitCode::from(exit_byte(exit))
}

/// The one byte a process can exit with, for a child's exit code: `0` only
/// for `0` (or no code at all); an ordinary `1..=255` as itself; anything
/// else — negative, or too wide for a byte — `1`.
///
/// Never a clamp or a truncation. On Windows `GetExitCodeProcess`'s `u32`
/// arrives here cast to `i32`, so a crash (`0xC0000005`, an access
/// violation) is negative: clamping it to `0` reported a crashed program as
/// success. Truncating is no better — `256`'s low byte is `0`.
fn exit_byte(exit: Option<i32>) -> u8 {
    match exit {
        None | Some(0) => 0,
        Some(code) => u8::try_from(code).ok().filter(|&b| b != 0).unwrap_or(1),
    }
}

fn discovery_path_for_env(path: &std::path::Path) -> Option<std::ffi::OsString> {
    Some(path.as_os_str().to_os_string())
}

#[cfg(test)]
mod tests {
    use super::exit_byte;

    #[test]
    fn success_and_no_code_exit_zero() {
        assert_eq!(exit_byte(None), 0);
        assert_eq!(exit_byte(Some(0)), 0);
    }

    #[test]
    fn an_ordinary_code_is_kept() {
        for c in [1, 3, 42, 255] {
            assert_eq!(exit_byte(Some(c)), c as u8);
        }
    }

    /// `0xC0000005` (access violation) as `GetExitCodeProcess`'s `u32` cast
    /// to `i32`: a crash must never read as success.
    #[test]
    fn a_negative_code_is_failure() {
        assert_eq!(exit_byte(Some(0xC000_0005_u32 as i32)), 1);
        assert_eq!(exit_byte(Some(-1)), 1);
        assert_eq!(exit_byte(Some(i32::MIN)), 1);
    }

    #[test]
    fn a_code_wider_than_a_byte_is_failure() {
        assert_eq!(
            exit_byte(Some(256)),
            1,
            "256's low byte is 0; it must not read as success"
        );
        assert_eq!(exit_byte(Some(257)), 1);
        assert_eq!(exit_byte(Some(i32::MAX)), 1);
    }
}
