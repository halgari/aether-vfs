fn main() {
    // Emits the cdylib link args N-API addons need on Windows. It does **not**
    // need a Node installation: napi-sys resolves `napi_*` from the host
    // process at runtime via `GetProcAddress`, so `cargo build` / `cargo
    // clippy` work on a machine with no Node at all. Only loading the addon
    // needs Node.
    napi_build::setup();
}
