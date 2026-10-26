//! This process's PEB and the fields of it the shim reads, at their offsets
//! for the build's pointer width: x64, and x86 (a 32-bit program, under
//! WOW64 on Windows or Wine).
//!
//! The PEB is reached through the thread's segment register: `gs:[0x60]` on
//! x64, `fs:[0x30]` on x86. Never `gs` on x86: under Wine's WOW64 mode `gs`
//! is not the TEB, and a read through it faults.

/// `PEB.Ldr`.
#[cfg(target_arch = "x86_64")]
pub(crate) const PEB_LDR: usize = 0x18;
#[cfg(target_arch = "x86")]
pub(crate) const PEB_LDR: usize = 0x0c;

/// `PEB.ProcessParameters`.
#[cfg(target_arch = "x86_64")]
pub(crate) const PEB_PROCESS_PARAMETERS: usize = 0x20;
#[cfg(target_arch = "x86")]
pub(crate) const PEB_PROCESS_PARAMETERS: usize = 0x10;

/// `PEB.LoaderLock`.
#[cfg(target_arch = "x86_64")]
pub(crate) const PEB_LOADER_LOCK: usize = 0x110;
#[cfg(target_arch = "x86")]
pub(crate) const PEB_LOADER_LOCK: usize = 0xa0;

/// `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory`: `{ UNICODE_STRING DosPath; HANDLE Handle }`.
#[cfg(target_arch = "x86_64")]
pub(crate) const PP_CURRENT_DIRECTORY: usize = 0x38;
#[cfg(target_arch = "x86")]
pub(crate) const PP_CURRENT_DIRECTORY: usize = 0x24;

/// `RTL_USER_PROCESS_PARAMETERS.ImagePathName`.
#[cfg(target_arch = "x86_64")]
pub(crate) const PP_IMAGE_PATH_NAME: usize = 0x60;
#[cfg(target_arch = "x86")]
pub(crate) const PP_IMAGE_PATH_NAME: usize = 0x38;

/// `PEB_LDR_DATA.InLoadOrderModuleList`.
#[cfg(target_arch = "x86_64")]
pub(crate) const LDR_IN_LOAD_ORDER: usize = 0x10;
#[cfg(target_arch = "x86")]
pub(crate) const LDR_IN_LOAD_ORDER: usize = 0x0c;

/// `LDR_DATA_TABLE_ENTRY.FullDllName`.
#[cfg(target_arch = "x86_64")]
pub(crate) const ENTRY_FULL_DLL_NAME: usize = 0x48;
#[cfg(target_arch = "x86")]
pub(crate) const ENTRY_FULL_DLL_NAME: usize = 0x24;

/// A `UNICODE_STRING`'s `Buffer`: after the two `u16` lengths, at the pointer's alignment.
pub(crate) const US_BUFFER: usize = core::mem::size_of::<usize>();

/// This process's PEB.
pub(crate) fn peb() -> usize {
    let peb: usize;
    // SAFETY: reads the current thread's TEB field through its segment
    // register; no memory is written.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("mov {}, gs:[0x60]", out(reg) peb, options(nostack, readonly, preserves_flags))
    };
    #[cfg(target_arch = "x86")]
    unsafe {
        core::arch::asm!("mov {}, fs:[0x30]", out(reg) peb, options(nostack, readonly, preserves_flags))
    };
    peb
}

/// This process's `RTL_USER_PROCESS_PARAMETERS`, or 0.
pub(crate) fn process_parameters() -> usize {
    let peb = peb();
    if peb == 0 {
        return 0;
    }
    // SAFETY: a field of this process's own PEB, which lives as long as it.
    unsafe { *((peb + PEB_PROCESS_PARAMETERS) as *const usize) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_curdir_handle_follows_its_dos_path() {
        // CURDIR = { UNICODE_STRING DosPath; HANDLE Handle }: the handle two pointers on
        let handle = PP_CURRENT_DIRECTORY + 2 * core::mem::size_of::<usize>();
        #[cfg(target_arch = "x86_64")]
        assert_eq!(0x48, handle);
        #[cfg(target_arch = "x86")]
        assert_eq!(0x2c, handle);
    }
}
