pub const DATA_PAGE_SIZE: usize = 4096;

pub const OFFSET_FLAG: usize = 0;
pub const OFFSET_THUNK_VALUE: usize = 8;
pub const OFFSET_VERDICT: usize = 16;
pub const OFFSET_FIRST_R10: usize = 24;
pub const OFFSET_FIRE_COUNT: usize = 32;

pub struct Decoded {
    pub flag: u8,
    pub thunk_value: u64,
    pub verdict: u8,
    pub first_r10: u64,
    pub fire_count: u32,
}

pub fn decode(buf: &[u8]) -> Decoded {
    Decoded {
        flag: buf[OFFSET_FLAG],
        thunk_value: u64::from_le_bytes(
            buf[OFFSET_THUNK_VALUE..OFFSET_THUNK_VALUE + 8].try_into().unwrap(),
        ),
        verdict: buf[OFFSET_VERDICT],
        first_r10: u64::from_le_bytes(
            buf[OFFSET_FIRST_R10..OFFSET_FIRST_R10 + 8].try_into().unwrap(),
        ),
        fire_count: u32::from_le_bytes(
            buf[OFFSET_FIRE_COUNT..OFFSET_FIRE_COUNT + 4].try_into().unwrap(),
        ),
    }
}

impl Decoded {
    pub fn verdict_str(&self) -> &'static str {
        match self.verdict {
            0 => "NEVER RECORDED (callback did not fire before the process ran to completion, \
                  or fired but the classification branch never executed)",
            1 => "UNSNAPPED (Task B fires BEFORE the IAT is bound -> viable pre-init vehicle)",
            2 => "SNAPPED (Task B fires AFTER the IAT is bound -> too late for pre-init)",
            _ => "UNKNOWN (unexpected verdict byte - likely a stub bug)",
        }
    }
}
