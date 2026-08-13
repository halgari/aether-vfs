//! CLI argument parsing for the `vfs-injector` binary, extracted here so it's
//! unit-testable (a `[[bin]]` target isn't importable by integration tests).

/// What `vfs-injector` needs from its command line.
#[derive(Debug, PartialEq, Eq)]
pub struct InjectorArgs {
    pub target: String,
    pub shim_dll: String,
    pub payload_dll: String,
    pub config: String,
    pub ready: String,
    pub target_args: Vec<String>,
}

/// Parse argv into [`InjectorArgs`], or `Err(usage)`.
pub fn parse_injector_args(a: &[String]) -> Result<InjectorArgs, String> {
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
    Ok(InjectorArgs {
        target: a[1].clone(),
        shim_dll: a[2].clone(),
        payload_dll: a[3].clone(),
        config: a[4].clone(),
        ready: a[5].clone(),
        target_args,
    })
}
