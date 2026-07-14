//! Host-side mirror of `vfs_payload::Config` — must match field-for-field.
#![allow(unsafe_code)]

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

impl Default for RedirectEntry {
    fn default() -> Self {
        Self {
            suffix_ptr: 0,
            suffix_wlen: 0,
            backing_ptr: 0,
            backing_wlen: 0,
            backing_size: 0,
        }
    }
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

impl Default for PayloadConfig {
    fn default() -> Self {
        Self {
            nt_protect: 0,
            open_target: 0,
            open_tramp: 0,
            qattr_target: 0,
            qattr_tramp: 0,
            qfull_target: 0,
            qfull_tramp: 0,
            create_target: 0,
            create_tramp: 0,
            install_mask: 0,
            redirect_count: 0,
            redirects: [RedirectEntry::default(); MAX_REDIRECTS],
            counters: 0,
            secondary_open: 0,
            secondary_create: 0,
            secondary_qattr: 0,
            secondary_qfull: 0,
        }
    }
}

impl PayloadConfig {
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: plain POD repr(C).
        unsafe {
            core::slice::from_raw_parts(
                (self as *const PayloadConfig).cast::<u8>(),
                core::mem::size_of::<PayloadConfig>(),
            )
        }
    }
}
