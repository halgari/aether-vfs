//! Value information classes: `NtQueryValueKey`, `NtEnumerateValueKey` and
//! `NtQueryMultipleValueKey`.
use super::*;
use crate::overlay::Value;

/// `NtQueryValueKey` / `NtEnumerateValueKey` for one value (WRK `CmpQueryKeyValueData`).
pub fn write_value_info(class: ValueInfoClass, v: &Value, buf: &mut [u8]) -> Written {
    let data = v.data.as_slice();
    match class {
        // KEY_VALUE_BASIC_INFORMATION: TitleIndex@0 Type@4 NameLength@8 Name@12.
        ValueInfoClass::Basic => {
            let name = utf16le(&v.name);
            let total = 12 + name.len();
            if buf.len() < 12 {
                return too_small(total);
            }
            let mut o = Out(buf);
            o.u32(0, 0);
            o.u32(4, v.ty);
            o.u32(8, len32(name.len()));
            let fit = o.tail(12, &name);
            Written {
                status: status(fit),
                result_length: len32(total),
            }
        }
        // KEY_VALUE_FULL_INFORMATION: TitleIndex@0 Type@4 DataOffset@8 DataLength@12
        // NameLength@16 Name@20. With data, the data is at ALIGN8(20 + NameLength) for both
        // Full and FullAlign64: the x64 kernel aligns both to 8 (WRK `_WIN64` branch). Without
        // data there is no padding, and DataOffset is the end of the name, as Wine's ntdll
        // reports it. Windows reports -1 there, but Wine's own `RegEnumValueW` takes the data
        // length as ResultLength - DataOffset and copies that many bytes from DataOffset, so
        // -1 makes it copy from far outside the buffer and fault. The end of the name gives a
        // length of 0 to that reading and to every reader of DataLength alike.
        ValueInfoClass::Full | ValueInfoClass::FullAlign64 => {
            let name = utf16le(&v.name);
            let base = 20 + name.len();
            let off = if data.is_empty() {
                base
            } else {
                align(base, 8)
            };
            let total = off + data.len();
            if buf.len() < 20 {
                return too_small(total);
            }
            let mut o = Out(buf);
            o.u32(0, 0);
            o.u32(4, v.ty);
            o.u32(12, len32(data.len()));
            o.u32(16, len32(name.len()));
            let mut fit = o.tail(20, &name);
            o.u32(8, len32(off));
            fit &= o.tail(off, data);
            Written {
                status: status(fit),
                result_length: len32(total),
            }
        }
        // KEY_VALUE_PARTIAL_INFORMATION: TitleIndex@0 Type@4 DataLength@8 Data@12.
        ValueInfoClass::Partial => {
            let total = 12 + data.len();
            if buf.len() < 12 {
                return too_small(total);
            }
            let mut o = Out(buf);
            o.u32(0, 0);
            o.u32(4, v.ty);
            o.u32(8, len32(data.len()));
            let fit = o.tail(12, data);
            Written {
                status: status(fit),
                result_length: len32(total),
            }
        }
        // KEY_VALUE_PARTIAL_INFORMATION_ALIGN64: Type@0 DataLength@4 Data@8 (no TitleIndex).
        ValueInfoClass::PartialAlign64 => {
            let total = 8 + data.len();
            if buf.len() < 8 {
                return too_small(total);
            }
            let mut o = Out(buf);
            o.u32(0, v.ty);
            o.u32(4, len32(data.len()));
            let fit = o.tail(8, data);
            Written {
                status: status(fit),
                result_length: len32(total),
            }
        }
    }
}

/// `NtQueryMultipleValueKey`, following WRK `CmQueryMultipleValueKey`:
/// - each value's data goes at the next ULONG (4-byte) aligned offset in `buf`;
/// - once one value does not fit, the status is `STATUS_BUFFER_OVERFLOW` and no later value is
///   copied or has its entry filled, even if it would fit; `result_length` keeps counting;
/// - `entries[i]` is set only for a copied value; the others are left as passed in, so the shim
///   seeds `entries` from the caller's array and writes them all back;
/// - a `None` (value not found) stops at once with `STATUS_OBJECT_NAME_NOT_FOUND`; earlier
///   values stay copied, and the caller's `*BufferLength` and `*RequiredBufferLength` are left
///   unchanged.
///
/// On success or overflow the shim writes `buffer_length` (bytes used) to `*BufferLength` and
/// `result_length` (bytes required) to `*RequiredBufferLength`.
pub fn write_multiple_values(
    values: &[Option<&Value>],
    entries: &mut [ValueEntry],
    buf: &mut [u8],
) -> MultipleWritten {
    debug_assert_eq!(values.len(), entries.len(), "one entry per value");
    let mut used = 0usize;
    let mut required = 0usize;
    let mut full = false;
    let mut status = STATUS_SUCCESS;
    for (v, entry) in values.iter().zip(entries.iter_mut()) {
        let Some(v) = v else {
            return MultipleWritten {
                status: STATUS_OBJECT_NAME_NOT_FOUND,
                buffer_length: len32(used),
                result_length: len32(required),
            };
        };
        let len = v.data.len();
        used = align(used, 4);
        required = align(required, 4);
        if !full && used + len <= buf.len() {
            buf[used..used + len].copy_from_slice(&v.data);
            *entry = ValueEntry {
                data_length: len32(len),
                data_offset: len32(used),
                ty: v.ty,
            };
            used += len;
        } else {
            full = true;
            status = STATUS_BUFFER_OVERFLOW;
        }
        required += len;
    }
    MultipleWritten {
        status,
        buffer_length: len32(used),
        result_length: len32(required),
    }
}

/// sizeof(KEY_VALUE_ENTRY) on x64: ValueName@0 (pointer) DataLength@8 DataOffset@12 Type@16.
pub const KEY_VALUE_ENTRY_SIZE: usize = 24;

impl ValueEntry {
    /// Write this entry into a caller's x64 KEY_VALUE_ENTRY slot, leaving ValueName unchanged.
    pub fn store(&self, slot: &mut [u8]) {
        let mut o = Out(slot);
        o.u32(8, self.data_length);
        o.u32(12, self.data_offset);
        o.u32(16, self.ty);
    }
}
