//! Test target: read the file at argv[1] and write its bytes to argv[2].
//! When injected, argv[1] is a VIRTUAL path the shim redirects to a mod file.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        std::process::exit(2);
    }
    let content = std::fs::read(&args[1]).unwrap_or_default();
    std::fs::write(&args[2], &content).expect("probe write output");
}
