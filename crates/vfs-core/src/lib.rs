#![forbid(unsafe_code)]
//! `vfs-core`: pure, OS-independent read-only resolver for a merged/overlaid
//! virtual filesystem. Fed enumerated layers (data-in); does no I/O.
//!
//! ```
//! use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId, Resolution};
//!
//! let tree = build(vec![Layer {
//!     id: LayerId(0),
//!     entries: vec![InputEntry {
//!         vpath: "data/a.esp".into(),
//!         kind: EntryKind::File,
//!         source: "root/data/a.esp".into(),
//!         size: 10,
//!         mtime: 42,
//!     }],
//! }])
//! .unwrap();
//! assert!(matches!(tree.resolve("data/a.esp"), Resolution::File { .. }));
//! ```

mod casefold;
mod cachekey;
mod model;
mod path;
mod tree;
mod wildcard;

pub use cachekey::compute_cache_key;
pub use model::{
    BuildError, CacheKey, DirEntry, EntryKind, InputEntry, Layer, LayerId, NodeKind, Resolution,
    SourceId, Stat, VfsError,
};
pub use path::{normalize_vpath, PathError};
pub use tree::VfsTree;
pub use tree::build;
pub use wildcard::wildcard_match;
