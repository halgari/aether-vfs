//! CLI flag parsing for `--source`, `--write-layer` and `--root`.

/// Parse `TYPE:PATH@MOUNT` CLI source flags.
///
/// Precedence among several `--source` flags is declaration order (later
/// flag wins on a shared path) — the same flat-list sugar
/// [`vfs_control::config`] documents for `[[source]]`, not a per-flag numeric
/// layer. Every entry this builds targets root `0`: `--root` declares where
/// roots are, but a `--source` cannot yet name a non-default root (config
/// files can, via `[[root]]` + `root =`).
pub fn parse_source_flag(s: &str) -> Result<vfs_control::SourceEntry, String> {
    let (ty, rest) = s
        .split_once(':')
        .ok_or_else(|| format!("source flag needs TYPE:PATH…, got {s:?}"))?;
    let ty = ty.to_ascii_lowercase();
    // A layer is only ever a root's write layer, which has its own flag.
    if ty == "layer" {
        return Err(format!(
            "--source {s:?}: a layer is a write layer, not a content source; \
             use --write-layer layer:NAME"
        ));
    }

    let (path, mount) = if let Some((p, m)) = rest.rsplit_once('@') {
        (p.to_string(), m.to_string())
    } else {
        (rest.to_string(), "/".to_string())
    };

    if path.is_empty() {
        return Err(format!("empty path in source flag: {s:?}"));
    }
    // The old syntax was `TYPE:PATH@MOUNT#LAYER`; `#LAYER` was removed when
    // `layer` left the config (precedence is now flag order). `rsplit_once('@')`
    // has no idea that suffix is gone, so a leftover `#20` from a command
    // line nobody updated silently becomes part of `mount` instead of being
    // stripped — the source then mounts at a mangled, unreachable prefix
    // (`sessions.rs`'s `is_root` check sees `"/#20"`, not `"/"`) and the
    // session starts cleanly while serving nothing where the caller expected
    // root content. Reject it loudly instead.
    if let Some((_, suffix)) = mount.split_once('#') {
        return Err(format!(
            "source flag {s:?}: the '#{suffix}' layer suffix no longer exists \
             (precedence is now --source flag order) — use TYPE:PATH@MOUNT"
        ));
    }

    let spec = match ty.as_str() {
        "disk" => vfs_control::SourceSpec::Disk { path },
        "zip" => vfs_control::SourceSpec::Zip { path },
        "http" => vfs_control::SourceSpec::Http { url: path },
        "remote" => vfs_control::SourceSpec::Remote { endpoint: path },
        other => return Err(format!("unknown source type {other:?}")),
    };

    Ok(vfs_control::SourceEntry {
        spec,
        mount,
        root: 0,
        // `--source` declares content. A write layer is a different fact
        // about a session (where its writes land), so it gets its own flag
        // rather than a magic suffix on this one — see `--write-layer`.
        write_layer: false,
        cache_key: None,
    })
}

/// The `--write-layer DIR|layer:NAME` flag as a config entry: root 0's
/// writable upper.
///
/// A separate flag rather than a `--source` spelling because it is a
/// different fact — `--source` says what the session *serves*, this says
/// where its writes *land*, seeded from whatever the sources hold. Either a
/// disk directory, or `layer:NAME`, a named persistent layer in the daemon's
/// storage (an empty name is refused). Always root 0 (the CLI has no syntax
/// for naming another root), always mounted at the root (the upper covers
/// the whole root by construction).
pub fn write_layer_flag_entry(path: &str) -> Result<vfs_control::SourceEntry, String> {
    let spec = match path.strip_prefix("layer:") {
        Some("") => {
            return Err(format!(
                "--write-layer {path:?}: `layer:` needs a layer name (layer:NAME)"
            ));
        }
        Some(name) => vfs_control::SourceSpec::Layer {
            name: name.to_string(),
        },
        None => vfs_control::SourceSpec::Disk {
            path: path.to_string(),
        },
    };
    Ok(vfs_control::SourceEntry {
        spec,
        mount: "/".to_string(),
        root: 0,
        write_layer: true,
        cache_key: None,
    })
}

/// Parse one `--root ID=NAME=LOCATION` flag into a `[[root]]` entry.
///
/// Only the first two `=` split, so a location may itself contain one. The
/// name is what a launch path spells as `{NAME}\…`; the location is where
/// the program sees the root (a `C:\…` path inside the prefix on Linux).
pub(crate) fn parse_root_flag(s: &str) -> Result<vfs_control::RootEntry, String> {
    let bad = || format!("--root expects ID=NAME=LOCATION, got `{s}`");
    let mut parts = s.splitn(3, '=');
    let (Some(id), Some(name), Some(location)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad());
    };
    let id: u32 = id.trim().parse().map_err(|_| bad())?;
    if name.is_empty() || location.is_empty() {
        return Err(bad());
    }
    Ok(vfs_control::RootEntry {
        id,
        name: name.to_string(),
        path: location.to_string(),
    })
}

/// Every `--root` flag as `[[root]]` entries. Once any root is declared the
/// config has a `[[root]]` table, and `--source`/`--write-layer` target root
/// 0 — so root 0 must be among them, or the config names a root it never
/// declares.
pub fn root_flag_entries(flags: &[String]) -> Result<Vec<vfs_control::RootEntry>, String> {
    let roots = flags
        .iter()
        .map(|f| parse_root_flag(f))
        .collect::<Result<Vec<_>, _>>()?;
    if !roots.is_empty() && !roots.iter().any(|r| r.id == 0) {
        return Err(
            "--root: declare root 0 too; --source and --write-layer target root 0".to_string(),
        );
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--source` and `--write-layer` must not be confusable: a source is
    /// content, a write layer is where writes land. The flag that reaches the
    /// daemon has to carry `write_layer: true`, or the CLI silently declares
    /// one more mod directory instead of a copy-up target.
    #[test]
    fn write_layer_flag_declares_a_write_layer_not_a_source() {
        let e = write_layer_flag_entry(r#"C:\mods\overwrite"#).unwrap();
        assert!(e.write_layer, "the --write-layer flag must set the flag");
        assert_eq!(
            e.spec,
            vfs_control::SourceSpec::Disk {
                path: r#"C:\mods\overwrite"#.into()
            }
        );
        assert_eq!(e.mount, "/", "a write layer covers the whole root");
        assert_eq!(e.root, 0);
        // The contrast that makes the assertion above mean something.
        assert!(
            !parse_source_flag(r#"disk:C:\mods\overwrite"#)
                .unwrap()
                .write_layer
        );
        // …and the config it produces is one the daemon will accept.
        vfs_control::SessionConfig {
            sources: vec![e],
            ..Default::default()
        }
        .validate_roots()
        .expect("the flag must produce a config that validates");
    }

    #[test]
    fn parse_root_flag_splits_id_name_location() {
        let r = parse_root_flag(r"0=Games=C:\Games\Fixture").unwrap();
        assert_eq!(r.id, 0);
        assert_eq!(r.name, "Games");
        assert_eq!(r.path, r"C:\Games\Fixture");
        // Only the first two `=` split: a location may contain one.
        assert_eq!(parse_root_flag("1=Docs=C:\\a=b").unwrap().path, "C:\\a=b");
    }

    #[test]
    fn parse_root_flag_rejects_malformed() {
        for bad in [
            "Games=C:\\x",
            "x=Games=C:\\x",
            "0=C:\\x",
            "0==C:\\x",
            "0=Games=",
        ] {
            let e = parse_root_flag(bad).unwrap_err();
            assert!(e.contains("--root") && e.contains(bad), "{bad}: {e}");
        }
    }

    #[test]
    fn root_flags_must_declare_root_zero() {
        assert!(root_flag_entries(&[]).unwrap().is_empty());
        let e = root_flag_entries(&["1=Docs=C:\\docs".to_string()]).unwrap_err();
        assert_eq!(
            e,
            "--root: declare root 0 too; --source and --write-layer target root 0"
        );
        let ok = root_flag_entries(&[
            "0=Games=C:\\games".to_string(),
            "1=Docs=C:\\docs".to_string(),
        ])
        .unwrap();
        assert_eq!(ok.iter().map(|r| r.id).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn parse_source_flag_disk_windows_path() {
        let e = parse_source_flag(r#"disk:C:\mods\SkyUI@/"#).unwrap();
        assert_eq!(
            e.spec,
            vfs_control::SourceSpec::Disk {
                path: r#"C:\mods\SkyUI"#.into()
            }
        );
        assert_eq!(e.mount, "/");
        assert_eq!(e.root, 0);
    }

    #[test]
    fn parse_source_flag_defaults() {
        let e = parse_source_flag("zip:C:/base.zip").unwrap();
        assert_eq!(
            e.spec,
            vfs_control::SourceSpec::Zip {
                path: "C:/base.zip".into()
            }
        );
        assert_eq!(e.mount, "/");
        assert_eq!(e.root, 0);
    }

    #[test]
    fn parse_source_flag_mount_without_at() {
        let e = parse_source_flag("disk:C:/mods@/Data").unwrap();
        assert_eq!(e.mount, "/Data");
        assert_eq!(e.root, 0);
    }

    #[test]
    fn parse_source_flag_rejects_unknown_type() {
        assert!(parse_source_flag("blob:C:/x").is_err());
    }

    /// The pre-2b syntax was `TYPE:PATH@MOUNT#LAYER`. Task 2 dropped `layer`
    /// from config but `parse_source_flag`'s `rsplit_once('@')` has no idea
    /// the `#LAYER` suffix is gone, so a stale command line's `#20` used to
    /// become part of `mount` silently — `sessions::add_source`'s `is_root`
    /// check then sees `"/#20"`, not `"/"`, and the source mounts at an
    /// unreachable prefix instead of the root the caller intended, with the
    /// session starting cleanly and serving nothing where expected. This
    /// must be a loud parse error instead.
    #[test]
    fn parse_source_flag_rejects_the_removed_layer_suffix() {
        let err = parse_source_flag(r#"disk:C:\mods\SkyUI@/#20"#).unwrap_err();
        assert!(
            err.contains('#') && err.contains("layer"),
            "error should name the removed '#LAYER' syntax: {err}"
        );
    }

    /// A layer is only ever a write layer; `--source layer:NAME` would be
    /// refused later by `validate_roots` with config-file advice. The flag
    /// parser refuses it at once and names the flag to use.
    #[test]
    fn parse_source_flag_refuses_a_layer() {
        for flag in ["layer:prof", "LAYER:prof@/"] {
            let e = parse_source_flag(flag).unwrap_err();
            assert!(e.contains("--write-layer layer:NAME"), "{flag}: {e}");
        }
    }

    #[test]
    fn write_layer_flag_entry_layer_prefix_builds_a_layer() {
        let e = write_layer_flag_entry("layer:prof").unwrap();
        assert_eq!(
            e.spec,
            vfs_control::SourceSpec::Layer {
                name: "prof".into()
            }
        );
        assert!(e.write_layer);
        let d = write_layer_flag_entry("C:/scratch").unwrap();
        assert!(matches!(d.spec, vfs_control::SourceSpec::Disk { .. }));
    }

    /// `layer:` with no name would reach the daemon as a layer called "",
    /// which `Storage::layer` would happily create. Refused at parse time.
    #[test]
    fn an_empty_layer_name_is_refused_by_both_flags() {
        for flag in ["layer:", "layer:@/"] {
            let e = parse_source_flag(flag).unwrap_err();
            assert!(e.contains("--write-layer layer:NAME"), "{flag}: {e}");
        }
        let e = write_layer_flag_entry("layer:").unwrap_err();
        assert!(
            e.contains("layer name") && e.contains("--write-layer"),
            "{e}"
        );
    }
}
