use crate::ids::{DepotId, ManifestId};
use std::io;
use std::time::Duration;

/// Format `d` for a person: milliseconds below one second (so a sub-second
/// timeout never silently rounds down to a meaningless "0s"), otherwise
/// whole or one-decimal seconds.
fn human_duration(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{}ms", d.as_millis())
    } else {
        let secs = d.as_secs_f64();
        if (secs - secs.round()).abs() < 0.05 {
            format!("{}s", d.as_secs())
        } else {
            format!("{secs:.1}s")
        }
    }
}

/// A Steam app as a person knows it.
fn app_name(app: u32) -> String {
    match app {
        489830 => "Skyrim Special Edition (app 489830)".into(),
        1746860 => "the Skyrim Anniversary Upgrade (app 1746860)".into(),
        a => format!("app {a}"),
    }
}

/// Every fallible public function in this crate returns this error. Messages
/// are written for the person running the host application and name the fix
/// where there is one; how to log in is the host's to add (match
/// [`NotLoggedIn`](Self::NotLoggedIn) and [`LoginExpired`](Self::LoginExpired)).
#[derive(Debug, thiserror::Error)]
pub enum SteamError {
    #[error("not logged in to Steam; log in to Steam first")]
    NotLoggedIn,
    #[error(
        "the saved Steam login for {account} has expired or was revoked; log in to Steam again"
    )]
    LoginExpired { account: String },
    #[error("Steam rejected the account name or password")]
    InvalidPassword,
    #[error("Steam rejected the Steam Guard code; try again")]
    InvalidGuardCode,
    #[error("the Steam login request expired before it was approved; start the login again")]
    AuthSessionExpired,
    #[error("Steam offered no sign-in confirmation this login supports (code or mobile approval)")]
    NoSupportedGuard,
    #[error("Steam denied access to depot {0}; is the game owned by the logged-in account?")]
    AccessDenied(DepotId),
    #[error("Steam says this account does not own {}", app_name(*app))]
    NotOwned { app: u32 },
    #[error("Steam is rate limiting ticket requests; wait a minute and try again")]
    RateLimited,
    #[error("Steam would not grant manifest {manifest} of depot {depot}")]
    ManifestUnavailable {
        depot: DepotId,
        manifest: ManifestId,
    },
    #[error("timed out after {shown} waiting for {what}", shown = human_duration(*after))]
    Timeout { what: &'static str, after: Duration },
    #[error("Steam CDN: {0}")]
    Cdn(String),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("{0:?} is not in the game's Steam depots")]
    FileNotFound(String),
    #[error("no Steam depots are known for {game} {version}")]
    UnknownGame { game: String, version: String },
    #[error("read of {len} bytes at {off} is outside the {size}-byte file")]
    OutOfRange { off: u64, len: u64, size: u64 },
    #[error("Steam protocol: {0}")]
    Protocol(String),
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
}

impl From<SteamError> for io::Error {
    fn from(e: SteamError) -> io::Error {
        match e {
            SteamError::Io(e) => e,
            SteamError::OutOfRange { .. } => io::Error::new(io::ErrorKind::UnexpectedEof, e),
            SteamError::Timeout { .. } => io::Error::new(io::ErrorKind::TimedOut, e),
            SteamError::Integrity(_) => io::Error::new(io::ErrorKind::InvalidData, e),
            SteamError::FileNotFound(_) => io::Error::new(io::ErrorKind::NotFound, e),
            other => io::Error::other(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_owned_names_the_game() {
        let e = SteamError::NotOwned { app: 489830 }.to_string();
        assert!(e.contains("Skyrim Special Edition"), "{e}");
        assert!(
            SteamError::NotOwned { app: 7 }
                .to_string()
                .contains("app 7")
        );
    }

    #[test]
    fn sub_second_timeouts_display_as_milliseconds_not_0s() {
        let err = SteamError::Timeout {
            what: "a thing",
            after: Duration::from_millis(250),
        };
        let msg = err.to_string();
        assert!(msg.contains("250ms"), "{msg}");
        assert!(!msg.contains("0s"), "{msg}");
    }

    #[test]
    fn whole_second_timeouts_display_without_a_decimal() {
        let err = SteamError::Timeout {
            what: "a thing",
            after: Duration::from_secs(30),
        };
        assert!(err.to_string().contains("30s"), "{err}");
    }

    #[test]
    fn fractional_second_timeouts_display_with_one_decimal() {
        let err = SteamError::Timeout {
            what: "a thing",
            after: Duration::from_millis(1500),
        };
        assert!(err.to_string().contains("1.5s"), "{err}");
    }
}
