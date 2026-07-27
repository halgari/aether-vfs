//! Parent helper for child-inject tests: spawn a child with an explicit cwd
//! (so the child can live in an isolated app dir without a static-import DLL
//! on disk). Exit code = child's exit code.
//!
//! Usage: vfs-spawn-child <child_exe> <child_cwd> [child_args...]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: vfs-spawn-child <child_exe> <child_cwd> [child_args...]");
        std::process::exit(2);
    }
    let child_exe = &args[1];
    let child_cwd = &args[2];
    let child_args = &args[3..];
    let status = std::process::Command::new(child_exe)
        .current_dir(child_cwd)
        .args(child_args)
        .status();
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("spawn failed: {e}");
            std::process::exit(1);
        }
    }
}
