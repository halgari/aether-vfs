//! Per-user daemon discovery file (endpoint + pid).
//!
//! M0 transport is loopback TCP; the file lets clients find the ephemeral port
//! without an extra broker. Override path with `VFS_DISCOVERY_PATH`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Discovery {
    pub endpoint: String,
    pub pid: u32,
}

/// Default discovery path: `%LOCALAPPDATA%/vfs-director/discovery.json` on
/// Windows, `~/.cache/vfs-director/discovery.json` elsewhere.
pub fn default_discovery_path() -> PathBuf {
    if let Some(p) = vfs_env::text(vfs_env::DISCOVERY_PATH) {
        return PathBuf::from(p);
    }
    if let Ok(base) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(base)
            .join("vfs-director")
            .join("discovery.json");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("vfs-director")
            .join("discovery.json");
    }
    std::env::temp_dir()
        .join("vfs-director")
        .join("discovery.json")
}

pub fn write_discovery(path: &Path, d: &Discovery) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(d).map_err(|e| format!("serialize discovery: {e}"))?;
    // Atomic-ish replace via temp + rename.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename discovery: {e}"))?;
    Ok(())
}

pub fn read_discovery(path: &Path) -> Result<Discovery, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parse discovery: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_discovery_file() {
        let dir = std::env::temp_dir().join(format!("vfs-disc-{}", std::process::id()));
        let path = dir.join("discovery.json");
        let d = Discovery {
            endpoint: "127.0.0.1:7000".into(),
            pid: 42,
        };
        write_discovery(&path, &d).unwrap();
        let got = read_discovery(&path).unwrap();
        assert_eq!(got, d);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
