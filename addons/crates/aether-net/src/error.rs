use std::time::Duration;

use aether_archive::{FormatError, Xxh64};

/// Every fallible function in this crate returns this error. Messages never
/// contain the Nexus API key or a signed URL's query string.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error(
        "Nexus Mods rejected the API key (HTTP 401). Log in to Nexus Mods again with a valid personal API key."
    )]
    NexusUnauthorized,
    #[error(
        "Nexus Mods refused the download (HTTP 403: {detail}). Downloading through the API needs a Nexus Mods Premium membership."
    )]
    NexusForbidden { detail: String },
    #[error("not found (HTTP 404): {what}")]
    NotFound { what: String },
    #[error("rate limited by {host} (HTTP 429){}", retry_after.map(|d| format!(", retry after {}s", d.as_secs())).unwrap_or_default())]
    RateLimited {
        host: String,
        retry_after: Option<Duration>,
    },
    #[error("HTTP {status} from {url}: {body}")]
    Status {
        url: String,
        status: u16,
        body: String,
    },
    #[error("network error talking to {url}: {source}")]
    Network {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("unexpected response from {url}: {msg}")]
    Protocol { url: String, msg: String },
    /// The file host now reports a different size than the archive was
    /// opened with: a re-upload replaced the repack while it was open.
    /// Callers that cache an index (or an archive handle) built from the
    /// old size must drop it and reopen.
    #[error("archive changed on the server: now {now} bytes, was {was} ({url})")]
    ArchiveChanged { url: String, now: u64, was: u64 },
    /// One download unit (a CDN part) arrived damaged; fetching it again may help.
    #[error("{what} is corrupt: {msg}")]
    CorruptPart { what: String, msg: String },
    #[error("{what}: expected xxHash64 {expected}, got {actual}")]
    HashMismatch {
        what: String,
        expected: Xxh64,
        actual: Xxh64,
    },
    /// No Steam app ticket could be minted (the Steam error, which names
    /// its fix).
    #[error("{0}")]
    Ticket(String),
    /// A service (`service`, e.g. "Bethesda.net") refused a request (never
    /// retried). `msg` is written for the user and carries nothing secret.
    #[error("{msg}")]
    Refused {
        service: &'static str,
        status: u16,
        code: Option<i64>,
        msg: String,
    },
    #[error("entries not found in archive: {0:?}")]
    MissingEntries(Vec<String>),
    #[error("{format}: {msg}")]
    Archive { format: &'static str, msg: String },
    #[error("unsupported archive: {0}")]
    Unsupported(String),
    /// An archive format error. [`FormatError::MissingEntries`] converts
    /// to [`SourceError::MissingEntries`] instead.
    #[error(transparent)]
    Format(FormatError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SourceError>;

impl From<FormatError> for SourceError {
    fn from(e: FormatError) -> SourceError {
        match e {
            FormatError::MissingEntries(missing) => SourceError::MissingEntries(missing),
            e => SourceError::Format(e),
        }
    }
}

impl SourceError {
    /// Whether trying the same request again may succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            SourceError::Network { .. }
            | SourceError::RateLimited { .. }
            | SourceError::CorruptPart { .. }
            | SourceError::Format(FormatError::Checksum { .. }) => true,
            // Nexus's file host answers a range past EOF with 500 "error code:
            // 1101": that is our bug, not a transient failure.
            SourceError::Status { status, body, .. } => {
                *status >= 500 && !body.contains("error code: 1101")
            }
            _ => false,
        }
    }

    /// How long the server asked us to wait, if it said.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            SourceError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    #[doc(hidden)]
    pub fn network(url: &url::Url, e: reqwest::Error) -> SourceError {
        SourceError::Network {
            url: redact(url),
            source: e.without_url(),
        }
    }

    #[doc(hidden)]
    pub fn protocol(url: &url::Url, msg: impl Into<String>) -> SourceError {
        SourceError::Protocol {
            url: redact(url),
            msg: msg.into(),
        }
    }

    #[doc(hidden)]
    pub fn archive_changed(url: &url::Url, now: u64, was: u64) -> SourceError {
        SourceError::ArchiveChanged {
            url: redact(url),
            now,
            was,
        }
    }
}

/// `scheme://host/path` without the query string: signed download URLs
/// carry their signature in the query, and it must not reach logs.
#[doc(hidden)]
pub fn redact(url: &url::Url) -> String {
    format!(
        "{}://{}{}",
        url.scheme(),
        url.host_str().unwrap_or(""),
        url.path()
    )
}

/// Like [`redact`], but for a raw string that failed to parse as a URL (so
/// there is no `Url` to ask): drops everything from the first `?` or `#`
/// on, since a malformed presigned URL may still carry its signature there.
#[doc(hidden)]
pub fn redact_raw(s: &str) -> String {
    let end = s.find(['?', '#']).unwrap_or(s.len());
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_drops_query() {
        let u = url::Url::parse("https://repacked-files.nexusmods.com/2b/68/x?exp=1&sig=SECRET")
            .unwrap();
        assert_eq!(redact(&u), "https://repacked-files.nexusmods.com/2b/68/x");
    }

    #[test]
    fn redact_raw_drops_query_and_fragment() {
        assert_eq!(redact_raw("ht!tp://host/p?sig=SECRET"), "ht!tp://host/p");
        assert_eq!(redact_raw("ht!tp://host/p#SECRET"), "ht!tp://host/p");
        assert_eq!(redact_raw("ht!tp://host/p"), "ht!tp://host/p");
    }

    #[test]
    fn missing_entries_keep_their_variant() {
        let e: SourceError = FormatError::MissingEntries(vec!["a.esp".into()]).into();
        assert!(matches!(e, SourceError::MissingEntries(m) if m == ["a.esp"]));
        let e: SourceError = aether_archive::invalid("zip", "bad").into();
        assert!(matches!(
            e,
            SourceError::Format(FormatError::Invalid { .. })
        ));
    }

    #[test]
    fn retryable_classes() {
        let st = |status: u16, body: &str| SourceError::Status {
            url: "u".into(),
            status,
            body: body.into(),
        };
        assert!(st(503, "busy").is_retryable());
        assert!(!st(500, "error code: 1101").is_retryable());
        assert!(!st(404, "").is_retryable());
        assert!(!SourceError::NexusUnauthorized.is_retryable());
        let rl = SourceError::RateLimited {
            host: "h".into(),
            retry_after: Some(Duration::from_secs(3)),
        };
        assert!(rl.is_retryable());
        assert_eq!(rl.retry_after(), Some(Duration::from_secs(3)));
        assert!(rl.to_string().contains("retry after 3s"));
    }
}
