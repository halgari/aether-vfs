use super::*;

fn basic_mod() -> Vec<u8> {
    cat(&[&LW_BYTES, &ZERO, &n(6), &w("Mod")])
}

#[test]
fn key_basic_exact() {
    let (r, b) = kq(KeyInfoClass::Basic, &key(None), 22);
    assert_eq!(r, ok(22));
    assert_eq!(b, basic_mod());
}

#[test]
fn key_basic_overflow_copies_part_of_the_name() {
    let (r, b) = kq(KeyInfoClass::Basic, &key(None), 19);
    assert_eq!(r, overflow(22));
    assert_eq!(b, basic_mod()[..19]);
    // Exactly the fixed part: header only.
    let (r, b) = kq(KeyInfoClass::Basic, &key(None), 16);
    assert_eq!(r, overflow(22));
    assert_eq!(b, basic_mod()[..16]);
}

#[test]
fn key_basic_too_small_writes_nothing() {
    for len in [0, 1, 15] {
        let (r, b) = kq(KeyInfoClass::Basic, &key(None), len);
        assert_eq!(r, too_small(22));
        assert_eq!(b, pad(len));
    }
}

// ---- KEY_NODE_INFORMATION: LastWriteTime@0 TitleIndex@8 ClassOffset@12 ClassLength@16
//      NameLength@20 Name@24, Class at ALIGN4(24 + NameLength) ----

#[test]
fn key_node_without_class() {
    let (r, b) = kq(KeyInfoClass::Node, &key(None), 30);
    assert_eq!(r, ok(30));
    assert_eq!(b, cat(&[&LW_BYTES, &ZERO, &NONE, &ZERO, &n(6), &w("Mod")]));
}

fn node_mod_abc() -> Vec<u8> {
    // Name ends at 30; the class is at 32; bytes 30..32 are padding the call leaves alone.
    cat(&[
        &LW_BYTES,
        &ZERO,
        &n(32),
        &n(6),
        &n(6),
        &w("Mod"),
        &pad(2),
        &w("ABC"),
    ])
}

#[test]
fn key_node_with_class_is_ulong_aligned() {
    let (r, b) = kq(KeyInfoClass::Node, &key(Some("ABC")), 38);
    // ResultLength does not count the padding (WRK: 24 + NameLength + ClassLength).
    assert_eq!(r, ok(36));
    assert_eq!(b, node_mod_abc());
}

#[test]
fn key_node_buffer_of_result_length_overflows_by_the_padding() {
    // Windows quirk: a buffer of exactly ResultLength is 2 bytes short of the aligned class.
    let (r, b) = kq(KeyInfoClass::Node, &key(Some("ABC")), 36);
    assert_eq!(r, overflow(36));
    assert_eq!(b, node_mod_abc()[..36]);
}

#[test]
fn key_node_overflow_cases() {
    let k = key(Some("ABC"));
    // Name partly copied; class offset and lengths still the full values; no class bytes.
    let (r, b) = kq(KeyInfoClass::Node, &k, 27);
    assert_eq!(r, overflow(36));
    assert_eq!(b, node_mod_abc()[..27]);
    // Ends inside the padding: nothing of the class.
    let (r, b) = kq(KeyInfoClass::Node, &k, 31);
    assert_eq!(r, overflow(36));
    assert_eq!(b, node_mod_abc()[..31]);
    // Fixed part only.
    let (r, b) = kq(KeyInfoClass::Node, &k, 24);
    assert_eq!(r, overflow(36));
    assert_eq!(b, node_mod_abc()[..24]);
}

#[test]
fn key_node_name_already_aligned_has_no_padding() {
    let (r, b) = run(32, |b| {
        write_key_info(KeyInfoClass::Node, &key(Some("Z")), r"\REGISTRY\Ab", b)
    });
    assert_eq!(r, ok(30));
    assert_eq!(
        b,
        cat(&[
            &LW_BYTES,
            &ZERO,
            &n(28),
            &n(2),
            &n(4),
            &w("Ab"),
            &w("Z"),
            &pad(2)
        ])
    );
}

#[test]
fn key_node_too_small() {
    for len in [0, 23] {
        let (r, b) = kq(KeyInfoClass::Node, &key(Some("ABC")), len);
        assert_eq!(r, too_small(36));
        assert_eq!(b, pad(len));
    }
}

// ---- KEY_FULL_INFORMATION: LastWriteTime@0 TitleIndex@8 ClassOffset@12 ClassLength@16
//      SubKeys@20 MaxNameLen@24 MaxClassLen@28 Values@32 MaxValueNameLen@36
//      MaxValueDataLen@40 Class@44 ----

const P: &str = r"\Registry\Machine\Software\Mod";

/// A key whose counts depend on the merge: a tombstoned real subkey and value, an overlay
/// subkey with the longest name, an overlay value shadowing nothing.
fn merged() -> MergedKey {
    let real = RealKey {
        subkeys: vec!["alphabetical".into(), "Be".into()],
        values: vec![
            val("x", 1, &[1, 2, 3]),
            val("LongestValueName", 3, &[0; 100]),
        ],
        class: Some("CL".encode_utf16().collect()),
        last_write: LW,
        max_subkey_class_len: 8,
    };
    let mut o = Overlay::new();
    o.delete_key(&format!(r"{P}\ALPHABETICAL"), LW - 1).unwrap();
    o.create_key(&format!(r"{P}\LongerName12"), false, false, LW - 1)
        .unwrap();
    o.delete_value(P, "longestvaluename", LW - 1).unwrap();
    o.set_value(P, "Value", 4, &[9; 10], LW - 1).unwrap();
    merge(Some(&real), o.node(P), false).unwrap()
}

#[test]
fn key_full_counts_and_maxima_over_the_merged_view() {
    let m = merged();
    assert_eq!(m.subkeys, vec!["Be", "LongerName12"]);
    let (r, b) = kq(KeyInfoClass::Full, &m, 48);
    assert_eq!(r, ok(48));
    let expected = cat(&[
        &LW_BYTES,
        &ZERO,
        &n(44), // ClassOffset
        &n(4),  // ClassLength
        &n(2),  // SubKeys: Be, LongerName12 (alphabetical tombstoned)
        &n(24), // MaxNameLen: "LongerName12" in bytes
        &n(8),  // MaxClassLen: carried from the real key
        &n(2),  // Values: Value, x (LongestValueName tombstoned)
        &n(10), // MaxValueNameLen: "Value"
        &n(10), // MaxValueDataLen: Value's 10 bytes, not the hidden 100
        &w("CL"),
    ]);
    assert_eq!(b, expected);
}

fn full_plain() -> Vec<u8> {
    cat(&[
        &LW_BYTES,
        &ZERO,
        &n(44),
        &n(6),
        &ZERO,
        &ZERO,
        &ZERO,
        &ZERO,
        &ZERO,
        &ZERO,
        &w("ABC"),
    ])
}

#[test]
fn key_full_without_class() {
    let (r, b) = kq(KeyInfoClass::Full, &key(None), 44);
    assert_eq!(r, ok(44));
    assert_eq!(
        b,
        cat(&[
            &LW_BYTES, &ZERO, &NONE, &ZERO, &ZERO, &ZERO, &ZERO, &ZERO, &ZERO, &ZERO
        ])
    );
}

#[test]
fn key_full_overflow_copies_part_of_the_class() {
    let (r, b) = kq(KeyInfoClass::Full, &key(Some("ABC")), 47);
    assert_eq!(r, overflow(50));
    assert_eq!(b, full_plain()[..47]);
    let (r, b) = kq(KeyInfoClass::Full, &key(Some("ABC")), 44);
    assert_eq!(r, overflow(50));
    assert_eq!(b, full_plain()[..44]);
    let (r, b) = kq(KeyInfoClass::Full, &key(Some("ABC")), 50);
    assert_eq!(r, ok(50));
    assert_eq!(b, full_plain());
}

#[test]
fn key_full_too_small() {
    for len in [0, 43] {
        let (r, b) = kq(KeyInfoClass::Full, &key(Some("ABC")), len);
        assert_eq!(r, too_small(50));
        assert_eq!(b, pad(len));
    }
}

// ---- KEY_NAME_INFORMATION: NameLength@0 Name@4 (the full NT path) ----

#[test]
fn key_name_exact_overflow_and_too_small() {
    let path = r"\REGISTRY\A";
    let full = cat(&[&n(22), &w(path)]);
    let q = |len| {
        run(len, |b| {
            write_key_info(KeyInfoClass::Name, &key(None), path, b)
        })
    };
    let (r, b) = q(26);
    assert_eq!(r, ok(26));
    assert_eq!(b, full);
    // WRK CmQueryKey copies as much of the name as fits.
    let (r, b) = q(10);
    assert_eq!(r, overflow(26));
    assert_eq!(b, full[..10]);
    let (r, b) = q(4);
    assert_eq!(r, overflow(26));
    assert_eq!(b, full[..4]);
    for len in [0, 3] {
        let (r, b) = q(len);
        assert_eq!(r, too_small(26));
        assert_eq!(b, pad(len));
    }
}

// ---- KEY_CACHED_INFORMATION (sizeof 40): LastWriteTime@0 TitleIndex@8 SubKeys@12
//      MaxNameLen@16 Values@20 MaxValueNameLen@24 MaxValueDataLen@28 NameLength@32,
//      36..40 padding. No name is copied; NameLength is the key's own (leaf) name. ----

#[test]
fn key_cached_exact() {
    let (r, b) = kq(KeyInfoClass::Cached, &merged(), 40);
    assert_eq!(r, ok(40));
    let expected = cat(&[
        &LW_BYTES,
        &ZERO,
        &n(2),
        &n(24),
        &n(2),
        &n(10),
        &n(10),
        &n(6), // "Mod"
        &pad(4),
    ]);
    assert_eq!(b, expected);
    // A larger buffer still reports and writes only the structure.
    let (r, b) = kq(KeyInfoClass::Cached, &merged(), 64);
    assert_eq!(r, ok(40));
    assert_eq!(b[..40], expected);
    assert_eq!(b[40..], pad(24));
}

#[test]
fn key_cached_has_no_overflow_case() {
    // A fixed-size class: anything short of sizeof is too small, never a partial write.
    for len in [0, 36, 39] {
        let (r, b) = kq(KeyInfoClass::Cached, &merged(), len);
        assert_eq!(r, too_small(40));
        assert_eq!(b, pad(len));
    }
}

// ---- Fixed-size classes with nothing to report for an overlay key ----

#[test]
fn key_flags_virtualization_handle_tags() {
    for (class, size) in [
        (KeyInfoClass::Flags, 12),
        (KeyInfoClass::Virtualization, 4),
        (KeyInfoClass::HandleTags, 4),
    ] {
        let (r, b) = kq(class, &merged(), size);
        assert_eq!(r, ok(size as u32), "{class:?}");
        assert_eq!(b, vec![0; size], "{class:?}");
        for len in [0, size - 1] {
            let (r, b) = kq(class, &merged(), len);
            assert_eq!(r, too_small(size as u32), "{class:?}");
            assert_eq!(b, pad(len), "{class:?}");
        }
    }
}

// ---- NtEnumerateKey: a subkey by name with its own merged view ----

#[test]
fn subkey_basic_node_full() {
    let sub = key(Some("ABC"));
    let (r, b) = run(22, |b| {
        write_subkey_info(KeyInfoClass::Basic, "Mod", &sub, b)
    });
    assert_eq!(r, ok(22));
    assert_eq!(b, basic_mod());
    let (r, b) = run(38, |b| {
        write_subkey_info(KeyInfoClass::Node, "Mod", &sub, b)
    });
    assert_eq!(r, ok(36));
    assert_eq!(b, node_mod_abc());
    let (r, b) = run(50, |b| {
        write_subkey_info(KeyInfoClass::Full, "Mod", &sub, b)
    });
    assert_eq!(r, ok(50));
    assert_eq!(b, full_plain());
    // Short buffers behave as for NtQueryKey.
    let (r, b) = run(19, |b| {
        write_subkey_info(KeyInfoClass::Basic, "Mod", &sub, b)
    });
    assert_eq!(r, overflow(22));
    assert_eq!(b, basic_mod()[..19]);
    let (r, b) = run(15, |b| {
        write_subkey_info(KeyInfoClass::Basic, "Mod", &sub, b)
    });
    assert_eq!(r, too_small(22));
    assert_eq!(b, pad(15));
}

#[test]
fn subkey_other_classes_are_invalid() {
    for class in [
        KeyInfoClass::Name,
        KeyInfoClass::Cached,
        KeyInfoClass::Flags,
        KeyInfoClass::Virtualization,
        KeyInfoClass::HandleTags,
    ] {
        let (r, b) = run(64, |b| write_subkey_info(class, "Mod", &merged(), b));
        assert_eq!(r.status, STATUS_INVALID_PARAMETER, "{class:?}");
        assert_eq!(b, pad(64));
    }
}

// ---- KEY_VALUE_BASIC_INFORMATION: TitleIndex@0 Type@4 NameLength@8 Name@12 ----
