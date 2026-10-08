//! The saved GOG login: a JSON file, mode 0600 in a 0700 directory, whose
//! tokens are wiped from memory when dropped.
use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::error::GogError;
use crate::fsutil::{read_optional, write_atomic};

/// Refresh an access token this long before GOG says it expires.
pub(crate) const EXPIRY_MARGIN_SECS: u64 = 60;

/// A GOG OAuth login: the access token, the refresh token that renews it,
/// and when the access token expires. `Debug` never prints a token; both
/// are zeroized on drop.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GogCredentials {
    pub user_id: String,
    access_token: String,
    refresh_token: String,
    /// Unix seconds.
    pub expires_at: u64,
}

impl GogCredentials {
    pub fn new(
        user_id: String,
        access_token: String,
        refresh_token: String,
        expires_at: u64,
    ) -> Self {
        GogCredentials {
            user_id,
            access_token,
            refresh_token,
            expires_at,
        }
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    /// True when the access token expires within a minute of `now_unix`.
    pub fn needs_refresh_at(&self, now_unix: u64) -> bool {
        self.expires_at <= now_unix.saturating_add(EXPIRY_MARGIN_SECS)
    }

    /// The saved login at `path`, or `None` when there is none. A file
    /// readable by group or others is tightened to 0600 first.
    pub fn load(path: &Path) -> Result<Option<GogCredentials>, GogError> {
        let Some(bytes) = read_optional(path)? else {
            return Ok(None);
        };
        let bytes = Zeroizing::new(bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)?.permissions().mode();
            if mode & 0o077 != 0 {
                tracing::warn!(path = %path.display(), "tightening GOG login file to 0600");
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            GogError::json(
                path.display().to_string(),
                format!("not a GOG login file ({e}); log in to GOG again"),
            )
        })
    }

    /// Write this login to `path` (atomically, owner-only).
    pub fn save(&self, path: &Path) -> Result<(), GogError> {
        let json = Zeroizing::new(
            serde_json::to_vec_pretty(self).map_err(|e| GogError::json("GOG login", e))?,
        );
        write_atomic(path, &json, true)?;
        Ok(())
    }
}

impl Drop for GogCredentials {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

impl fmt::Debug for GogCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GogCredentials")
            .field("user_id", &self.user_id)
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> GogCredentials {
        GogCredentials::new("7".into(), "acc-secret".into(), "ref-secret".into(), 1000)
    }

    #[test]
    fn save_then_load_round_trips_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("login/gog.json");
        assert!(GogCredentials::load(&path).unwrap().is_none());
        creds().save(&path).unwrap();
        assert_eq!(GogCredentials::load(&path).unwrap(), Some(creds()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            GogCredentials::load(&path).unwrap().unwrap();
            assert_eq!(mode(&path), 0o600);
        }
    }

    #[test]
    fn debug_hides_tokens() {
        let s = format!("{:?}", creds());
        assert!(!s.contains("secret"), "{s}");
    }

    #[test]
    fn refresh_a_minute_early() {
        let c = creds();
        assert!(!c.needs_refresh_at(939));
        assert!(c.needs_refresh_at(940));
    }

    #[test]
    fn garbage_file_names_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gog.json");
        std::fs::write(&path, b"nope").unwrap();
        let e = GogCredentials::load(&path).unwrap_err().to_string();
        assert!(e.contains("log in to GOG again"), "{e}");
    }
}
