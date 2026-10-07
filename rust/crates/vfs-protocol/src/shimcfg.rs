//! Shim config wire codec: the root and the static-import table, packed into the
//! byte buffer the injected DLL reads on bootstrap.
//!
//! The host (vfs-embed, or the director) writes it and the shim (and vfs-inject,
//! for the static-import rows) reads it. Both halves live here, in `vfs-protocol`
//! (portable, no OS dependency), because a native Linux host must build a shim
//! config for a Wine-hosted shim and the readers must agree with it byte for byte.
//!
//! The format is versioned. A host and a shim from different builds must fail
//! loudly at bootstrap rather than read each other's bytes as something else, so
//! every change to the layout bumps [`CONFIG_VERSION`] and [`decode_config`]
//! names both versions when they differ. (The ring `VERSION` check rejects a stale
//! shim later, at attach; this check runs first, during bootstrap.)

/// One static-import DLL virtualization: the EXE's import of `dll_name`
/// (final path component, e.g. `d3d11.dll`) is redirected pre-init to
/// `backing_path` (absolute Win32 path; NT `\??\` form also accepted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticImport {
    pub dll_name: String,
    pub backing_path: String,
}

/// First four bytes of every config. A pre-versioned config starts with a `u32`
/// root length instead, and this value read as one (about 1.1 billion) is not a
/// plausible length, so an old config is recognised by its missing magic.
pub const CONFIG_MAGIC: &[u8; 4] = b"VFSC";

/// The layout version this build writes and reads. Version 1 was the
/// pre-versioned layout (root, overlay, `VFS1` marker, static imports, tree
/// snapshot). Version 2 dropped the overlay and the snapshot.
pub const CONFIG_VERSION: u32 = 2;

/// A decoded config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShimConfig {
    pub root: String,
    pub static_imports: Vec<StaticImport>,
}

/// Why a config did not decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// No `VFSC` magic: the file is shorter than a header, or it predates
    /// versioning (a config written by an older host).
    Unversioned,
    /// A versioned config from a different build.
    Version { found: u32, expected: u32 },
    /// Truncated, over-long, or not UTF-8.
    Malformed,
}

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConfigError::Unversioned => write!(
                f,
                "shim config has no version header (written by an older host); this shim reads \
                 config version {CONFIG_VERSION}. Rebuild the host and the shim together."
            ),
            ConfigError::Version { found, expected } => write!(
                f,
                "shim config is version {found}, this shim reads version {expected}. Rebuild the \
                 host and the shim together."
            ),
            ConfigError::Malformed => write!(f, "shim config is truncated or malformed"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Encode a config with no static imports.
pub fn encode_config(root: &str) -> Vec<u8> {
    encode_config_full(root, &[])
}

/// Full config: root and the static-import table.
///
/// Wire format (all integers little-endian):
/// ```text
/// "VFSC"
/// [u32 version = 2]
/// [u32 root_len][root utf8]
/// [u32 n_static]
/// n times: [u32 name_len][name utf8][u32 backing_len][backing utf8]
/// ```
/// Nothing follows the table.
pub fn encode_config_full(root: &str, static_imports: &[StaticImport]) -> Vec<u8> {
    let root_b = root.as_bytes();
    let mut out = Vec::with_capacity(16 + root_b.len() + 64);
    out.extend_from_slice(CONFIG_MAGIC);
    out.extend_from_slice(&CONFIG_VERSION.to_le_bytes());
    out.extend_from_slice(&(root_b.len() as u32).to_le_bytes());
    out.extend_from_slice(root_b);
    out.extend_from_slice(&(static_imports.len() as u32).to_le_bytes());
    for e in static_imports {
        let n = e.dll_name.as_bytes();
        let b = e.backing_path.as_bytes();
        out.extend_from_slice(&(n.len() as u32).to_le_bytes());
        out.extend_from_slice(n);
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(b);
    }
    out
}

fn read_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off.checked_add(4)?)?.try_into().ok()?))
}

fn read_field(b: &[u8], off: usize) -> Option<(String, usize)> {
    let len = read_u32(b, off)? as usize;
    let start = off + 4;
    let end = start.checked_add(len)?;
    let s = std::str::from_utf8(b.get(start..end)?).ok()?.to_string();
    Some((s, end))
}

/// Decode a config buffer. Never panics.
pub fn decode_config(bytes: &[u8]) -> Result<ShimConfig, ConfigError> {
    if bytes.get(..4) != Some(&CONFIG_MAGIC[..]) {
        return Err(ConfigError::Unversioned);
    }
    let found = read_u32(bytes, 4).ok_or(ConfigError::Malformed)?;
    if found != CONFIG_VERSION {
        return Err(ConfigError::Version { found, expected: CONFIG_VERSION });
    }
    decode_v2_body(&bytes[8..]).ok_or(ConfigError::Malformed)
}

fn decode_v2_body(b: &[u8]) -> Option<ShimConfig> {
    let (root, mut off) = read_field(b, 0)?;
    let n = read_u32(b, off)? as usize;
    off += 4;
    let mut static_imports = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        let (dll_name, o1) = read_field(b, off)?;
        let (backing_path, o2) = read_field(b, o1)?;
        off = o2;
        static_imports.push(StaticImport { dll_name, backing_path });
    }
    (off == b.len()).then_some(ShimConfig { root, static_imports })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire bytes are a pinned format, so this asserts the exact encoding
    /// rather than a round trip: a round trip would pass even if both sides
    /// moved together, which is precisely the regression that would break an
    /// already-shipped shim.
    #[test]
    fn encode_config_full_layout_is_unchanged() {
        let out = encode_config_full(
            "R",
            &[StaticImport { dll_name: "a".into(), backing_path: "bb".into() }],
        );
        let mut want = Vec::new();
        want.extend_from_slice(b"VFSC");
        want.extend_from_slice(&2u32.to_le_bytes());
        want.extend_from_slice(&1u32.to_le_bytes());
        want.push(b'R');
        want.extend_from_slice(&1u32.to_le_bytes()); // n_static
        want.extend_from_slice(&1u32.to_le_bytes());
        want.push(b'a');
        want.extend_from_slice(&2u32.to_le_bytes());
        want.extend_from_slice(b"bb");
        assert_eq!(out, want);
    }

    #[test]
    fn config_round_trips() {
        let statics = vec![
            StaticImport { dll_name: "d3d11.dll".into(), backing_path: r"C:\M\d3d11.dll".into() },
            StaticImport { dll_name: "dxgi.dll".into(), backing_path: r"\??\C:\M\dxgi.dll".into() },
        ];
        let got = decode_config(&encode_config_full(r"C:\Game", &statics)).unwrap();
        assert_eq!(got, ShimConfig { root: r"C:\Game".into(), static_imports: statics });
        let bare = decode_config(&encode_config(r"C:\Game")).unwrap();
        assert!(bare.static_imports.is_empty());
    }

    #[test]
    fn a_pre_versioned_config_is_refused_by_name() {
        // Version 1: root, overlay, "VFS1", n_static, snapshot.
        let mut v1 = Vec::new();
        v1.extend_from_slice(&3u32.to_le_bytes());
        v1.extend_from_slice(b"C:\\");
        v1.extend_from_slice(&0u32.to_le_bytes());
        v1.extend_from_slice(b"VFS1");
        v1.extend_from_slice(&0u32.to_le_bytes());
        v1.extend_from_slice(b"SNAP");
        assert_eq!(decode_config(&v1), Err(ConfigError::Unversioned));
    }

    #[test]
    fn another_version_is_refused_with_both_numbers() {
        let mut bytes = encode_config("R");
        bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
        let err = decode_config(&bytes).unwrap_err();
        assert_eq!(err, ConfigError::Version { found: 3, expected: CONFIG_VERSION });
        let msg = err.to_string();
        assert!(msg.contains("version 3") && msg.contains("version 2"), "{msg}");
    }

    #[test]
    fn truncated_and_trailing_bytes_are_malformed() {
        let good = encode_config_full(
            "R",
            &[StaticImport { dll_name: "a".into(), backing_path: "b".into() }],
        );
        for n in 8..good.len() {
            assert_eq!(decode_config(&good[..n]), Err(ConfigError::Malformed), "len {n}");
        }
        let mut long = good.clone();
        long.push(0);
        assert_eq!(decode_config(&long), Err(ConfigError::Malformed));
        assert_eq!(decode_config(&[0u8, 1]), Err(ConfigError::Unversioned));
        assert_eq!(decode_config(b"VFSC\x02"), Err(ConfigError::Malformed));
    }
}
