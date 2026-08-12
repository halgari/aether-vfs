//! Declarative scenario config (TOML primary, JSON accepted). One serde schema
//! shared by the daemon, the `vfs` CLI, and the integration tests so there is a
//! single source of truth for "how a session is set up".
//!
//! ```toml
//! [session]
//! name = "skyrim-test"
//!
//! [[source]]
//! type  = "zip"
//! path  = "C:/GameLayers/base.zip"
//! mount = "/"
//! layer = 0
//!
//! [[source]]
//! type  = "disk"
//! path  = "C:/mods/SkyUI"
//! layer = 20
//!
//! [launch]
//! exec = "SkyrimSE.exe"
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A file source. `type` is the discriminant in TOML/JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SourceSpec {
    Disk { path: String },
    Zip { path: String },
    Http { url: String },
    Remote { endpoint: String },
}

fn default_mount() -> String {
    "/".to_string()
}
fn default_true() -> bool {
    true
}

/// One `[[source]]` entry: a spec plus where it mounts and its precedence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    #[serde(flatten)]
    pub spec: SourceSpec,
    #[serde(default = "default_mount")]
    pub mount: String,
    #[serde(default)]
    pub layer: i32,
}

/// The `[launch]` block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchConfig {
    pub exec: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_true")]
    pub wait: bool,
    #[serde(default = "default_true")]
    pub hollow_pe: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// The `[cache]` block. Honored from M2; ignored earlier.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheConfig {
    pub block_size: Option<String>,
    pub ram_budget: Option<String>,
    pub dir: Option<String>,
}

/// The `[session]` block.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default)]
    pub name: Option<String>,
}

/// A whole scenario: session meta, sources, an optional launch, optional cache.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    #[serde(default)]
    pub session: SessionMeta,
    #[serde(default, rename = "source")]
    pub sources: Vec<SourceEntry>,
    #[serde(default)]
    pub launch: Option<LaunchConfig>,
    #[serde(default)]
    pub cache: Option<CacheConfig>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read {0}: {1}")]
    Read(String, #[source] std::io::Error),
    #[error("parse TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("parse JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown config extension for {0} (use .toml or .json)")]
    Extension(String),
}

/// Load a scenario from a `.toml` or `.json` file (chosen by extension).
pub fn load(path: impl AsRef<Path>) -> Result<SessionConfig, ConfigError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::Read(path.display().to_string(), e))?;
    match path.extension().and_then(|s| s.to_str()) {
        Some("toml") => Ok(toml::from_str(&text)?),
        Some("json") => Ok(serde_json::from_str(&text)?),
        _ => Err(ConfigError::Extension(path.display().to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toml_scenario() {
        let toml = r#"
[session]
name = "skyrim-test"

[[source]]
type  = "zip"
path  = "C:/GameLayers/base.zip"
layer = 0

[[source]]
type  = "disk"
path  = "C:/mods/SkyUI"
mount = "/"
layer = 20

[launch]
exec = "SkyrimSE.exe"
args = ["--foo"]
"#;
        let cfg: SessionConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.session.name.as_deref(), Some("skyrim-test"));
        assert_eq!(cfg.sources.len(), 2);
        assert_eq!(cfg.sources[0].spec, SourceSpec::Zip { path: "C:/GameLayers/base.zip".into() });
        assert_eq!(cfg.sources[0].mount, "/"); // defaulted
        assert_eq!(cfg.sources[1].layer, 20);
        let launch = cfg.launch.unwrap();
        assert_eq!(launch.exec, "SkyrimSE.exe");
        assert!(launch.wait && launch.hollow_pe); // defaulted true
    }

    #[test]
    fn parses_json_equivalent() {
        let json = r#"
        { "session": {"name": "t"},
          "source": [ {"type":"disk","path":"C:/x","layer":5} ],
          "launch": {"exec":"a.exe","wait":false} }"#;
        let cfg: SessionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.sources[0].spec, SourceSpec::Disk { path: "C:/x".into() });
        assert_eq!(cfg.sources[0].layer, 5);
        assert!(!cfg.launch.unwrap().wait);
    }

    #[test]
    fn load_detects_extension() {
        let dir = std::env::temp_dir().join(format!("vfs-cfg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let toml_path = dir.join("s.toml");
        std::fs::write(
            &toml_path,
            r#"
[[source]]
type = "disk"
path = "C:/a"
"#,
        )
        .unwrap();
        let cfg = load(&toml_path).unwrap();
        assert_eq!(cfg.sources.len(), 1);

        let json_path = dir.join("s.json");
        std::fs::write(
            &json_path,
            r#"{"source":[{"type":"zip","path":"C:/b.zip"}]}"#,
        )
        .unwrap();
        let cfg = load(&json_path).unwrap();
        assert!(matches!(cfg.sources[0].spec, SourceSpec::Zip { .. }));

        let bad = dir.join("s.txt");
        std::fs::write(&bad, "x").unwrap();
        assert!(matches!(load(&bad), Err(ConfigError::Extension(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
