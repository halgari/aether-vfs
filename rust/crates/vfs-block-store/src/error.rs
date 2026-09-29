use std::io;

/// Errors returned by the block store.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("index error: {0}")]
    Index(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("file not found")]
    NotFound,
    #[error("write is not block-aligned")]
    Unaligned,
    #[error("block index out of range")]
    OutOfRange,
    #[error("file id is longer than the configured maximum")]
    FileIdTooLong,
    #[error("store is locked by another process")]
    Locked,
    #[error("corrupt store: {0}")]
    Corrupt(String),
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("write failed after {blocks_written} blocks: {source}")]
    PartialWrite {
        blocks_written: u64,
        #[source]
        source: Box<Error>,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

macro_rules! from_redb {
    ($($t:ty),* $(,)?) => {
        $(impl From<$t> for Error {
            fn from(e: $t) -> Self {
                Error::Index(Box::new(e))
            }
        })*
    };
}

from_redb!(
    redb::Error,
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::SetDurabilityError,
);
