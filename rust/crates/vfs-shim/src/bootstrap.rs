//! Bootstrap glue: the config-file entry point used by the injected DLL to attach
//! the director's client and install the hooks. The config codec is
//! `vfs_protocol::shimcfg`.
use crate::hook::{install, HookGuard, InstallError};
use vfs_protocol::shimcfg::{self, ConfigError};

/// Errors bootstrapping the shim from a config file.
#[derive(Debug)]
pub enum BootstrapError {
    /// The config file could not be read.
    Io,
    /// The config did not decode: truncated or malformed, or written by a host from a
    /// different build (no version header, or another version). The message names both
    /// versions; a launcher shows it, because the fix is rebuilding the pair together.
    Config(ConfigError),
    /// A director was configured (a ring was named) but the FUSE client
    /// failed to attach. Fails before any hook is installed, and before the
    /// program has run: the load fails and the process with it.
    Fuse(String),
    /// The hook could not be installed.
    Install(InstallError),
}

/// The ready-file content that reports a bootstrap failure that is not the
/// director's: [`vfs_env::READY_BOOTSTRAP_FAILED_PREFIX`] and the reason. A
/// config error carries its own message (it names both versions).
pub fn bootstrap_failed_content(e: &BootstrapError) -> String {
    let why = match e {
        BootstrapError::Config(c) => c.to_string(),
        other => format!("{other:?}"),
    };
    format!("{}{why}", vfs_env::READY_BOOTSTRAP_FAILED_PREFIX)
}

/// Read a config file, attach the director's client, and install the hooks. Returns the
/// guard keeping the hooks alive (the shim DLL leaks it).
pub fn bootstrap_from_config_path(path: &str) -> Result<HookGuard, BootstrapError> {
    let bytes = std::fs::read(path).map_err(|_| BootstrapError::Io)?;
    // The root is the director's to answer for (the client's roots come from the environment), so
    // decoding is only the version and shape check: a config from another build fails here, loudly.
    shimcfg::decode_config(&bytes).map_err(BootstrapError::Config)?;
    // Attach to the parent director's FUSE ring. A process that names no ring at all is refused
    // like a ring that failed to attach: a shim with no director would run the program
    // un-virtualised while appearing to work. The client's roots
    // (`director::roots_from_env`) are the only notion of "under a root" the hooks have.
    match crate::director::try_init_from_env() {
        Ok(()) => {}
        Err(crate::director::FuseInitError::NotConfigured) => {
            return Err(BootstrapError::Fuse(
                "no VFS_RING_SECTION configured: standalone (no-director) shim launches are \
                 retired — a director must be attached"
                    .to_string(),
            ));
        }
        Err(crate::director::FuseInitError::ConnectFailed(msg)) => {
            return Err(BootstrapError::Fuse(msg));
        }
    }
    let guard = install().map_err(BootstrapError::Install)?;
    // Tell any spawning parent (that force-suspended us) our hooks are live.
    crate::child::signal_ready();
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Use `matches!` on the whole Result rather than `.unwrap_err()`: the latter
    // needs the Ok type `HookGuard: Debug`, which it deliberately is not (it owns
    // a RawDetour; a guard type carries no useful Debug). Same convention the rest
    // of the workspace uses for resource/guard types.
    #[test]
    fn bootstrap_missing_file_is_io_error() {
        assert!(matches!(
            bootstrap_from_config_path(r"C:\nope\does-not-exist.cfg"),
            Err(BootstrapError::Io)
        ));
    }

    #[test]
    fn bootstrap_garbage_config_is_bad_config() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("vfs-shim-badcfg-{}.bin", std::process::id()));
        std::fs::write(&path, [0u8, 1]).unwrap(); // too short for the header
        assert!(matches!(
            bootstrap_from_config_path(path.to_str().unwrap()),
            Err(BootstrapError::Config(_))
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// A config from another build is refused at bootstrap, before the ring is touched, with an
    /// error that names the versions.
    #[test]
    fn bootstrap_refuses_a_config_from_another_build() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("vfs-shim-oldcfg-{}.bin", std::process::id()));
        // A version-1 config: root, overlay, "VFS1", n_static, snapshot.
        let mut v1 = Vec::new();
        v1.extend_from_slice(&1u32.to_le_bytes());
        v1.push(b'R');
        v1.extend_from_slice(&0u32.to_le_bytes());
        v1.extend_from_slice(b"VFS1");
        v1.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, &v1).unwrap();
        let r = bootstrap_from_config_path(path.to_str().unwrap());
        assert!(matches!(
            r,
            Err(BootstrapError::Config(ConfigError::Unversioned))
        ));
        let mut v3 = shimcfg::encode_config("R");
        v3[4..8].copy_from_slice(&3u32.to_le_bytes());
        std::fs::write(&path, &v3).unwrap();
        let r = bootstrap_from_config_path(path.to_str().unwrap());
        assert!(matches!(
            r,
            Err(BootstrapError::Config(ConfigError::Version {
                found: 3,
                ..
            }))
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// What the launcher reads in the ready file for a config refusal: the bootstrap spelling
    /// (not the director's), carrying the message that names both versions.
    #[test]
    fn a_config_refusal_is_spelled_for_the_ready_file() {
        let e = BootstrapError::Config(ConfigError::Version {
            found: 3,
            expected: 2,
        });
        let content = bootstrap_failed_content(&e);
        assert!(
            content.starts_with(vfs_env::READY_BOOTSTRAP_FAILED_PREFIX),
            "{content}"
        );
        assert!(
            content.contains("version 3") && content.contains("version 2"),
            "{content}"
        );
        assert!(!content.starts_with(vfs_env::READY_FUSE_FAILED_PREFIX));
        let io = bootstrap_failed_content(&BootstrapError::Io);
        assert_eq!(io, format!("{}Io", vfs_env::READY_BOOTSTRAP_FAILED_PREFIX));
    }
}
