extern "C" {
    fn helper_value() -> u32;
}

fn main() {
    let value = unsafe { helper_value() };
    std::process::exit(value as i32);
}
