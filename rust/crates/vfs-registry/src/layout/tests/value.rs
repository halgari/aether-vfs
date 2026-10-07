use super::*;

#[test]
fn value_basic_exact_overflow_too_small() {
    let v = val("Ab", 4, &[1, 2, 3, 4, 5]);
    let full = cat(&[&ZERO, &n(4), &n(4), &w("Ab")]);
    let (r, b) = vq(ValueInfoClass::Basic, &v, 16);
    assert_eq!(r, ok(16));
    assert_eq!(b, full);
    for len in [12, 14, 15] {
        let (r, b) = vq(ValueInfoClass::Basic, &v, len);
        assert_eq!(r, overflow(16));
        assert_eq!(b, full[..len]);
    }
    for len in [0, 11] {
        let (r, b) = vq(ValueInfoClass::Basic, &v, len);
        assert_eq!(r, too_small(16));
        assert_eq!(b, pad(len));
    }
}

// ---- KEY_VALUE_FULL_INFORMATION: TitleIndex@0 Type@4 DataOffset@8 DataLength@12
//      NameLength@16 Name@20, data at ALIGN8(20 + NameLength) on x64 ----

fn full_abc() -> Vec<u8> {
    // Name ends at 26; data at 32; 26..32 padding left alone.
    cat(&[
        &ZERO,
        &n(3),
        &n(32),
        &n(5),
        &n(6),
        &w("Abc"),
        &pad(6),
        &[1, 2, 3, 4, 5],
    ])
}

#[test]
fn value_full_aligns_data_to_8() {
    let v = val("Abc", 3, &[1, 2, 3, 4, 5]);
    for class in [ValueInfoClass::Full, ValueInfoClass::FullAlign64] {
        let (r, b) = vq(class, &v, 37);
        assert_eq!(r, ok(37), "{class:?}");
        assert_eq!(b, full_abc(), "{class:?}");
    }
}

#[test]
fn value_full_name_already_aligned() {
    let v = val("Ab", 4, &[1, 2, 3, 4, 5]);
    let expected = cat(&[
        &ZERO,
        &n(4),
        &n(24),
        &n(5),
        &n(4),
        &w("Ab"),
        &[1, 2, 3, 4, 5],
    ]);
    for class in [ValueInfoClass::Full, ValueInfoClass::FullAlign64] {
        let (r, b) = vq(class, &v, 29);
        assert_eq!(r, ok(29), "{class:?}");
        assert_eq!(b, expected, "{class:?}");
    }
}

#[test]
fn value_full_without_data() {
    let v = val("Abc", 1, &[]);
    // DataOffset is the end of the name (26), not -1: see `write_value_info`.
    let expected = cat(&[&ZERO, &n(1), &n(26), &ZERO, &n(6), &w("Abc")]);
    for class in [ValueInfoClass::Full, ValueInfoClass::FullAlign64] {
        let (r, b) = vq(class, &v, 26);
        assert_eq!(r, ok(26), "{class:?}");
        assert_eq!(b, expected, "{class:?}");
    }
}

#[test]
fn value_full_overflow_cases() {
    let v = val("Abc", 3, &[1, 2, 3, 4, 5]);
    for class in [ValueInfoClass::Full, ValueInfoClass::FullAlign64] {
        // Fixed part; part of the name; end inside the padding; part of the data.
        for len in [20, 23, 28, 34, 36] {
            let (r, b) = vq(class, &v, len);
            assert_eq!(r, overflow(37), "{class:?} {len}");
            assert_eq!(b, full_abc()[..len], "{class:?} {len}");
        }
        for len in [0, 19] {
            let (r, b) = vq(class, &v, len);
            assert_eq!(r, too_small(37), "{class:?}");
            assert_eq!(b, pad(len));
        }
    }
}

// ---- KEY_VALUE_PARTIAL_INFORMATION: TitleIndex@0 Type@4 DataLength@8 Data@12 ----

#[test]
fn value_partial_exact_overflow_too_small() {
    let v = val("ignored", 4, &[1, 2, 3, 4, 5]);
    let full = cat(&[&ZERO, &n(4), &n(5), &[1, 2, 3, 4, 5]]);
    let (r, b) = vq(ValueInfoClass::Partial, &v, 17);
    assert_eq!(r, ok(17));
    assert_eq!(b, full);
    // Review Focus 2: header fits, data does not.
    for len in [12, 14, 16] {
        let (r, b) = vq(ValueInfoClass::Partial, &v, len);
        assert_eq!(r, overflow(17));
        assert_eq!(b, full[..len]);
    }
    for len in [0, 1, 11] {
        let (r, b) = vq(ValueInfoClass::Partial, &v, len);
        assert_eq!(r, too_small(17));
        assert_eq!(b, pad(len));
    }
}

#[test]
fn value_partial_without_data() {
    let v = val("x", 0xff00_ff00, &[]);
    let (r, b) = vq(ValueInfoClass::Partial, &v, 12);
    assert_eq!(r, ok(12));
    assert_eq!(b, cat(&[&ZERO, &n(0xff00_ff00), &ZERO]));
}

// ---- KEY_VALUE_PARTIAL_INFORMATION_ALIGN64: Type@0 DataLength@4 Data@8 ----

#[test]
fn value_partial_align64_exact_overflow_too_small() {
    let v = val("ignored", 4, &[1, 2, 3, 4, 5]);
    let full = cat(&[&n(4), &n(5), &[1, 2, 3, 4, 5]]);
    let (r, b) = vq(ValueInfoClass::PartialAlign64, &v, 13);
    assert_eq!(r, ok(13));
    assert_eq!(b, full);
    for len in [8, 10] {
        let (r, b) = vq(ValueInfoClass::PartialAlign64, &v, len);
        assert_eq!(r, overflow(13));
        assert_eq!(b, full[..len]);
    }
    for len in [0, 7] {
        let (r, b) = vq(ValueInfoClass::PartialAlign64, &v, len);
        assert_eq!(r, too_small(13));
        assert_eq!(b, pad(len));
    }
}

// ---- NtQueryMultipleValueKey ----

const SEED: ValueEntry = ValueEntry {
    data_length: 0xAAAA,
    data_offset: 0xBBBB,
    ty: 0xCCCC,
};

#[test]
fn multiple_all_fit_ulong_aligned() {
    let (a, b, c) = (
        val("a", 3, &[1, 2, 3]),
        val("b", 4, &[4, 5, 6, 7]),
        val("c", 1, &[8]),
    );
    let mut entries = [SEED; 3];
    let mut buf = vec![S; 9 + 4];
    let r = write_multiple_values(&[Some(&a), Some(&b), Some(&c)], &mut entries, &mut buf[..9]);
    assert_eq!(
        r,
        MultipleWritten {
            status: STATUS_SUCCESS,
            buffer_length: 9,
            result_length: 9
        }
    );
    assert_eq!(buf, [1, 2, 3, S, 4, 5, 6, 7, 8, S, S, S, S]);
    let e = |data_length, data_offset, ty| ValueEntry {
        data_length,
        data_offset,
        ty,
    };
    assert_eq!(entries, [e(3, 0, 3), e(4, 4, 4), e(1, 8, 1)]);
}

#[test]
fn multiple_overflow_stops_filling_at_the_first_value_that_does_not_fit() {
    let (a, b, c) = (
        val("a", 3, &[1, 2, 3]),
        val("b", 4, &[0x55; 10]),
        val("c", 1, &[8]),
    );
    let mut entries = [SEED; 3];
    let mut buf = vec![S; 6];
    let r = write_multiple_values(&[Some(&a), Some(&b), Some(&c)], &mut entries, &mut buf);
    // c would fit at offset 4, but WRK stops copying once the buffer is full.
    // BufferLength is the used length, rounded up before the value that did not fit.
    assert_eq!(
        r,
        MultipleWritten {
            status: STATUS_BUFFER_OVERFLOW,
            buffer_length: 4,
            result_length: 17
        }
    );
    assert_eq!(buf, [1, 2, 3, S, S, S]);
    assert_eq!(
        entries,
        [
            ValueEntry {
                data_length: 3,
                data_offset: 0,
                ty: 3
            },
            SEED,
            SEED
        ]
    );
}

#[test]
fn multiple_missing_value_is_name_not_found() {
    let (a, c) = (val("a", 3, &[1, 2, 3]), val("c", 1, &[8]));
    let mut entries = [SEED; 3];
    let mut buf = vec![S; 16];
    let r = write_multiple_values(&[Some(&a), None, Some(&c)], &mut entries, &mut buf);
    assert_eq!(r.status, STATUS_OBJECT_NAME_NOT_FOUND);
    assert_eq!(buf[..4], [1, 2, 3, S]);
    assert_eq!(buf[4..], pad(12));
    assert_eq!(entries[1..], [SEED, SEED]);
    assert_eq!(entries[0].data_length, 3);
}

#[test]
fn multiple_empty_data_into_empty_buffer() {
    let a = val("a", 7, &[]);
    let mut entries = [SEED; 1];
    let r = write_multiple_values(&[Some(&a)], &mut entries, &mut []);
    assert_eq!(
        r,
        MultipleWritten {
            status: STATUS_SUCCESS,
            buffer_length: 0,
            result_length: 0
        }
    );
    assert_eq!(
        entries[0],
        ValueEntry {
            data_length: 0,
            data_offset: 0,
            ty: 7
        }
    );
}

#[test]
fn value_entry_store_leaves_the_name_pointer() {
    // KEY_VALUE_ENTRY (x64): ValueName@0 (8) DataLength@8 DataOffset@12 Type@16, size 24.
    let mut slot = [S; KEY_VALUE_ENTRY_SIZE];
    ValueEntry {
        data_length: 0x0102_0304,
        data_offset: 0x10,
        ty: 4,
    }
    .store(&mut slot);
    let expected = cat(&[&pad(8), &n(0x0102_0304), &n(0x10), &n(4), &pad(4)]);
    assert_eq!(slot[..], expected[..]);
}
