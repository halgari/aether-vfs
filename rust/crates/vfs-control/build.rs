//! Generate the tonic gRPC stubs from `proto/director.proto`.
//!
//! We point `PROTOC` at a vendored binary so the build does not depend on a
//! system `protoc` install (this repo builds on locked-down machines).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(protoc) = protoc_bin_vendored::protoc_bin_path() {
        std::env::set_var("PROTOC", protoc);
    }
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/director.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/director.proto");
    Ok(())
}
