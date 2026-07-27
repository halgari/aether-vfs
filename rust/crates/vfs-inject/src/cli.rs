//! CLI argument parsing for the `vfs-injector` binary, extracted here so it's
//! unit-testable (a `[[bin]]` target isn't importable by integration tests).

/// Parse argv into (target, shim, payload, config, ready, target_args), or Err(usage).
pub fn parse_injector_args(
    a: &[String],
) -> Result<(String, String, String, String, String, Vec<String>), String> {
    if a.len() < 6 {
        return Err(
            "usage: vfs-injector <target> <shim_dll> <payload_dll> <config> <ready> [-- args...]"
                .into(),
        );
    }
    let target_args = if a.len() > 6 && a[6] == "--" {
        a[7..].to_vec()
    } else {
        a[6..].to_vec()
    };
    Ok((
        a[1].clone(),
        a[2].clone(),
        a[3].clone(),
        a[4].clone(),
        a[5].clone(),
        target_args,
    ))
}
