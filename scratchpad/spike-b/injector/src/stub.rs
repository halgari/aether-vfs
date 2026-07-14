use std::arch::global_asm;

global_asm!(
    ".global callback_stub_start",
    ".global callback_stub_end",
    "callback_stub_start:",
    "pushfq",
    "push rax",
    "push rbx",
    "push rcx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push rbp",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "movabs r15, 0xDEADBEEFCAFEBABE",
    "cmp byte ptr [r15], 0",
    "jne callback_stub_skip_work",
    "mov byte ptr [r15], 1",
    "mov rax, gs:[0x60]",
    "mov rax, [rax+0x10]",
    "mov rbx, rax",
    "mov ecx, dword ptr [rax+0x3C]",
    "add rax, rcx",
    "mov ecx, dword ptr [rax+144]",
    "add rcx, rbx",
    "mov edx, dword ptr [rcx+16]",
    "add rdx, rbx",
    "mov rax, [rdx]",
    "mov [r15+8], rax",
    "cmp rax, rbx",
    "jb callback_stub_unsnapped",
    "mov byte ptr [r15+16], 2",
    "jmp callback_stub_store_r10",
    "callback_stub_unsnapped:",
    "mov byte ptr [r15+16], 1",
    "callback_stub_store_r10:",
    "mov [r15+24], r10",
    "callback_stub_skip_work:",
    "add dword ptr [r15+32], 1",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rbp",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "pop rbx",
    "pop rax",
    "popfq",
    "jmp r10",
    "callback_stub_end:",
);

extern "C" {
    fn callback_stub_start();
    fn callback_stub_end();
}

pub const DATA_PAGE_MAGIC: u64 = 0xDEAD_BEEF_CAFE_BABE;

/// Copies the compiled stub's own machine code out of THIS process's loaded
/// image. Position-independent aside from the one patched magic constant, so
/// the returned bytes are safe to write verbatim into another process.
pub fn stub_bytes() -> Vec<u8> {
    let start = callback_stub_start as usize;
    let end = callback_stub_end as usize;
    assert!(end > start, "bad stub symbol range: start={start:#x} end={end:#x}");
    let len = end - start;
    unsafe { std::slice::from_raw_parts(start as *const u8, len) }.to_vec()
}

/// Finds the 8-byte little-endian `DATA_PAGE_MAGIC` sequence in `stub` and
/// overwrites it with `addr`'s little-endian bytes.
pub fn patch_data_page_address(stub: &mut [u8], addr: u64) {
    let needle = DATA_PAGE_MAGIC.to_le_bytes();
    let pos = stub
        .windows(8)
        .position(|w| w == needle)
        .expect("magic constant not found in stub bytes");
    stub[pos..pos + 8].copy_from_slice(&addr.to_le_bytes());
}
