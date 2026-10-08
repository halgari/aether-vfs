use std::io;

use aether_net::SourceError;

use crate::ids::ProductId;

/// Every fallible public function in this crate returns this error.
/// Messages never contain a token or a signed CDN URL's secret.
#[derive(Debug, thiserror::Error)]
pub enum GogError {
    #[error("not logged in to GOG; log in to GOG first")]
    NotLoggedIn,
    /// The saved login no longer works (GOG refused its refresh token).
    /// The message says to log in to GOG again.
    #[error("{0}")]
    LoginExpired(String),
    /// A login could not be completed: the pasted text held no code, or
    /// GOG refused the code.
    #[error("GOG login failed: {0}")]
    Login(String),
    /// GOG refused a secure link: the account does not own the product.
    #[error("GOG refused access to product {0}; is it owned by the logged-in account?")]
    NotOwned(ProductId),
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error("cannot read {what}: {msg}")]
    Json { what: String, msg: String },
    #[error("{0:?} is not in the GOG depot")]
    NotInDepot(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

impl GogError {
    pub(crate) fn json(what: impl Into<String>, e: impl std::fmt::Display) -> GogError {
        GogError::Json {
            what: what.into(),
            msg: e.to_string(),
        }
    }
}

impl GogError {
    /// A copy for a second caller of a shared chunk download: the same
    /// variant where it holds plain data, else the message as an I/O error.
    pub(crate) fn duplicate(&self) -> GogError {
        use SourceError as S;
        let source = |e: S| GogError::Source(e);
        match self {
            GogError::NotLoggedIn => GogError::NotLoggedIn,
            GogError::LoginExpired(m) => GogError::LoginExpired(m.clone()),
            GogError::Login(m) => GogError::Login(m.clone()),
            GogError::NotOwned(p) => GogError::NotOwned(*p),
            GogError::Json { what, msg } => GogError::Json {
                what: what.clone(),
                msg: msg.clone(),
            },
            GogError::NotInDepot(p) => GogError::NotInDepot(p.clone()),
            GogError::Source(S::CorruptPart { what, msg }) => source(S::CorruptPart {
                what: what.clone(),
                msg: msg.clone(),
            }),
            GogError::Source(S::NotFound { what }) => source(S::NotFound { what: what.clone() }),
            GogError::Source(S::Status { url, status, body }) => source(S::Status {
                url: url.clone(),
                status: *status,
                body: body.clone(),
            }),
            GogError::Source(S::Protocol { url, msg }) => source(S::Protocol {
                url: url.clone(),
                msg: msg.clone(),
            }),
            GogError::Io(e) => GogError::Io(io::Error::new(e.kind(), e.to_string())),
            other => GogError::Io(io::Error::other(other.to_string())),
        }
    }
}

impl From<GogError> for io::Error {
    fn from(e: GogError) -> io::Error {
        match e {
            GogError::Io(e) => e,
            GogError::NotInDepot(_) => io::Error::new(io::ErrorKind::NotFound, e),
            GogError::Source(SourceError::CorruptPart { .. }) => {
                io::Error::new(io::ErrorKind::InvalidData, e)
            }
            GogError::NotLoggedIn
            | GogError::LoginExpired(_)
            | GogError::Login(_)
            | GogError::NotOwned(_) => io::Error::new(io::ErrorKind::PermissionDenied, e),
            other => io::Error::other(other),
        }
    }
}
