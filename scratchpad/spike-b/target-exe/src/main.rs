extern "C" {
    fn helper_value() -> u32;
}

fn main() {
    let value = unsafe { helper_value() };
    // Deliberate pause: gives an external observer a window to read this
    // process's memory before it exits and its address space is reclaimed.
    std::thread::sleep(std::time::Duration::from_millis(1000));
    std::process::exit(value as i32);
}
