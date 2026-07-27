use std::path::PathBuf;

fn main() {
    let out_dir: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../../resources"));
    std::fs::create_dir_all(&out_dir).expect("create out dir");
    std::fs::write(out_dir.join("protocol-descriptor.edn"), xtask_descriptor::descriptor_edn())
        .expect("write descriptor");
    std::fs::write(out_dir.join("protocol-golden.edn"), xtask_descriptor::golden_edn())
        .expect("write golden");
    println!("wrote descriptor to {}", out_dir.display());
}
