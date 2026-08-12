fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(protoc) = protoc_bin_vendored::protoc_bin_path() {
        std::env::set_var("PROTOC", protoc);
    }
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/source.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/source.proto");
    Ok(())
}
