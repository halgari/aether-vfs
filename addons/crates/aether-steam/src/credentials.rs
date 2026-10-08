//! Haskill's own saved Steam login. Haskill never reads or reuses the native
//! Steam client's files or cached login (enforced by
//! `tests/no_native_credentials.rs`); the only way to get credentials is
//! `haskill login steam`, which writes this file.
use crate::error::SteamError;
use crate::fsutil::{read_optional, write_atomic};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// A Steam account name plus the long-lived refresh token Steam issued to
/// Haskill. `Debug` never prints the token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteamCredentials {
    pub account_name: String,
    refresh_token: String,
    /// Steam Guard "remember this device" blob; sent on the next password
    /// login so Steam does not ask for an e-mail code again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard_data: Option<String>,
}

impl SteamCredentials {
    pub fn new(account_name: String, refresh_token: String, guard_data: Option<String>) -> Self {
        SteamCredentials {
            account_name,
            refresh_token,
            guard_data,
        }
    }

    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    /// The token's `exp` claim (Unix seconds), if the token is a readable JWT.
    pub fn expires_at(&self) -> Option<u64> {
        let payload = self.refresh_token.split('.').nth(1)?;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload.trim_end_matches('='))
            .ok()?;
        #[derive(Deserialize)]
        struct Claims {
            exp: u64,
        }
        serde_json::from_slice::<Claims>(&bytes).ok().map(|c| c.exp)
    }

    /// True when the token's `exp` is at or before `now_unix`. A token whose
    /// expiry cannot be read is assumed valid; Steam has the final word.
    pub fn is_expired_at(&self, now_unix: u64) -> bool {
        self.expires_at().is_some_and(|exp| exp <= now_unix)
    }
}

impl fmt::Debug for SteamCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SteamCredentials")
            .field("account_name", &self.account_name)
            .field("refresh_token", &"<redacted>")
            .field(
                "guard_data",
                &self.guard_data.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// The file holding Haskill's saved Steam login (JSON, mode 0600, parent
/// directory 0700).
#[derive(Clone, Debug)]
pub struct CredentialFile {
    path: PathBuf,
}

impl CredentialFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        CredentialFile { path: path.into() }
    }

    /// `$XDG_DATA_HOME/haskill/steam-login.json`, falling back to
    /// `$HOME/.local/share/haskill/steam-login.json` (Windows:
    /// `%LOCALAPPDATA%\haskill\steam-login.json`), next to Haskill's
    /// `config.toml`.
    pub fn default_path() -> Result<PathBuf, SteamError> {
        let base = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_DATA_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        };
        base.map(|b| b.join("haskill").join("steam-login.json"))
            .ok_or_else(|| SteamError::Protocol("cannot locate the Haskill data directory".into()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved login, or `None` when there is none. A file readable by
    /// group or others is tightened to 0600 before it is used.
    pub fn load(&self) -> Result<Option<SteamCredentials>, SteamError> {
        let Some(bytes) = read_optional(&self.path)? else {
            return Ok(None);
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&self.path)?.permissions().mode();
            if mode & 0o077 != 0 {
                tracing::warn!(path = %self.path.display(), "tightening Steam login file to 0600");
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        let creds = serde_json::from_slice(&bytes).map_err(|e| {
            SteamError::Protocol(format!(
                "{} is not a Haskill Steam login file ({e}); run `haskill login steam`",
                self.path.display()
            ))
        })?;
        Ok(Some(creds))
    }

    pub fn save(&self, creds: &SteamCredentials) -> Result<(), SteamError> {
        let json =
            serde_json::to_vec_pretty(creds).map_err(|e| SteamError::Protocol(e.to_string()))?;
        write_atomic(&self.path, &json, true)?;
        Ok(())
    }

    pub fn remove(&self) -> Result<(), SteamError> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: &str) -> String {
        let e = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        format!("{}.{}.sig", e(r#"{"typ":"JWT","alg":"EdDSA"}"#), e(claims))
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = CredentialFile::new(dir.path().join("haskill/steam-login.json"));
        assert!(file.load().unwrap().is_none());
        let c = SteamCredentials::new("alice".into(), jwt(r#"{"exp":100}"#), Some("g".into()));
        file.save(&c).unwrap();
        assert_eq!(file.load().unwrap(), Some(c));
        file.remove().unwrap();
        assert!(file.load().unwrap().is_none());
        file.remove().unwrap(); // removing twice is fine
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("haskill/steam-login.json");
        let file = CredentialFile::new(&path);
        file.save(&SteamCredentials::new("a".into(), "t".into(), None))
            .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        // A file someone loosened is tightened on load.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        file.load().unwrap().unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn debug_hides_token() {
        let c = SteamCredentials::new("alice".into(), "secret-token".into(), Some("gd".into()));
        let s = format!("{c:?}");
        assert!(s.contains("alice"));
        assert!(!s.contains("secret-token") && !s.contains("gd\""));
    }

    #[test]
    fn expiry_comes_from_jwt_exp() {
        let c = SteamCredentials::new("a".into(), jwt(r#"{"sub":"7656","exp":1700000000}"#), None);
        assert_eq!(c.expires_at(), Some(1_700_000_000));
        assert!(c.is_expired_at(1_700_000_000));
        assert!(!c.is_expired_at(1_699_999_999));
        let opaque = SteamCredentials::new("a".into(), "not-a-jwt".into(), None);
        assert_eq!(opaque.expires_at(), None);
        assert!(!opaque.is_expired_at(u64::MAX));
    }

    #[test]
    fn newer_files_with_extra_fields_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("steam-login.json");
        std::fs::write(
            &path,
            br#"{"account_name":"a","refresh_token":"t","steam_id":7656,"future":{}}"#,
        )
        .unwrap();
        let c = CredentialFile::new(&path).load().unwrap().unwrap();
        assert_eq!((c.account_name.as_str(), c.refresh_token()), ("a", "t"));
    }

    #[test]
    fn garbage_file_names_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("steam-login.json");
        std::fs::write(&path, b"not json").unwrap();
        let err = CredentialFile::new(&path).load().unwrap_err().to_string();
        assert!(err.contains("haskill login steam"), "{err}");
    }
}
