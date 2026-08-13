//! The main workspace must unwind so the PyO3 binding can turn a Rust panic
//! into a Python exception instead of aborting the host process.
//!
//! This asserts the manifest rather than calling `catch_unwind`: Cargo always
//! builds `--test` harnesses with `panic = "unwind"` regardless of the profile
//! setting, so a runtime check passes under both and proves nothing.

#[test]
fn main_workspace_profiles_unwind() {
    let manifest = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"),
    )
    .expect("read the workspace manifest");

    let panic_lines: Vec<&str> = manifest
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("panic"))
        .collect();

    assert!(
        !panic_lines.is_empty(),
        "no panic setting found in the workspace manifest"
    );
    for line in panic_lines {
        assert!(
            line.contains("unwind"),
            "main workspace must unwind, found: {line}"
        );
    }
}

#[test]
fn vfs_payload_is_excluded_and_still_aborts() {
    let root = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"),
    )
    .expect("read the workspace manifest");
    assert!(
        root.contains(r#"exclude = ["crates/vfs-payload"]"#),
        "vfs-payload must stay excluded — it is #![no_std] and cannot unwind"
    );

    let payload = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../vfs-payload/Cargo.toml"
    ))
    .expect("read the vfs-payload manifest");
    assert!(
        payload.lines().map(str::trim).any(|l| l == r#"panic = "abort""#),
        "vfs-payload must keep panic = \"abort\""
    );
    assert!(
        payload.contains("[workspace]"),
        "vfs-payload must be its own workspace root"
    );
}
