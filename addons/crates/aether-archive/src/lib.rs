//! Random-access archive formats, with no network dependencies: seekable
//! zstd, zip central directories (ZIP64, Nexus seek tables), in-process
//! 7z / zip extraction, the [`RangeRead`] byte-source trait they all read
//! through, and [`Xxh64`], the xxHash64 file hash Wabbajack and the
//! downloaders verify content with.

pub mod error;
pub mod extract;
mod hash;
pub mod path;
pub mod range;
pub mod seekable;
pub mod zip;

pub use error::{FormatError, Result};
#[doc(hidden)]
pub use error::{checksum, invalid, unsupported};
pub use hash::{HashParseError, Xxh64};
pub use range::RangeRead;
