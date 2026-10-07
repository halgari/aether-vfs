//! The name `NtQueryObject` reports for a redirected handle.

/// The `ObjectNameInformation` name to emit for a redirected handle, given the
/// host's own answer for the same handle (`real`) and the virtual NT path the
/// caller opened (`vpath`).
///
/// The host's answer is consulted only for its **prefix convention** — never
/// for its content, which is exactly the backing path being hidden. `device_of`
/// is consulted only on the `\Device\` branch, so a host that uses `\??\` pays
/// no `QueryDosDeviceW` call.
///
/// `None` means "emit nothing, pass the host's answer through": a convention
/// this function does not recognise, or a virtual path that is not a
/// drive-letter path, is a case where a made-up name would be worse than the
/// real one.
pub fn spoofed_object_name(
    real: &str,
    vpath: &str,
    device_of: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    // DOS portion of the virtual path: `C:\dir\file`, no NT prefix.
    let dos = vpath
        .strip_prefix(r"\??\")
        .or_else(|| vpath.strip_prefix(r"\\?\"))
        .unwrap_or(vpath);
    let b = dos.as_bytes();
    // Must be `X:` optionally followed by a rooted remainder. Anything else
    // (a UNC name, a volume GUID, a relative leftover) has no drive letter to
    // resolve and no safe device form to build.
    if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        return None;
    }
    if b.len() > 2 && b[2] != b'\\' {
        return None;
    }
    if real.starts_with(r"\??\") {
        Some(format!(r"\??\{dos}"))
    } else if real.starts_with(r"\Device\") {
        let dev = device_of(&dos[..2])?;
        Some(format!("{dev}{}", &dos[2..]))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `spoofed_object_name` adopts the host's prefix rather than assuming one.
    /// Both forms are measured facts (2026-09-01): Windows answers
    /// `\Device\HarddiskVolumeN\...`, Wine answers `\??\C:\...`. A hook that
    /// emitted one fixed form would be wrong on one of the two hosts.
    #[test]
    fn spoofed_object_name_adopts_the_hosts_prefix() {
        // Wine's form: the DOS portion of the virtual path, re-prefixed. The
        // device lookup must not even be consulted here -- it disagrees with
        // Wine's own answer, so consulting it would be actively misleading.
        assert_eq!(
            spoofed_object_name(
                r"\??\C:\backing\blob.dat",
                r"\??\C:\root\mod.esp",
                |_| panic!("QueryDosDeviceW must not be consulted on the \\??\\ branch"),
            ),
            Some(r"\??\C:\root\mod.esp".to_string())
        );
        // Windows' form: the VIRTUAL path's drive resolved to a device, with the
        // virtual path's volume-relative remainder appended. Note the device
        // comes from the virtual drive, not from the real answer's device --
        // the backing file may live on another volume entirely.
        assert_eq!(
            spoofed_object_name(
                r"\Device\HarddiskVolume7\backing\blob.dat",
                r"\??\C:\root\mod.esp",
                |d| {
                    assert_eq!(d, "C:");
                    Some(r"\Device\HarddiskVolume3".to_string())
                },
            ),
            Some(r"\Device\HarddiskVolume3\root\mod.esp".to_string())
        );
    }

    /// Everything this function cannot build honestly must come back `None`, so
    /// the hook passes the host's own answer through. A wrong name is worse
    /// than the backing one: it is undiagnosable.
    #[test]
    fn spoofed_object_name_declines_rather_than_guesses() {
        let dev = |_: &str| Some(r"\Device\HarddiskVolume3".to_string());
        // A convention we do not recognise. `\Device\Mup\...`-style names reach
        // the `\Device\` branch legitimately, but a bare NT object path such as
        // a named pipe or a mailslot root does not.
        assert_eq!(
            spoofed_object_name(r"\BaseNamedObjects\SomeMutex", r"\??\C:\root\mod.esp", dev),
            None
        );
        assert_eq!(spoofed_object_name("", r"\??\C:\root\mod.esp", dev), None);
        // A virtual path with no drive letter to resolve.
        assert_eq!(
            spoofed_object_name(r"\Device\HarddiskVolume3\x", r"\??\UNC\server\share\f", dev),
            None
        );
        assert_eq!(
            spoofed_object_name(r"\Device\HarddiskVolume3\x", r"\??\", dev),
            None
        );
        // `C:relative` is not a rooted path; appending it to a device prefix
        // would splice two names together (`\Device\HarddiskVolume3relative`).
        assert_eq!(
            spoofed_object_name(r"\Device\HarddiskVolume3\x", r"\??\C:relative", dev),
            None
        );
        // The device lookup failing is a decline, not a fallback: with no
        // device name there is nothing to build the Windows form out of.
        assert_eq!(
            spoofed_object_name(r"\Device\HarddiskVolume3\x", r"\??\C:\root\mod.esp", |_| {
                None
            }),
            None
        );
    }

    /// A drive root has an empty remainder, and both prefixes must survive it
    /// rather than producing a trailing-separator variant of the volume name.
    #[test]
    fn spoofed_object_name_handles_a_drive_root() {
        assert_eq!(
            spoofed_object_name(r"\??\C:\x", r"\??\C:", |_| unreachable!()),
            Some(r"\??\C:".to_string())
        );
        assert_eq!(
            spoofed_object_name(r"\Device\HarddiskVolume3\x", r"\??\C:", |_| Some(
                r"\Device\HarddiskVolume3".to_string()
            )),
            Some(r"\Device\HarddiskVolume3".to_string())
        );
    }

    /// `PATH_TABLE` holds `\??\`-prefixed paths today, but `record_path` stores
    /// whatever the open was decoded as. The `\\?\` long-path prefix and a bare
    /// Win32 path must both be understood, or a handle opened by one of those
    /// spellings silently declines the spoof and leaks.
    #[test]
    fn spoofed_object_name_accepts_every_prefix_path_table_can_hold() {
        for vpath in [
            r"\??\C:\root\mod.esp",
            r"\\?\C:\root\mod.esp",
            r"C:\root\mod.esp",
        ] {
            assert_eq!(
                spoofed_object_name(r"\??\C:\backing", vpath, |_| unreachable!()),
                Some(r"\??\C:\root\mod.esp".to_string()),
                "vpath = {vpath}"
            );
        }
    }
}
