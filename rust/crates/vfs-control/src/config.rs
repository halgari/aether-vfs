//! Declarative scenario config (TOML primary, JSON accepted). One serde schema
//! shared by the daemon, the `vfs` CLI, and the integration tests so there is a
//! single source of truth for "how a session is set up".
//!
//! A session virtualizes one or more real filesystem locations — **roots** —
//! each served by exactly one provider (see
//! `docs/superpowers/specs/2026-08-13-pluggable-providers-design.md` §6).
//! Roots are declared with an id, a name, and a host path:
//!
//! ```toml
//! [session]
//! name = "skyrim-test"
//!
//! [[root]]
//! id   = 0
//! name = "game"
//! path = "C:/Games/Skyrim"
//!
//! [[root]]
//! id   = 1
//! name = "docs"
//! path = "C:/Users/me/Documents/My Games/Skyrim"
//!
//! [[source]]
//! type  = "zip"
//! path  = "C:/GameLayers/base.zip"
//! root  = 0
//!
//! [[source]]
//! type  = "disk"
//! path  = "C:/mods/SkyUI"
//! root  = 0
//!
//! [[source]]
//! type  = "disk"
//! path  = "C:/scratch/skyrim-docs"
//! root  = 1
//!
//! # Where root 0's writes go. Without this the root is read-only content and
//! # the director refuses an in-place edit of it (gate 4) — see
//! # `SourceEntry::write_layer`.
//! [[source]]
//! type        = "disk"
//! path        = "C:/mods/overwrite"
//! root        = 0
//! write_layer = true
//!
//! [launch]
//! exec = "SkyrimSE.exe"
//! ```
//!
//! **The flat `[[source]]` list is documented sugar, deprecated but kept for
//! compatibility.** A `source` entry with no `root` defaults to root `0`, and
//! entries sharing a root are combined in declaration order — later entries
//! win on a shared path — which is exactly what a single-root config with no
//! `[[root]]` table at all has always meant. Prefer declaring roots and a
//! `root` per source; the flat form is for configs that only ever needed one
//! root.

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
    /// An in-memory name→content map (`vfs_compose::MemoryProvider`).
    ///
    /// Content is UTF-8 text, not arbitrary bytes: this variant exists so a
    /// scenario file can declare one (an INI, say) without a host writing
    /// Rust, and TOML/JSON have no ergonomic way to spell binary. A host
    /// supplying real bytes constructs `vfs_compose::MemoryProvider` directly
    /// instead of going through config — see that type's doc for why it
    /// lives in `vfs-compose` rather than here.
    ///
    /// **The daemon refuses it, and this is where to find that out rather than
    /// at `vfs up --config`.** `source.proto` has no `Kind::Memory`, so the gRPC
    /// control plane has no wire shape for an inline name→bytes map, and
    /// `vfs-directord`'s `SourceSpec` → proto conversion returns an error naming
    /// this variant. Reachable via `vfs_source::build_provider` (in-process) and
    /// as `vfs_embed::MemoryProvider` from any embedding host — which is where
    /// the Node binding's `memory()` primitive comes from.
    ///
    /// It stays on this enum rather than being removed because the enum is
    /// `vfs-control`'s *config format*, which is broader than what the daemon's
    /// RPC surface carries today, and because `vfs_source::build_provider`
    /// really does build it. Wiring it end to end is a proto change: add
    /// `MemorySource { map<string, bytes> files }` to `source.proto`, regenerate
    /// (`bin/regen-protocol`), and handle it in `add_source`. Nobody has needed
    /// it over the wire — the hosts that want an in-memory provider compose one
    /// in code — so it is stated, not built.
    Memory { files: BTreeMap<String, String> },
}

fn default_mount() -> String {
    "/".to_string()
}
fn default_true() -> bool {
    true
}

/// One `[[root]]` entry: a real filesystem location the session virtualizes,
/// served by exactly one provider. `id` is what travels the hot path and the
/// wire; `name` exists for config, logs, and error messages. `path` is the
/// host directory this root maps onto.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootEntry {
    pub id: u32,
    pub name: String,
    pub path: String,
}

/// One `[[source]]` entry: a spec, where it mounts under its root, and which
/// root it belongs to.
///
/// `layer` is gone: combining several sources at the same root is no longer
/// an implicit numeric ordering — it is the flat-list sugar's own rule
/// (declaration order, later wins) or, outside the sugar, an explicit
/// `layered(...)` in the provider graph. See the module doc for the sugar's
/// exact meaning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    #[serde(flatten)]
    pub spec: SourceSpec,
    #[serde(default = "default_mount")]
    pub mount: String,
    /// Which declared root this source belongs to. Defaults to `0`, the root
    /// every single-root config (no `[[root]]` table) has always used.
    #[serde(default)]
    pub root: u32,
    /// `write_layer = true` makes this source the root's **writable layer**
    /// rather than one more content layer: it sits above the whole graph, and
    /// a write to content only a read-only source (an archive) holds is
    /// copied up into it instead of being refused. Without one, every source
    /// in a root composes as a sibling, and an in-place edit of archive
    /// content — what a mod tool or an INI writer does — fails, because
    /// layering can route a write but cannot seed a copy from a lower layer.
    ///
    /// Must be a writable source (`type = "disk"`), must mount at the root,
    /// and at most one per root — see
    /// [`SessionConfig::validate_roots`].
    #[serde(default)]
    pub write_layer: bool,
}

/// The `[launch]` block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchConfig {
    pub exec: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_true")]
    pub wait: bool,
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

/// A whole scenario: session meta, roots, sources, an optional launch,
/// optional cache.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    #[serde(default)]
    pub session: SessionMeta,
    /// Declared roots. May be empty — a config with no `[[root]]` table is
    /// the single-root case, root `0`, implicitly.
    #[serde(default, rename = "root")]
    pub roots: Vec<RootEntry>,
    #[serde(default, rename = "source")]
    pub sources: Vec<SourceEntry>,
    #[serde(default)]
    pub launch: Option<LaunchConfig>,
    #[serde(default)]
    pub cache: Option<CacheConfig>,
}

impl SessionConfig {
    /// Validate the relationship between declared `[[root]]` entries and the
    /// `root` each source names.
    ///
    /// A config with **no** `[[root]]` table is the flat-list-sugar case:
    /// nothing is declared, so every source implicitly targets root `0` and
    /// there is nothing to check against. Once `[[root]]` is present,
    /// though, every source's `root` must name a declared id (including `0`
    /// — declaring roots at all means declaring all of them), and declared
    /// ids must be unique. Without this, a source naming an undeclared root
    /// would silently produce a provider addressable by a number nothing
    /// documents, and a duplicate `[[root]]` id would silently pick
    /// whichever entry a `BTreeMap`/`HashMap` insert happened to keep.
    ///
    /// A root declared with no source is not an error: it simply produces no
    /// provider for that root, which is a valid (if likely accidental)
    /// config.
    ///
    /// Also checks the `write_layer` flag, which is a per-root singleton, has
    /// to be writable, and covers the whole root: two of them, one at a
    /// sub-path, or one on a source that cannot be written would each leave a
    /// declaration silently doing nothing (the second replacing the first, a
    /// prefix being ignored) or failing far from where it was written — and a
    /// session that believes it has copy-on-write and does not is the failure
    /// this flag exists to prevent. Checked before the flat-list early return,
    /// because the flat form can declare a write layer too.
    pub fn validate_roots(&self) -> Result<(), String> {
        let mut write_layer_roots = std::collections::HashSet::new();
        for entry in self.sources.iter().filter(|e| e.write_layer) {
            if !write_layer_roots.insert(entry.root) {
                return Err(format!(
                    "root {} declares more than one write_layer source",
                    entry.root
                ));
            }
            let mount = entry.mount.trim();
            if !(mount.is_empty() || mount == "/" || mount == "\\") {
                return Err(format!(
                    "write_layer source for root {} mounts at {:?}; a write layer is the \
                     root's writable upper and cannot be scoped to a sub-path",
                    entry.root, entry.mount
                ));
            }
            // `disk` is the only source kind accepted as a write layer. A zip
            // is read-only and the `remote` wire has no write ops at all (it
            // clamps to `Access::Read`), so either is refused for being
            // unwritable. `memory` is writable too but is deliberately not
            // accepted here: a write layer's whole point is a copy-up
            // destination that survives independently of the graph it sits
            // over, and a host that wants a writable in-memory root mounts a
            // `MemoryProvider` as an ordinary sibling instead (it needs no
            // copy-up — it is already `Access::ReadWrite`). Say so here,
            // where the author can see which line is wrong, rather than as a
            // status code out of `AddSource`.
            if !matches!(entry.spec, SourceSpec::Disk { .. }) {
                return Err(format!(
                    "write_layer source for root {} is {:?}; only a disk source may serve as a \
                     write layer",
                    entry.root, entry.spec
                ));
            }
        }
        if self.roots.is_empty() {
            return Ok(());
        }
        let mut seen = std::collections::HashSet::new();
        for r in &self.roots {
            if !seen.insert(r.id) {
                return Err(format!("duplicate [[root]] id {}", r.id));
            }
        }
        for entry in &self.sources {
            if !seen.contains(&entry.root) {
                return Err(format!(
                    "source targets root {} which no [[root]] entry declares",
                    entry.root
                ));
            }
        }
        Ok(())
    }
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

[[source]]
type  = "disk"
path  = "C:/mods/SkyUI"
mount = "/"

[launch]
exec = "SkyrimSE.exe"
args = ["--foo"]
"#;
        let cfg: SessionConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.session.name.as_deref(), Some("skyrim-test"));
        assert!(cfg.roots.is_empty(), "no [[root]] table declared");
        assert_eq!(cfg.sources.len(), 2);
        assert_eq!(cfg.sources[0].spec, SourceSpec::Zip { path: "C:/GameLayers/base.zip".into() });
        assert_eq!(cfg.sources[0].mount, "/"); // defaulted
        assert_eq!(cfg.sources[0].root, 0); // defaulted — the flat-list sugar
        assert_eq!(cfg.sources[1].root, 0);
        let launch = cfg.launch.unwrap();
        assert_eq!(launch.exec, "SkyrimSE.exe");
        assert!(launch.wait); // defaulted true
    }

    #[test]
    fn parses_json_equivalent() {
        let json = r#"
        { "session": {"name": "t"},
          "source": [ {"type":"disk","path":"C:/x","root":1} ],
          "launch": {"exec":"a.exe","wait":false} }"#;
        let cfg: SessionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.sources[0].spec, SourceSpec::Disk { path: "C:/x".into() });
        assert_eq!(cfg.sources[0].root, 1);
        assert!(!cfg.launch.unwrap().wait);
    }

    #[test]
    fn parses_a_declared_root_table() {
        let toml = r#"
[[root]]
id   = 0
name = "game"
path = "C:/Games/Skyrim"

[[root]]
id   = 1
name = "docs"
path = "C:/Users/me/Documents/My Games/Skyrim"

[[source]]
type = "disk"
path = "C:/Games/Skyrim"
root = 0

[[source]]
type = "disk"
path = "C:/scratch/skyrim-docs"
root = 1
"#;
        let cfg: SessionConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.roots.len(), 2);
        assert_eq!(cfg.roots[0], RootEntry { id: 0, name: "game".into(), path: "C:/Games/Skyrim".into() });
        assert_eq!(cfg.roots[1].name, "docs");
        assert_eq!(cfg.sources[0].root, 0);
        assert_eq!(cfg.sources[1].root, 1);
    }

    /// A source is content unless it says otherwise: every config written
    /// before `write_layer` existed keeps meaning exactly what it meant, and
    /// a config that wants copy-on-write says so on one source.
    #[test]
    fn write_layer_defaults_off_and_parses_where_declared() {
        let toml = r#"
[[source]]
type = "zip"
path = "C:/layers/base.zip"

[[source]]
type        = "disk"
path        = "C:/mods/overwrite"
write_layer = true
"#;
        let cfg: SessionConfig = toml::from_str(toml).unwrap();
        assert!(!cfg.sources[0].write_layer, "an undeclared source is content");
        assert!(cfg.sources[1].write_layer);
        cfg.validate_roots().expect("one write layer on one root is valid");
    }

    /// Two write layers on one root would mean one of the two declarations
    /// does nothing (the second replaces the first) — a session whose author
    /// believes writes go somewhere they do not.
    #[test]
    fn validate_rejects_two_write_layers_on_one_root() {
        let toml = r#"
[[source]]
type        = "disk"
path        = "C:/a"
write_layer = true

[[source]]
type        = "disk"
path        = "C:/b"
write_layer = true
"#;
        let cfg: SessionConfig = toml::from_str(toml).unwrap();
        let err = cfg.validate_roots().unwrap_err();
        assert!(err.contains("write_layer"), "{err}");
    }

    /// Only a disk source is writable, so nothing else can be a write layer.
    /// Rejected here, at the line the author wrote, rather than as a status
    /// code out of `AddSource` when the session composes.
    #[test]
    fn validate_rejects_a_write_layer_that_is_not_a_disk_source() {
        let toml = r#"
[[source]]
type        = "zip"
path        = "C:/layers/base.zip"
write_layer = true
"#;
        let cfg: SessionConfig = toml::from_str(toml).unwrap();
        let err = cfg.validate_roots().unwrap_err();
        assert!(err.contains("disk"), "{err}");
    }

    /// The write layer is the root's writable upper, which covers the whole
    /// root; a `mount` prefix on it would be silently ignored.
    #[test]
    fn validate_rejects_a_write_layer_scoped_to_a_sub_path() {
        let toml = r#"
[[source]]
type        = "disk"
path        = "C:/a"
mount       = "Data/SomeMod"
write_layer = true
"#;
        let cfg: SessionConfig = toml::from_str(toml).unwrap();
        let err = cfg.validate_roots().unwrap_err();
        assert!(err.contains("Data/SomeMod"), "{err}");
    }

    /// A config authored before `layer` was removed still parses: serde
    /// silently ignores unknown fields, so a stray `layer = N` left over
    /// from an old config is harmless rather than a parse error. This is
    /// the compatibility guarantee the flat-list sugar depends on.
    #[test]
    fn a_stray_layer_key_from_an_old_config_is_ignored_not_rejected() {
        let toml = r#"
[[source]]
type  = "disk"
path  = "C:/x"
layer = 20
"#;
        let cfg: SessionConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.sources[0].root, 0);
    }

    #[test]
    fn validate_roots_is_a_no_op_when_no_root_table_is_declared() {
        let cfg = SessionConfig {
            sources: vec![SourceEntry {
                spec: SourceSpec::Disk { path: "C:/x".into() },
                mount: "/".into(),
                root: 7, // would be undeclared if any [[root]] existed
                write_layer: false,
            }],
            ..Default::default()
        };
        assert!(cfg.validate_roots().is_ok());
    }

    #[test]
    fn validate_roots_rejects_a_source_naming_an_undeclared_root() {
        let cfg = SessionConfig {
            roots: vec![RootEntry { id: 0, name: "game".into(), path: "C:/g".into() }],
            sources: vec![SourceEntry {
                spec: SourceSpec::Disk { path: "C:/x".into() },
                mount: "/".into(),
                root: 1,
                write_layer: false,
            }],
            ..Default::default()
        };
        let err = cfg.validate_roots().unwrap_err();
        assert!(err.contains('1'), "error should name the offending root: {err}");
    }

    #[test]
    fn validate_roots_rejects_duplicate_root_ids() {
        let cfg = SessionConfig {
            roots: vec![
                RootEntry { id: 0, name: "a".into(), path: "C:/a".into() },
                RootEntry { id: 0, name: "b".into(), path: "C:/b".into() },
            ],
            ..Default::default()
        };
        let err = cfg.validate_roots().unwrap_err();
        assert!(err.contains('0'), "error should name the duplicated id: {err}");
    }

    #[test]
    fn validate_roots_accepts_a_root_declared_with_no_sources() {
        let cfg = SessionConfig {
            roots: vec![
                RootEntry { id: 0, name: "game".into(), path: "C:/g".into() },
                RootEntry { id: 1, name: "docs".into(), path: "C:/d".into() },
            ],
            sources: vec![SourceEntry {
                spec: SourceSpec::Disk { path: "C:/x".into() },
                mount: "/".into(),
                root: 0,
                write_layer: false,
            }],
            ..Default::default()
        };
        assert!(cfg.validate_roots().is_ok(), "root 1 has no source, which is allowed");
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
