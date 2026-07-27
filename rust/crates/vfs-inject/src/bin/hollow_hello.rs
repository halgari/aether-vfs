fn main() {
    eprintln!("hollow-hello running pid={}", std::process::id());
    std::process::exit(42);
}
