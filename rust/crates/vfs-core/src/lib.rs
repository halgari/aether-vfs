#![forbid(unsafe_code)]
//! `vfs-core`: pure, OS-independent resolver for a merged/overlaid
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
pub mod finalname;
mod model;
mod path;
mod source;
mod tree;
mod wildcard;

pub use cachekey::compute_cache_key;
pub use model::{
    BuildError, CacheKey, EntryKind, InputEntry, Layer, LayerId, Resolution, SourceId, TreeEntry,
    TreeStat, VfsError,
};
// compat: removed by cleanup stream I
#[doc(hidden)]
pub use model::NodeKind;
pub use casefold::fold;
pub use path::{
    normalize_rel, normalize_vpath, rel_components, split_parent, trim_rel, BadComponent, PathError,
};
pub use source::{decode, encode_zip_window, Source};
pub use tree::VfsTree;
pub use tree::build;
pub use tree::{WalkNode, WalkNodeKind};
pub use wildcard::wildcard_match;
