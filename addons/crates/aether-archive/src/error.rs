use std::io;

/// Every fallible function in this crate returns this error.
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("{format}: invalid data: {msg}")]
    Invalid { format: &'static str, msg: String },
    #[error("{format}: unsupported: {msg}")]
    Unsupported { format: &'static str, msg: String },
    #[error("{format}: checksum mismatch: {msg}")]
    Checksum { format: &'static str, msg: String },
    /// [`extract`](crate::extract::extract) did not find these wanted
    /// paths (spelled as asked for) in the archive.
    #[error("entries not found in archive: {0:?}")]
    MissingEntries(Vec<String>),
}

pub type Result<T> = std::result::Result<T, FormatError>;

#[doc(hidden)]
pub fn invalid(format: &'static str, msg: impl Into<String>) -> FormatError {
    FormatError::Invalid {
        format,
        msg: msg.into(),
    }
}

#[doc(hidden)]
pub fn unsupported(format: &'static str, msg: impl Into<String>) -> FormatError {
    FormatError::Unsupported {
        format,
        msg: msg.into(),
    }
}

#[doc(hidden)]
pub fn checksum(format: &'static str, msg: impl Into<String>) -> FormatError {
    FormatError::Checksum {
        format,
        msg: msg.into(),
    }
}
