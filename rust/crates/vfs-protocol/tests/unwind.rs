//! The workspace must unwind on panic: `catch_unwind` is a no-op under
//! `panic = "abort"`, and the PyO3 binding needs panics to become exceptions.

#[test]
fn workspace_panics_unwind_rather_than_abort() {
    let caught = std::panic::catch_unwind(|| {
        panic!("intentional");
    });
    assert!(caught.is_err(), "panic did not unwind — check profile.panic in the root Cargo.toml");
}
