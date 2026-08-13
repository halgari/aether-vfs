//! PIC stub run via the redirected primary-thread RIP: writes a sentinel,
//! preserves rcx/rdx (RtlUserThreadStart args), aligns the stack, calls
//! shim_install(remote_config), optionally spins on a release flag (dual-layer
//! gate), restores, then jumps to the original RIP.
#![allow(unsafe_code)]

/// Build the hand-assembled x64 stub bytes.
///
/// `release_flag` — if non-zero, address of a u32 the stub spins on until
/// non-zero after install (injector sets it after full-shim LoadLibrary).
pub fn build_stub(
    remote_config: u64,
    remote_install: u64,
    orig_rip: u64,
    counters: u64,
    release_flag: u64,
) -> Vec<u8> {
    let mut s = Vec::with_capacity(96);
    // Sentinel: mov rax, counters; mov dword [rax+0x1C], 0xC0DE  (counters[7])
    s.extend_from_slice(&[0x48, 0xB8]);
    s.extend_from_slice(&counters.to_le_bytes());
    s.extend_from_slice(&[0xC7, 0x40, 0x1C, 0xDE, 0xC0, 0x00, 0x00]);
    s.push(0x51); // push rcx
    s.push(0x52); // push rdx
    s.push(0x53); // push rbx
    s.extend_from_slice(&[0x48, 0x89, 0xE3]); // mov rbx, rsp
    s.extend_from_slice(&[0x48, 0x83, 0xE4, 0xF0]); // and rsp, -16
    s.extend_from_slice(&[0x48, 0x83, 0xEC, 0x20]); // sub rsp, 0x20
    s.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
    s.extend_from_slice(&remote_config.to_le_bytes());
    s.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    s.extend_from_slice(&remote_install.to_le_bytes());
    s.extend_from_slice(&[0xFF, 0xD0]); // call rax
    s.extend_from_slice(&[0x48, 0x89, 0xDC]); // mov rsp, rbx
    s.push(0x5B); // pop rbx
    s.push(0x5A); // pop rdx
    s.push(0x59); // pop rcx

    if release_flag != 0 {
        // spin: mov rax, flag; cmp dword [rax], 0; je spin
        s.extend_from_slice(&[0x48, 0xB8]);
        s.extend_from_slice(&release_flag.to_le_bytes());
        // spin_loop:
        let spin = s.len();
        s.extend_from_slice(&[0x83, 0x38, 0x00]); // cmp dword ptr [rax], 0
        // je spin_loop (rel8)
        let je = s.len();
        s.extend_from_slice(&[0x74, 0x00]);
        s[je + 1] = (spin as i8 - (je as i8 + 2)) as u8;
        // pause (optional energy) — skip for simplicity
    }

    s.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    s.extend_from_slice(&orig_rip.to_le_bytes());
    s.extend_from_slice(&[0xFF, 0xE0]); // jmp rax
    s
}
