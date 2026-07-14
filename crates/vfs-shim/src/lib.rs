#![deny(unsafe_code)]

//! `vfs-shim`: installs NT detours that redirect virtualized paths to mod
//! backing files. Supports standalone in-process install and dual-layer
//! install_late (early payload owns the four path/attr stubs).

mod bootstrap;
mod engine;
mod hook;
mod inject;
mod ntdef;
mod overlay;
mod payload_abi;

pub use bootstrap::{
    bootstrap_from_config_path, bootstrap_from_config_path_with_payload, decode_config,
    decode_config_full, encode_config, encode_config_full, encode_config_with_overlay,
    load_static_imports_from_config_path, static_imports_to_preinit, sync_bootstrap,
    BootstrapError, StaticImport,
};
pub use engine::{Engine, EngineError};
pub use hook::{install, install_late, HookGuard, InstallError};
pub use payload_abi::PayloadConfig;
