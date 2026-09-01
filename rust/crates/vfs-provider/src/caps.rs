//! What a provider can do. Declared, not probed: the composition layer reads
//! these at construction time to validate a stack and to warn about one that
//! will perform badly.

/// Access level. `ReadWrite` implies positional read; there is no
/// write-without-seek tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    /// Forward-only reads via `read_next`. Must be wrapped in `seekable`.
    SeqRead,
    /// Positional reads via `read_at`.
    Read,
    /// Positional reads and writes.
    ReadWrite,
}

/// How this provider matches a name it is given.
///
/// Declared, not probed — like every other capability here. The composition
/// layer reads it to select conformance cases, and a future FUSE mount will
/// read it to refuse a `Sensitive` provider outright, since a Windows program
/// over one is broken by construction.
///
/// This exists because two delivery paths disagreed about the spelling a
/// provider receives: the shim folds a vpath before sending it
/// (`vfs-redirect`'s `match_canonical`), while a host-side caller
/// (`vfs-embed`, `vfs-node`, this crate's conformance suite) sends the
/// original case. A provider that resolves fold-equal names identically is
/// correct under both, which is why the guarantee lives here rather than at
/// either boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseMatch {
    /// Fold-equal names resolve to the same entry, where fold-equal means
    /// `vfs_core::fold` — not `to_ascii_lowercase`, and not "the OS will
    /// sort it out".
    Insensitive,
    /// Byte-exact names only. Correct for a provider over a case-sensitive
    /// store that has not indexed for folding; **not** safe under a FUSE mount
    /// serving a Windows program.
    Sensitive,
}

/// A provider's declared capabilities.
///
/// `immutable` and `slow` are orthogonal and the pair is what carries
/// information: `immutable` says caching is *safe*, `slow` says it is
/// *warranted*. Only both together justify persisting blocks to disk across
/// sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub access: Access,
    /// Content never changes for the provider's lifetime.
    pub immutable: bool,
    /// Reads are expensive; this provider should sit behind a cache.
    pub slow: bool,
    /// Block-size hint for `cached`. `None` means "caller decides".
    pub preferred_block: Option<u32>,
    /// How names are matched. See [`CaseMatch`]; `Insensitive` is what a
    /// Windows-facing VFS must provide.
    pub case: CaseMatch,
}

impl Capabilities {
    /// A fast, mutable, positional read-only provider — the common default.
    pub fn read_only() -> Self {
        Capabilities {
            access: Access::Read,
            immutable: false,
            slow: false,
            preferred_block: None,
            case: CaseMatch::Insensitive,
        }
    }

    /// Reject self-contradictory declarations. Called at construction.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.access == Access::ReadWrite && self.immutable {
            return Err("a ReadWrite provider cannot be immutable");
        }
        Ok(())
    }

    /// Capabilities of `seekable(self)`: sequential becomes positional.
    pub fn seekable(self) -> Self {
        let access = if self.access == Access::SeqRead { Access::Read } else { self.access };
        Capabilities { access, ..self }
    }

    /// Capabilities of `cached(self)`: access passes through, slow is answered.
    pub fn cached(self) -> Self {
        Capabilities { slow: false, ..self }
    }

    /// Capabilities of `readonly(self)`: write access is demoted.
    pub fn read_only_clamp(self) -> Self {
        let access = if self.access == Access::ReadWrite { Access::Read } else { self.access };
        Capabilities { access, ..self }
    }

    /// Capabilities of a combinator over several children: the weakest access,
    /// immutable only if all are, slow if any is, smallest block hint present.
    pub fn weakest(children: impl IntoIterator<Item = Capabilities>) -> Self {
        let mut out: Option<Capabilities> = None;
        for c in children {
            out = Some(match out {
                None => c,
                Some(acc) => Capabilities {
                    access: acc.access.min(c.access),
                    immutable: acc.immutable && c.immutable,
                    slow: acc.slow || c.slow,
                    preferred_block: match (acc.preferred_block, c.preferred_block) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    },
                    case: match (acc.case, c.case) {
                        (CaseMatch::Insensitive, CaseMatch::Insensitive) => CaseMatch::Insensitive,
                        _ => CaseMatch::Sensitive,
                    },
                },
            });
        }
        out.unwrap_or_else(Capabilities::read_only)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_plus_immutable_is_rejected() {
        let c = Capabilities { access: Access::ReadWrite, immutable: true, ..Capabilities::read_only() };
        assert!(c.validate().is_err());
    }

    #[test]
    fn a_plain_read_only_provider_validates() {
        assert!(Capabilities::read_only().validate().is_ok());
    }

    #[test]
    fn seekable_promotes_sequential_to_positional() {
        let seq = Capabilities { access: Access::SeqRead, ..Capabilities::read_only() };
        assert_eq!(seq.seekable().access, Access::Read);
    }

    #[test]
    fn seekable_leaves_an_already_positional_provider_alone() {
        let rw = Capabilities { access: Access::ReadWrite, ..Capabilities::read_only() };
        assert_eq!(rw.seekable().access, Access::ReadWrite);
    }

    #[test]
    fn caching_clears_the_slow_marker() {
        let slow = Capabilities { slow: true, ..Capabilities::read_only() };
        assert!(!slow.cached().slow);
    }

    #[test]
    fn read_only_clamp_demotes_write_access() {
        let rw = Capabilities { access: Access::ReadWrite, ..Capabilities::read_only() };
        assert_eq!(rw.read_only_clamp().access, Access::Read);
    }

    #[test]
    fn weakest_takes_the_lowest_access_and_ands_immutability() {
        let rw = Capabilities { access: Access::ReadWrite, immutable: false, ..Capabilities::read_only() };
        let ro = Capabilities { access: Access::Read, immutable: true, ..Capabilities::read_only() };
        let w = Capabilities::weakest([rw, ro]);
        assert_eq!(w.access, Access::Read);
        assert!(!w.immutable);
    }

    #[test]
    fn weakest_of_nothing_is_read_only() {
        assert_eq!(Capabilities::weakest([]).access, Access::Read);
    }

    #[test]
    fn weakest_marks_slow_if_any_child_is_slow() {
        let fast = Capabilities::read_only();
        let slow = Capabilities { slow: true, ..Capabilities::read_only() };
        assert!(Capabilities::weakest([fast, slow]).slow);
    }

    #[test]
    fn read_only_declares_case_insensitive_because_that_is_what_windows_needs() {
        assert_eq!(Capabilities::read_only().case, CaseMatch::Insensitive);
    }

    /// A graph is only as case-insensitive as its least-insensitive leaf. One
    /// `Sensitive` child makes the whole composition `Sensitive`, the same way
    /// one non-immutable child makes it mutable.
    #[test]
    fn weakest_is_case_sensitive_if_any_child_is() {
        let ins = Capabilities::read_only();
        let sen = Capabilities { case: CaseMatch::Sensitive, ..Capabilities::read_only() };
        assert_eq!(Capabilities::weakest([ins, sen]).case, CaseMatch::Sensitive);
        assert_eq!(Capabilities::weakest([sen, ins]).case, CaseMatch::Sensitive);
    }

    #[test]
    fn weakest_stays_insensitive_when_all_children_are() {
        let ins = Capabilities::read_only();
        assert_eq!(Capabilities::weakest([ins, ins]).case, CaseMatch::Insensitive);
    }

    /// The combinators that pass access through must not silently reset case.
    #[test]
    fn the_passthrough_combinators_preserve_the_case_declaration() {
        let sen = Capabilities { case: CaseMatch::Sensitive, ..Capabilities::read_only() };
        assert_eq!(sen.seekable().case, CaseMatch::Sensitive);
        assert_eq!(sen.cached().case, CaseMatch::Sensitive);
        assert_eq!(sen.read_only_clamp().case, CaseMatch::Sensitive);
    }
}
