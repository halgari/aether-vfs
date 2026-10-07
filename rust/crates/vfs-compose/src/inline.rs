//! Read-only, immutable in-memory file tree (Clojure `inline-provider`).
//!
//! A [`MemoryProvider`] with the write half of the contract taken away: the
//! tree is the one `MemoryProvider` serves (fold-equal lookup, synthesized
//! parent directories), but the capabilities declare `Access::Read` and
//! `immutable`, and every mutating call is refused. Many tests across the
//! workspace key off exactly those capabilities, which is why this stays a
//! type of its own rather than a mode of `MemoryProvider`.

use vfs_provider::{
    bad_request, Capabilities, DirEntry, Handle, Provider, Stat, VPath, OPEN_READ, OPEN_WRITE,
};

use crate::MemoryProvider;

/// Flat map of virtual paths → file bytes. Parent dirs are synthesized.
pub struct InlineProvider {
    tree: MemoryProvider,
}

impl InlineProvider {
    pub fn from_files<I, P, B>(entries: I) -> Self
    where
        I: IntoIterator<Item = (P, B)>,
        P: AsRef<str>,
        B: AsRef<[u8]>,
    {
        Self {
            tree: MemoryProvider::from_files(entries),
        }
    }
}

impl Provider for InlineProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            immutable: true,
            ..Capabilities::read_only()
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.tree.getattr(p)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.tree.readdir(p)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        if flags & OPEN_WRITE != 0 {
            return Err(bad_request());
        }
        // Read-only whatever else the caller asked for: create, truncate and
        // exclusive are meaningless here and must not reach the tree.
        self.tree.open(p, OPEN_READ)
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.tree.read_at(h, offset, buf)
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        self.tree.close(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fold-equal spellings name the same entry. `InlineProvider` is the leaf
    /// under most composed test stacks, so a byte-exact match here makes every
    /// stack above it byte-exact too.
    #[test]
    fn fold_equal_spellings_resolve_to_one_entry() {
        let p = InlineProvider::from_files([("Data/A.esp", &b"body"[..])]);

        for spelling in ["Data/A.esp", "data/a.esp", "DATA/A.ESP", "dAtA/a.EsP"] {
            let st = p
                .getattr(VPath::at_default(spelling))
                .unwrap()
                .unwrap_or_else(|| panic!("{spelling} did not resolve"));
            assert_eq!(st.size, 4, "{spelling} resolved to the wrong entry");
        }
    }

    /// Non-ASCII, because `to_ascii_lowercase` would pass every case above.
    #[test]
    fn folding_is_unicode_not_ascii() {
        let p = InlineProvider::from_files([("Über/A.esp", &b"x"[..])]);
        assert!(
            p.getattr(VPath::at_default("über/a.esp")).unwrap().is_some(),
            "Unicode fold-equal spelling did not resolve"
        );
    }
}
