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
use vfs_control::pb::{Empty, HealthReq, TeardownReq};
use vfs_directord::{
    apply_session_config, connect_or_spawn, default_discovery_path, parse_source_flag,
    root_flag_entries, run_launch, serve_daemon, DEFAULT_BIND,
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
        /// Directory the session's writes land in, copied up from whatever
        /// the sources hold (gate 4). Without it every source composes as
        /// read-only content and an in-place edit of it is refused.
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
    /// Cache / daemon stats.
    Stats,
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
    let discovery = cli.discovery.clone().or_else(|| {
        vfs_env::path(vfs_env::DISCOVERY_PATH)
    });

    match cli.command {
        Command::Daemon { bind } => {
            let addr: SocketAddr = bind.parse().map_err(|e| format!("bad --bind {bind}: {e}"))?;
            let path = discovery.unwrap_or_else(default_discovery_path);
            if let Some(p) = discovery_path_for_env(&path) {
                std::env::set_var(vfs_env::DISCOVERY_PATH, p);
            }
            serve_daemon(addr, path)
                .await
                .map_err(|e| format!("daemon: {e}"))?;
            Ok(ExitCode::SUCCESS)
        }
        other => {
            let exe = std::env::current_exe()?;
            let mut client = connect_or_spawn(
                cli.endpoint.as_deref(),
                discovery.clone(),
                exe,
            )
            .await?;

            match other {
                Command::Daemon { .. } => unreachable!(),
                Command::Health => {
                    let resp = client.health(HealthReq {}).await?.into_inner();
                    println!(
                        "ok version={} sessions={}",
                        resp.version, resp.sessions
                    );
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
                        entries.push(vfs_directord::write_layer_flag_entry(path));
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
                    let (session_id, exit) = apply_session_config(&mut client, &cfg).await?;
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
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
    }
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

/// The CLI's exit status for a launch: the child's code (clamped to a byte),
/// or success when there was none to report.
fn exit_code(exit: Option<i32>) -> ExitCode {
    match exit {
        None | Some(0) => ExitCode::SUCCESS,
        Some(code) => ExitCode::from(code.clamp(0, 255) as u8),
    }
}

fn discovery_path_for_env(path: &std::path::Path) -> Option<std::ffi::OsString> {
    Some(path.as_os_str().to_os_string())
}
