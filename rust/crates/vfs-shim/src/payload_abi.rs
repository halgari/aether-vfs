//! Mirror of the early payload's `#[repr(C)]` Config so the full shim can
//! publish secondary dispatch pointers into the reflectively-mapped Config
//! without linking the zero-import cdylib.
//!
//! MUST match `vfs-payload::Config` / `vfs-inject::payload_cfg::PayloadConfig`
//! field-for-field.

pub const MAX_REDIRECTS: usize = 4;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RedirectEntry {
    pub suffix_ptr: usize,
    pub suffix_wlen: u32,
    pub backing_ptr: usize,
    pub backing_wlen: u32,
    pub backing_size: u64,
}

#[repr(C)]
pub struct PayloadConfig {
    pub nt_protect: usize,
    pub open_target: usize,
    pub open_tramp: usize,
    pub qattr_target: usize,
    pub qattr_tramp: usize,
    pub qfull_target: usize,
    pub qfull_tramp: usize,
    pub create_target: usize,
    pub create_tramp: usize,
    pub install_mask: u32,
    pub redirect_count: u32,
    pub redirects: [RedirectEntry; MAX_REDIRECTS],
    pub counters: usize,
    pub secondary_open: usize,
    pub secondary_create: usize,
    pub secondary_qattr: usize,
    pub secondary_qfull: usize,
}
