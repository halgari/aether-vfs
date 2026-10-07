use super::*;

#[test]
fn disabled_timer_records_nothing() {
    // VFS_SHIM_STATS_LOG is unset under test, so `enabled()` is false and
    // the guard must not read a clock or touch counters.
    let before = CALLS[Hook::Create as usize].load(Ordering::Relaxed);
    {
        let mut t = Timed::new(Hook::Create);
        t.mark_rooted();
    }
    assert_eq!(CALLS[Hook::Create as usize].load(Ordering::Relaxed), before);
}

#[test]
fn render_reports_nothing_when_no_calls() {
    let s = render(&snapshot());
    assert!(s.contains("hook stats"), "{s}");
    assert!(s.contains("TOTAL"), "{s}");
    // Never divide by zero on an idle process.
    assert!(!s.contains("NaN"), "{s}");
}

#[test]
fn busiest_path_is_reported_first() {
    // The point of counting repeats: a retry loop's path must outrank the
    // hundreds of one-shot opens a launch makes, whatever order they hashed
    // into the map.
    let s = format_paths(vec![
        ("\\??\\c:\\a.esm".into(), 3),
        ("\\??\\c:\\loop.bsa".into(), 9001),
        ("\\??\\c:\\b.esm".into(), 3),
    ]);
    let lines: Vec<&str> = s.lines().filter(|l| l.starts_with("  ")).collect();
    assert!(lines[0].contains("loop.bsa"), "{s}");
    assert!(lines[0].contains("9001"), "{s}");
    // Equal counts must tie-break by path, so two snapshots stay diffable.
    assert!(lines[1].contains("a.esm"), "{s}");
    assert!(lines[2].contains("b.esm"), "{s}");
    assert!(s.contains("3 distinct"), "{s}");
    assert!(s.contains("9007 opens"), "{s}");
}

#[test]
fn no_paths_renders_nothing() {
    assert_eq!(format_paths(Vec::new()), "");
}

#[test]
fn hook_names_cover_every_variant() {
    assert_eq!(NAMES.len(), N);
    // The last variant must index the last name, or a hook silently
    // reports under a neighbour's label.
    assert_eq!(Hook::Cpiw as usize, N - 1);
    assert_eq!(NAMES[Hook::UnmapView as usize], "NtUnmapViewOfSection");
    assert_eq!(NAMES[Hook::Cpiw as usize], "CreateProcessInternalW");
    assert_eq!(NAMES[Hook::FlushKey as usize], "NtFlushKey");
    assert_eq!(NAMES[Hook::NotifyChangeKey as usize], "NtNotifyChangeKey");
    assert_eq!(
        NAMES[Hook::QueryMultipleValueKey as usize],
        "NtQueryMultipleValueKey"
    );
    assert_eq!(NAMES[Hook::QObj as usize], "NtQueryObject");
    // Spot-check the middle of the table too: appending variants without
    // appending names in the same order is the failure this guards, and
    // only the *last* index is caught by the check above.
    assert_eq!(NAMES[Hook::SetInfo as usize], "NtSetInformationFile");
    assert_eq!(NAMES[Hook::Lock as usize], "NtLockFile");
    assert!(NAMES.iter().all(|n| !n.is_empty()));
}

/// The synthetic-lock section must carry its warning, not just its counts.
/// The counts alone would read as ordinary activity; the section exists to
/// say that each of those grants is a lock nobody actually holds, and a
/// reader who does not know that cannot act on the numbers.
#[test]
fn synthetic_lock_section_names_the_path_and_says_the_lock_is_not_real() {
    // The key is built exactly as `note_synthetic_lock` builds it, rather
    // than the counter being driven: it is a no-op under test
    // (`VFS_SHIM_STATS_LOG` is unset, the same convention
    // `outcome_counters_are_free_when_disabled` relies on), so going
    // through it would render an empty section and assert nothing.
    let mut snap = empty_snapshot();
    snap.synth_locks.insert(
        format!(
            "{:<16} {}",
            "lock-exclusive", r"\??\c:\root\skyrimprefs.ini"
        ),
        3,
    );
    let s = render_synth_locks(&snap);
    assert!(s.contains("skyrimprefs.ini"), "{s}");
    assert!(s.contains("3x"), "{s}");
    assert!(s.contains("lock-exclusive"), "{s}");
    assert!(s.contains("no lock is actually held"), "{s}");
}

#[test]
fn empty_synthetic_lock_section_renders_nothing() {
    assert_eq!(render_synth_locks(&empty_snapshot()), "");
}

/// The banner must say both things a reader needs: that this is a
/// point-in-time snapshot, and *which* point. Dropping either turns an
/// absent row back into the ambiguity the banner exists to remove.
#[test]
fn banner_marks_the_report_as_a_snapshot_and_dates_it() {
    let b = banner();
    assert!(b.starts_with("SNAPSHOT at t+"), "{b}");
    assert!(b.contains("no exit report exists"), "{b}");
    assert!(b.ends_with('\n'), "{b:?}");
}

#[test]
fn outcome_counters_are_free_when_disabled() {
    // VFS_SHIM_STATS_LOG is unset under test, so `enabled()` is false and
    // recording must not touch the counters at all.
    let before = outcome_count(OpenOutcome::Routed);
    note_open_outcome(OpenOutcome::Routed, "a.esp");
    assert_eq!(outcome_count(OpenOutcome::Routed), before);
}

#[test]
fn every_outcome_renders_with_a_distinct_label() {
    // A gate that removes one bypass class must be able to see that class
    // alone; identical or missing labels would defeat that.
    let mut labels: Vec<&str> = ALL_OUTCOMES.iter().map(|o| o.label()).collect();
    let n = labels.len();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(
        labels.len(),
        n,
        "outcome labels must be distinct: {labels:?}"
    );
}

#[test]
fn outcome_path_truncation_says_how_many_more() {
    // A truncated per-outcome path list silently presented as complete
    // would make a later gate measure against a count that is quietly
    // wrong, so the cut must say what it left out.
    let pairs: Vec<(String, u64)> = (0..OUTCOME_PATHS_SHOWN + 5)
        .map(|i| (format!("path{i}.esp"), 1))
        .collect();
    let s = format_outcome_paths(pairs);
    assert!(s.contains("... and 5 more"), "{s}");
}

#[test]
fn no_outcome_paths_renders_nothing() {
    assert_eq!(format_outcome_paths(Vec::new()), "");
}

/// The unrouted-open row must render inside the outcomes section, in the
/// same shape as an outcome row: `vfs-directord`'s `assert_reconciled`
/// parses that one section and needs both halves of the reconciliation
/// out of it. Its label must also not collide with any outcome's, or the
/// count would parse as a fall-through class instead.
#[test]
fn unrouted_director_opens_render_as_a_row_in_the_outcomes_section() {
    let mut snap = empty_snapshot();
    snap.outcome_counts[OpenOutcome::Routed as usize] = 9;
    snap.unrouted_director_opens = 3;
    let s = render_outcomes(&snap);
    assert!(s.starts_with("\nunder-root open outcomes:\n"), "{s}");
    assert!(
        s.contains(&format!("  {UNROUTED_OPEN_LABEL:<32} {:>8}\n", 3)),
        "{s}"
    );
    assert!(
        !ALL_OUTCOMES
            .iter()
            .any(|o| o.label() == UNROUTED_OPEN_LABEL),
        "the unrouted-open label collides with an outcome label"
    );
}

/// Zero is omitted, exactly like an outcome at zero — so a run with
/// neither drift source present renders the section it always did.
#[test]
fn no_unrouted_director_opens_renders_no_row() {
    let mut snap = empty_snapshot();
    snap.outcome_counts[OpenOutcome::Routed as usize] = 1;
    assert!(!render_outcomes(&snap).contains(UNROUTED_OPEN_LABEL));
}

#[test]
fn hook_panics_are_counted_even_though_instrumentation_is_disabled() {
    assert!(
        !enabled(),
        "test process must have stats off for this to mean anything"
    );
    // A name no other test uses, because the counters are process-wide and
    // the unit tests share one process.
    let name = "NtCountedWhileDisabled";
    let before_total = hook_panics_total();
    assert_eq!(hook_panic_count(name), 0);
    note_hook_panic(name);
    assert_eq!(hook_panic_count(name), 1);
    note_hook_panic(name);
    assert_eq!(hook_panic_count(name), 2);
    // Other tests may be panicking their own hooks concurrently, so the
    // total is only ever asserted as a lower bound.
    assert!(hook_panics_total() >= before_total + 2);
}

/// A caught panic must say what the number means rather than just
/// printing it — a bare count reads as ordinary activity.
#[test]
fn the_panic_section_names_each_faulting_hook_and_says_it_is_a_bug() {
    let mut snap = empty_snapshot();
    snap.hook_panics_total = 4;
    snap.hook_panics.insert("NtReadFile", 3);
    snap.hook_panics.insert("NtCreateFile", 1);
    let s = render_hook_panics(&snap);
    assert!(s.starts_with("CAUGHT PANICS: 4 "), "{s}");
    assert!(s.contains("is a bug"), "{s}");
    // The log is the only place the message and location survive; a count
    // with no pointer to it is a dead end.
    assert!(s.contains("VFS_SHIM_PANIC_LOG"), "{s}");
    let rows: Vec<&str> = s.lines().skip(1).collect();
    // Busiest first, so the hook that is faulting every call outranks the
    // one that faulted once.
    assert!(
        rows[0].contains("NtReadFile") && rows[0].contains('3'),
        "{s}"
    );
    assert!(rows[1].contains("NtCreateFile"), "{s}");
}

#[test]
fn a_clean_run_renders_no_panic_section() {
    assert_eq!(render_hook_panics(&empty_snapshot()), "");
}

/// Ordering, in the real report rather than in one section's own string.
/// A caught panic invalidates the reading of everything below it — that
/// hook returned a failure status without doing its job — so it cannot sit
/// under a `TOTAL` line and a thousand path rows where a reader reaches it
/// only after forming the wrong conclusion.
///
/// This works because [`note_hook_panic`] is ungated: the counter it feeds
/// is live in this test process even though `enabled()` is false, which is
/// the whole point of it being the one ungated counter here.
#[test]
fn a_caught_panic_leads_the_rendered_report() {
    note_hook_panic("NtOrderingProbe");
    let r = render_report();
    let panics = r
        .find("CAUGHT PANICS")
        .unwrap_or_else(|| panic!("no panic section:\n{r}"));
    let table = r
        .find("vfs-shim hook stats")
        .unwrap_or_else(|| panic!("no table:\n{r}"));
    assert!(
        panics < table,
        "the panic section rendered below the hook table:\n{r}"
    );
    // Below the banner, though: a reader has to know the report is a
    // snapshot before reading any number in it.
    assert!(r.starts_with("SNAPSHOT at"), "{r}");
}

/// A zeroed `Snapshot` for rendering tests, so one can be built without
/// the process-wide counters (inert under test) and without every test
/// listing all twenty-odd fields.
fn empty_snapshot() -> Snapshot {
    Snapshot {
        calls: [0; N],
        nanos: [0; N],
        rooted: [0; N],
        max_nanos: [0; N],
        slow: [0; N],
        async_opens: 0,
        sync_opens: 0,
        apc_reads: 0,
        event_reads: 0,
        bare_reads: 0,
        iocp_binds: 0,
        fills_started: 0,
        fills_completed: 0,
        fills_failed: 0,
        fill_bytes: 0,
        fill_nanos: 0,
        fill_max_nanos: 0,
        unrouted_director_opens: 0,
        name_queries: 0,
        name_queries_cached: 0,
        name_lookups: 0,
        reg_read_fallbacks: 0,
        reg_unresolved: 0,
        reg_close_lock_given_up: 0,
        reg: RegCounters::default(),
        setinfo_noop: HashMap::new(),
        delete_on_close_refused: 0,
        delete_on_close_refused_paths: HashMap::new(),
        synth_locks: HashMap::new(),
        passthrough: HashMap::new(),
        undecodable: HashMap::new(),
        trace: Vec::new(),
        stats: HashMap::new(),
        readdirs: Vec::new(),
        readdir_calls: 0,
        readdirs_dropped: 0,
        outcome_counts: [0; OUTCOME_N],
        outcome_paths: std::array::from_fn(|_| HashMap::new()),
        hook_panics_total: 0,
        hook_panics: HashMap::new(),
        child_inject_refused_total: 0,
        child_inject_refused: HashMap::new(),
        read_cache: None,
        read_cache_files: Vec::new(),
    }
}

#[test]
fn the_read_cache_section_reports_hits_misses_fetches_evictions_and_invalidations() {
    let snap = Snapshot {
        read_cache: Some(vfs_ipc::CacheStats {
            hits: 990,
            misses: 10,
            declined: 3,
            fetches: 10,
            bytes_fetched: 10 << 20,
            evictions: 2,
            invalidations: 1,
            blocks_invalidated: 4,
            cold: 0,
            fetch_failures: 7,
            fetches_abandoned: 1,
            pressure_misses: 4,
            resident_bytes: 8 << 20,
            max_bytes: 256 << 20,
            files: 5,
        }),
        read_cache_files: vec![
            vfs_ipc::FileReport {
                root: 0,
                path: "data/skyrim.esm".into(),
                diag: vfs_ipc::FileDiag {
                    reads: 838_643,
                    hits: 838_400,
                    misses: 243,
                    bytes_fetched: 240 << 20,
                    ..Default::default()
                },
                cold_now: false,
                poisoned: false,
            },
            vfs_ipc::FileReport {
                root: 0,
                path: "data/random.bsa".into(),
                diag: vfs_ipc::FileDiag {
                    reads: 900,
                    cold_guard: 2,
                    ..Default::default()
                },
                cold_now: true,
                poisoned: false,
            },
        ],
        ..empty_snapshot()
    };
    let s = render_read_cache(&snap);
    assert!(s.contains(READ_CACHE_LABEL), "{s}");
    for want in [
        "hits 990",
        "misses 10",
        "declined 3",
        "99.0% of cached reads were hits",
        "fetches 10 (10.0 MiB fetched)",
        "evictions 2",
        "invalidations 1 (4 blocks dropped)",
        "failed fetches 7",
        "given up on (past their deadline) 1",
        "resident 8.0 of 256 MiB",
        "misses on units the cap evicted 4",
        "busiest files (top 2)",
        "0:data/skyrim.esm",
        "guard x2 (now)",
    ] {
        assert!(s.contains(want), "missing {want:?} in:\n{s}");
    }
    // Off is said, not left as an absent section.
    assert!(render_read_cache(&empty_snapshot()).contains("OFF"));
    // On but idle: nothing.
    let idle = Snapshot {
        read_cache: Some(vfs_ipc::CacheStats::default()),
        ..empty_snapshot()
    };
    assert_eq!(render_read_cache(&idle), "");
}

#[test]
fn refused_deletes_on_close_render_with_their_paths_and_none_render_nothing() {
    let mut snap = empty_snapshot();
    assert_eq!(render_delete_on_close_refused(&snap), "");
    snap.delete_on_close_refused = 2;
    snap.delete_on_close_refused_paths
        .insert(r"\??\C:\root\a.esp (status -1)".to_string(), 2);
    let s = render_delete_on_close_refused(&snap);
    assert!(s.contains("refused") && s.contains(": 2"), "{s}");
    assert!(s.contains(r"2x  \??\C:\root\a.esp (status -1)"), "{s}");
}

#[test]
fn a_refused_child_is_counted_and_reported_by_reason() {
    let reason = "test-reason-unique";
    assert_eq!(child_inject_refused_count(reason), 0);
    note_child_inject_refused(reason);
    note_child_inject_refused(reason);
    assert_eq!(child_inject_refused_count(reason), 2);
    assert!(child_inject_refused_total() >= 2);

    assert_eq!(render_child_inject_refused(&empty_snapshot()), "");
    let mut snap = empty_snapshot();
    snap.child_inject_refused_total = 3;
    snap.child_inject_refused.insert("ready-timeout", 3);
    let s = render_child_inject_refused(&snap);
    assert!(s.contains("CHILD PROCESSES REFUSED: 3"), "{s}");
    assert!(s.contains("ready-timeout"), "{s}");
}
