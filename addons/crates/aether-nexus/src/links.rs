//! Signed download links, and the file that keeps them from one run to the
//! next (`<data>/index/nexus-links.json`).
//!
//! A link is a secret, like the API key: whoever holds it can download the
//! file until it expires. So the file is mode 0600, [`SignedUrl`]'s `Debug`
//! shows no URL, and nothing here puts a link (or a parse error, which may
//! quote one) in a log line or an error.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use url::Url;

/// The file format this build reads and writes.
const VERSION: u32 = 1;

/// A signed URL of a repacked zip, and when the file host stops honouring
/// it (the API's `expires_at`; no lifetime is assumed).
#[derive(Clone, PartialEq, Eq)]
pub(super) struct SignedUrl {
    pub url: Url,
    pub expires_at: SystemTime,
}

impl fmt::Debug for SignedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignedUrl")
            .field("url", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl SignedUrl {
    /// Whether the link still has more than `margin` left at `now`.
    pub fn valid(&self, now: SystemTime, margin: Duration) -> bool {
        now.checked_add(margin)
            .is_some_and(|until| self.expires_at > until)
    }
}

// No `Debug` on these two: they hold links.
#[derive(Serialize, Deserialize)]
struct LinkFile {
    v: u32,
    links: Vec<SavedLink>,
}

#[derive(Serialize, Deserialize)]
struct SavedLink {
    uid: u64,
    url: String,
    /// Seconds since the Unix epoch.
    expires_at: u64,
}

/// The links in `path` that still have more than `margin` left at `now`.
/// A missing file is no links; so is one that cannot be read or is not a
/// link file (it is replaced at the next save).
pub(super) fn load(path: &Path, now: SystemTime, margin: Duration) -> HashMap<u64, SignedUrl> {
    remove_stale_partials(path);
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the saved Nexus download links; starting without them");
            return HashMap::new();
        }
    };
    // The parse error is not logged: serde quotes the value it stopped at.
    let file = match serde_json::from_slice::<LinkFile>(&bytes) {
        Ok(f) if f.v == VERSION => f,
        _ => {
            tracing::warn!("the saved Nexus download links are unreadable; starting without them");
            return HashMap::new();
        }
    };
    file.links
        .into_iter()
        .filter_map(|l| {
            let signed = SignedUrl {
                url: Url::parse(&l.url).ok()?,
                expires_at: UNIX_EPOCH.checked_add(Duration::from_secs(l.expires_at))?,
            };
            signed.valid(now, margin).then_some((l.uid, signed))
        })
        .collect()
}

/// Remove the temp files of saves that never finished (a process killed
/// between creating one and renaming it): they hold links. Only one
/// process writes `path` at a time (the store lock), and it loads before
/// it saves, so none of them is in use.
fn remove_stale_partials(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{name}.");
    for e in entries.flatten() {
        let file = e.file_name();
        let stale = file
            .to_str()
            .is_some_and(|f| f.starts_with(&prefix) && f.ends_with(".partial"));
        if stale {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Write the links of `links` that still have more than `margin` left at
/// `now` to `path`: a sibling `.partial` created with mode 0600, renamed
/// over `path`. Not synced: a link lost to a crash is signed again.
pub(super) fn save(
    path: &Path,
    links: &HashMap<u64, SignedUrl>,
    now: SystemTime,
    margin: Duration,
) -> io::Result<()> {
    let mut saved: Vec<SavedLink> = links
        .iter()
        .filter(|(_, s)| s.valid(now, margin))
        .map(|(uid, s)| SavedLink {
            uid: *uid,
            url: s.url.as_str().to_string(),
            expires_at: s
                .expires_at
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        })
        .collect();
    saved.sort_unstable_by_key(|l| l.uid);
    let bytes = serde_json::to_vec(&LinkFile {
        v: VERSION,
        links: saved,
    })
    .map_err(io::Error::other)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("the link file has no parent directory"))?;
    fs::create_dir_all(parent)?;
    // The pid and a counter keep two writers off each other's temp file.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut name = path
        .file_name()
        .ok_or_else(|| io::Error::other("the link file has no name"))?
        .to_os_string();
    name.push(format!(
        ".{}.{}.partial",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = parent.join(name);
    let result = (|| {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            // A stale .partial keeps its old mode; whatever the umask.
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(&bytes)?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(secs_left: u64) -> SignedUrl {
        SignedUrl {
            url: Url::parse("https://files.example/repacked/7?exp=1&sig=SECRET-SIGNATURE").unwrap(),
            expires_at: now() + Duration::from_secs(secs_left),
        }
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    #[test]
    fn debug_never_shows_the_link() {
        let text = format!("{:?}", link(3600));
        assert!(!text.contains("SECRET"), "{text}");
        assert!(!text.contains("files.example"), "{text}");
        assert!(text.contains("expires_at"), "{text}");
    }

    #[test]
    fn a_link_is_valid_only_with_more_than_the_margin_left() {
        let margin = Duration::from_secs(300);
        assert!(link(301).valid(now(), margin));
        assert!(!link(300).valid(now(), margin));
        assert!(!link(0).valid(now(), margin));
        let expired = SignedUrl {
            expires_at: now() - Duration::from_secs(1),
            ..link(0)
        };
        assert!(!expired.valid(now(), Duration::ZERO));
    }
}
