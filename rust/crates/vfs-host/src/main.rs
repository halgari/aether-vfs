//! Neutral hollow host.
//!
//! `vfs-inject` needs a *real on-disk* EXE to `CreateProcess` before it can
//! hollow a game image into it — Windows cannot create a process from bytes
//! alone. Which EXE that is turned out to matter a great deal, and every
//! constraint below came out of a measured failure rather than a guess:
//!
//! - **No game-local imports.** Using the game's own EXE as the host means the
//!   loader must resolve `steam_api64.dll` / `bink2w64.dll` at process init,
//!   *before* the shim is live to serve them. With those absent the process
//!   dies `0xC0000135` (`STATUS_DLL_NOT_FOUND`) during bootstrap. This host
//!   imports only what the CRT pulls from System32, so that cannot happen —
//!   and the game's own imports are then loaded through the shim, from the
//!   zip, once hooks are up.
//!
//! - **A large image reservation.** Hollowing a 59 MiB game image into a small
//!   host (`cmd.exe` is ~0.3 MiB) forces the "allocating zip image" path
//!   instead of an in-place write, which fails with `RPM u32 failed`. The BSS
//!   pad below reserves enough VA that the game image fits in place. BSS costs
//!   virtual size only, so the file on disk stays tiny.
//!
//! - **A fixed, game-preferred base.** Linked at `0x140000000` with ASLR off
//!   (see `build.rs`). Relocation is avoidable work, and without a fixed base
//!   the zip-served DLLs can land in the range the game image wants — in one
//!   run `bink2w64` took `0x80000000`.
//!
//! The process is created suspended and hollowed before it ever runs, so
//! `main` is effectively unreachable; it parks rather than exits so that a
//! host resumed without a hollow does not look like a crash.

/// Virtual bytes reserved for the hollowed image.
///
/// Skyrim SE's `SizeOfImage` is 0x3870000 (~56.4 MiB). Round well past it so
/// other Bethesda-era images fit without another rebuild.
const IMAGE_PAD_BYTES: usize = 128 * 1024 * 1024;

/// Zero-initialised, so it lands in `.bss`: it inflates `SizeOfImage` (the VA
/// the loader reserves) without adding a single byte to the file.
///
/// `#[used]` and the volatile read in `main` keep the linker from discarding
/// it — the reservation *is* the payload.
#[used]
static mut IMAGE_PAD: [u8; IMAGE_PAD_BYTES] = [0; IMAGE_PAD_BYTES];

fn main() {
    // Touch the pad so it cannot be optimised away, without committing pages.
    let probe = unsafe { std::ptr::addr_of!(IMAGE_PAD) as usize };
    eprintln!("vfs-host: neutral hollow host (image pad @ {probe:#x}, {IMAGE_PAD_BYTES} bytes)");
    eprintln!("vfs-host: not hollowed — parking so this is not mistaken for a crash");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
