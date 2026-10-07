//! **A hard link with an end under a managed root** never reaches the real filesystem.
//!
//! `FileLinkInformation`/`FileLinkInformationEx` have the rename classes' layout, and they were
//! not intercepted: a real handle outside every root, linked to a name under a root, went to the
//! trampoline, and the kernel created a real file under a root that seals everything the provider
//! graph does not serve. The director has no link operation, so every link with an end under a
//! root is refused with `STATUS_ACCESS_DENIED`, as a rename into a root from outside is.
//!
//! Checked with the hooks down (`write_seal`'s method): the destination directory is the
//! witness, since a link that was made reports no other trace at the API. The control is a link
//! with both ends outside every root, which must still work. Its own process, because the
//! detours and the `FuseClient` are process-global.

use crate::fakedirector;
use crate::ntapi;

use fakedirector::{Fake, ReadStyle};
use vfs_shim::{install, link_refused_count};

const BYTES: &[u8] = b"outside: to be linked";
const DELETE: u32 = ntapi::DELETE;

#[test]
fn a_hard_link_touching_a_managed_root_is_refused_and_one_outside_works() {
    isolate!();
    let base = std::env::temp_dir().join(format!("vfs-link-seal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let outside = base.join("outside");
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    for n in ["std.esp", "nt.esp", "nt-ex.esp", "ctl.esp", "ctl-nt.esp"] {
        std::fs::write(outside.join(n), BYTES).unwrap();
    }

    let fake = fakedirector::install(
        &root,
        Fake::new()
            .with("data/existing.esp", b"director".to_vec(), ReadStyle::Whole)
            .writable_under("data/"),
        0,
    );
    let hooks = install().expect("install");
    let refused_before = link_refused_count();

    // Into the root from outside: the Win32 route and both NT classes.
    let std_result = std::fs::hard_link(outside.join("std.esp"), root.join("data").join("std.esp"));
    let nt_into = |name: &str, class: u32| {
        let (st, h) = ntapi::nt_open_abs(&outside.join(name).to_string_lossy(), DELETE);
        assert!(st >= 0, "opening the outside source failed: {st:#x}");
        let r = ntapi::nt_rename(h, &root.join("data").join(name).to_string_lossy(), class);
        ntapi::close(h);
        r
    };
    let nt_status = nt_into("nt.esp", ntapi::FILE_LINK_INFORMATION);
    let nt_ex_status = nt_into("nt-ex.esp", ntapi::FILE_LINK_INFORMATION_EX);
    let refused = link_refused_count() - refused_before;

    // A link *from* a served (synthetic) handle to somewhere outside.
    let (st, h) = ntapi::nt_open_abs(
        &root.join("data").join("existing.esp").to_string_lossy(),
        DELETE,
    );
    assert!(st >= 0, "opening the served file failed: {st:#x}");
    let from_served = ntapi::nt_rename(
        h,
        &outside.join("from-served.esp").to_string_lossy(),
        ntapi::FILE_LINK_INFORMATION,
    );
    ntapi::close(h);

    // A target that cannot be decoded (declared name length past the buffer) fails closed.
    let (st, h) = ntapi::nt_open_abs(&outside.join("ctl.esp").to_string_lossy(), DELETE);
    assert!(st >= 0, "opening the control source failed: {st:#x}");
    let mut bad = vec![0u8; 24];
    bad[16..20].copy_from_slice(&200u32.to_le_bytes());
    let undecodable = ntapi::nt_set_info_raw(h, &mut bad, ntapi::FILE_LINK_INFORMATION);
    ntapi::close(h);

    // Control: both ends outside every root.
    let ctl_std = std::fs::hard_link(outside.join("ctl.esp"), outside.join("ctl-link.esp"));
    let (st, h) = ntapi::nt_open_abs(&outside.join("ctl-nt.esp").to_string_lossy(), DELETE);
    assert!(st >= 0, "opening the control source failed: {st:#x}");
    let ctl_nt = ntapi::nt_rename(
        h,
        &outside.join("ctl-nt-link.esp").to_string_lossy(),
        ntapi::FILE_LINK_INFORMATION,
    );
    ntapi::close(h);

    drop(hooks);

    // Filesystem first, with the hooks down.
    for name in ["std.esp", "nt.esp", "nt-ex.esp"] {
        assert!(
            !root.join("data").join(name).exists(),
            "{name} exists on real disk under the managed root: the link was made by the kernel"
        );
        assert_eq!(
            fake.contents(&format!("data/{name}")),
            None,
            "{name} is in the director's table"
        );
    }
    assert!(
        !outside.join("from-served.esp").exists(),
        "a link from a served handle was made"
    );

    assert!(
        std_result.is_err(),
        "std::fs::hard_link into a managed root reported success"
    );
    assert_eq!(
        nt_status,
        ntapi::STATUS_ACCESS_DENIED,
        "FILE_LINK_INFORMATION into a root: {nt_status:#x}"
    );
    assert_eq!(
        nt_ex_status,
        ntapi::STATUS_ACCESS_DENIED,
        "FILE_LINK_INFORMATION_EX into a root: {nt_ex_status:#x}"
    );
    assert_eq!(
        from_served,
        ntapi::STATUS_ACCESS_DENIED,
        "a link from a served handle: {from_served:#x}"
    );
    assert_eq!(
        undecodable,
        ntapi::STATUS_ACCESS_DENIED,
        "an undecodable link target reached the kernel: {undecodable:#x}"
    );
    assert!(refused >= 2, "the refusals were not counted ({refused})");

    // Control: untouched.
    ctl_std.expect("a hard link with both ends outside every root must still work");
    assert_eq!(std::fs::read(outside.join("ctl-link.esp")).unwrap(), BYTES);
    assert!(
        ctl_nt >= 0,
        "an NT link outside every root failed: {ctl_nt:#x}"
    );
    assert_eq!(
        std::fs::read(outside.join("ctl-nt-link.esp")).unwrap(),
        BYTES
    );

    let _ = std::fs::remove_dir_all(&base);
}
