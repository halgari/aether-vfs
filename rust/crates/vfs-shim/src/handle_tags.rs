//! The tag bits that mark the shim's synthetic handles, in one place.
//!
//! Three families of handle are handed to the game that no kernel object stands behind. Each is a
//! slot number OR-ed with a tag, and the tags must never overlap, or a handle of one family reads
//! as another's:
//!
//! | family | tag | module |
//! |---|---|---|
//! | synthetic *section* (an `NtCreateSection` result) | [`SYNTH_SECTION_TAG`], 2^45 | `synth_section` |
//! | synthetic *file* (a director file handle) | [`SYNTH_FILE_TAG`], 2^47 | `synth_file` |
//! | synthetic registry *key* | [`REG_TAG`], `0x6000_0000` | `regkeys` |
//!
//! 2^46 belonged to the zip-window file handles that gate 4 task 7 removed, and is unassigned.

/// Tag bit (2^45) marking a synthetic *section* handle (an `NtCreateSection` result); real kernel
/// handles never reach this magnitude. The sign bit (2^63) stays clear so the value is a positive
/// handle, never confused with pseudo-handles (-1..-6) or `INVALID_HANDLE_VALUE`.
pub(crate) const SYNTH_SECTION_TAG: usize = 0x0000_2000_0000_0000;

/// Tag bit (2^47) marking a synthetic *file* handle: a director FUSE file handle.
pub(crate) const SYNTH_FILE_TAG: usize = 0x0000_8000_0000_0000;

/// Tag bits of a synthetic key handle: bits 29 and 30, nothing above them.
///
/// **Below `0x80000000`, on purpose.** Wine's `RegCloseKey` (kernelbase) returns
/// `ERROR_SUCCESS` without calling `NtClose` for any `hkey >= (HKEY)0x80000000`, taking it for a
/// predefined key. A synthetic handle up there (they were once tagged 2^46) never reached the
/// close hook through `RegCloseKey`, so every key a program opened and closed through advapi32
/// leaked its record and its private real handle: 180k of them in ten minutes of a game that
/// writes a key every frame. Kernelbase's other predefined-key tests take the low 32 bits
/// (`HandleToUlong`), which here never fall in `0x80000000..=0x80000006` either, and a handle
/// truncated to 32 bits stays itself.
///
/// **Clear of real handles.** Wine's process-local handles are `(index + 1) << 2` with fewer
/// than 2^24 entries, so below `0x0400_0000`; its global handles are a local one XOR
/// `0x544a4def`, whose bit 29 is clear. The sign bit is clear, so the value is never a
/// pseudo-handle, and none of [`SYNTH_SECTION_TAG`]'s (2^45) or [`SYNTH_FILE_TAG`]'s (2^47) bits is set.
pub(crate) const REG_TAG: usize = 0x6000_0000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tags_do_not_overlap() {
        let tags = [SYNTH_SECTION_TAG, SYNTH_FILE_TAG, REG_TAG];
        for (i, a) in tags.iter().enumerate() {
            for b in &tags[i + 1..] {
                assert_eq!(a & b, 0, "tags {a:#x} and {b:#x} share a bit");
            }
        }
    }

    #[test]
    fn every_tag_leaves_the_sign_bit_clear() {
        for t in [SYNTH_SECTION_TAG, SYNTH_FILE_TAG, REG_TAG] {
            assert_eq!(t >> 63, 0, "tag {t:#x} would read as a pseudo-handle");
        }
    }
}
