#![forbid(unsafe_code)]
//! `vfs-core`: pure, OS-independent read-only resolver for a merged/overlaid
//! virtual filesystem. Fed enumerated layers (data-in); does no I/O.

mod casefold;
mod cachekey;
mod model;
mod path;
mod tree;
mod wildcard;

// pub use cachekey::compute_cache_key;
// pub use model::{
//     BuildError, CacheKey, DirEntry, EntryKind, InputEntry, Layer, LayerId, NodeKind, Resolution,
//     SourceId, Stat, VfsError,
// };
pub use path::{normalize_vpath, PathError};
// pub use tree::VfsTree;
// pub use tree::build;
pub use wildcard::wildcard_match;
