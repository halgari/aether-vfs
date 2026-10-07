//! The one table of detours.
//!
//! Every NT export the shim hooks is a row here, and nothing else names it: the panic-contained
//! `extern "system"` wrappers (`hook::hook_entry_points!`), the trampoline slots, the install
//! loop (`hook::install_all_detours`) and the stats enum `hookstats::Hook` with its names and
//! count are all generated from these rows. A hook cannot be added to one list and forgotten in
//! another.
//!
//! `detour_table!(callback)` expands to `callback! { <rows> }`. Each consumer is a macro that
//! picks the columns it needs; the rest are matched with `$($rest:tt)*` and ignored.
//!
//! Row columns:
//!
//! | column | meaning |
//! |---|---|
//! | `export` | the export name (also the stats name and the panic-count key) |
//! | `stat` | `[Variant = id]` for `hookstats::Hook` (ids are the breadcrumb ids and are fixed), `[]` for a hook with no stats row |
//! | `tramp: NAME: Ty` | the trampoline slot (a `Tramp<Ty>` static) |
//! | `install` | `Required` (a failure to detour aborts the install), `Optional` (skipped and noted in `skipped_detours()`), `IfPresent` (an absent export is silently fine, a present one that fails is noted) or `BestEffort` (any failure is silent) |
//! | `group` | `File`, `Registry` (only with `VFS_REGISTRY`) or `Process` |
//! | `flags` | `Early`, `RawFallback`, `NeededByRegistry` (see the comments on the rows that use them) |
//! | `hook = body(args) -> ret` | the wrapper `extern "system" fn` and the body it calls |
//! | `on_panic` | what the wrapper returns when the body panics |
//!
//! The columns are in this order because a consumer that reads only the leading ones ends its
//! pattern in `$($rest:tt)*`, which cannot be followed by another literal.
//!
//! **Order is the install order.** It is not the order of the stats ids.

macro_rules! detour_table {
    ($cb:ident) => {
        $cb! {
            // --- file hooks ---
            // The four path/attr stubs the early payload owns. `install` (standalone) detours these
            // first; `install_late` wires their trampolines to the payload's and skips them.
            {
                export: "NtCreateFile",
                stat: [Create = 0],
                tramp: TRAMP_CREATE: NtCreateFileFn,
                install: Required, group: File, flags: [Early],
                hook: create_hook = create_hook_body(
                    file_handle: *mut HANDLE,
                    access: u32,
                    oa: *const ObjectAttributes,
                    iosb: *mut c_void,
                    alloc: *const i64,
                    attrs: u32,
                    share: u32,
                    disp: u32,
                    opts: u32,
                    ea: *const c_void,
                    ealen: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtQueryAttributesFile",
                stat: [QAttr = 2],
                tramp: TRAMP_QATTR: NtQueryAttributesFileFn,
                install: Required, group: File, flags: [Early],
                hook: qattr_hook = qattr_hook_body(
                    oa: *const ObjectAttributes,
                    info: *mut FileBasicInformation,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtQueryFullAttributesFile",
                stat: [QFull = 3],
                tramp: TRAMP_QFULL: NtQueryFullAttributesFileFn,
                install: Required, group: File, flags: [Early],
                hook: qfull_hook = qfull_hook_body(
                    oa: *const ObjectAttributes,
                    info: *mut FileNetworkOpenInformation,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtOpenFile",
                stat: [Open = 1],
                tramp: TRAMP_OPEN: NtOpenFileFn,
                install: Required, group: File, flags: [Early],
                hook: open_hook = open_hook_body(
                    file_handle: *mut HANDLE,
                    access: u32,
                    oa: *const ObjectAttributes,
                    iosb: *mut c_void,
                    share: u32,
                    opts: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            // Present since Win8, and optional for the same reason `NtQueryInformationByName` is: a host
            // may not export it. Measured against GE-Proton11-6 (Wine 11.0 Staging), whose ntdll omits
            // exactly these two of the functions installed here (this one and `NtQueryInformationByName`).
            //
            // Skipping it costs no coverage on such a host. A symbol absent from ntdll's export table is
            // equally unreachable for the game: it cannot be resolved dynamically and a static import
            // against it would fail module load. So the only reachable enumeration entry point there is
            // `NtQueryDirectoryFile`, installed as Required just below.
            //
            // This is NOT licence to let enumeration go unhooked where the export does exist: on Windows
            // both are present and both are hooked, and a caller on an unhooked enumeration path sees the
            // real, near-empty folder and leaves no trace anywhere. `skipped_detours()` reports what was
            // passed over so a host that expects total coverage can assert it rather than discover the hole
            // from a mod list that silently reads empty.
            {
                export: "NtQueryDirectoryFileEx",
                stat: [QDirEx = 7],
                tramp: TRAMP_QDIREX: NtQueryDirectoryFileExFn,
                install: Optional, group: File, flags: [],
                hook: qdirex_hook = qdirex_hook_body(
                    handle: HANDLE,
                    event: HANDLE,
                    apc: *const c_void,
                    apc_ctx: *const c_void,
                    iosb: *mut c_void,
                    info: *mut c_void,
                    length: u32,
                    class_raw: u32,
                    flags: u32,
                    file_name: *const UnicodeString,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            // Both enumeration exports must be covered: whichever one the caller picks decides whether it
            // sees the composed tree or the real, near-empty folder behind it.
            {
                export: "NtQueryDirectoryFile",
                stat: [QDir = 11],
                tramp: TRAMP_QDIR: NtQueryDirectoryFileFn,
                install: Required, group: File, flags: [],
                hook: qdir_hook = qdir_hook_body(
                    handle: HANDLE,
                    event: HANDLE,
                    apc: *const c_void,
                    apc_ctx: *const c_void,
                    iosb: *mut c_void,
                    info: *mut c_void,
                    length: u32,
                    class_raw: u32,
                    single: u8,
                    file_name: *const UnicodeString,
                    restart: u8,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            // The path-based delete. It is not optional and not a nicety: it takes no handle, so an
            // unhooked call resolves the `OBJECT_ATTRIBUTES` path itself and deletes the file that is
            // really there, under a managed root or not. See `delete_hook`.
            {
                export: "NtDeleteFile",
                stat: [DeleteFile = 18],
                tramp: TRAMP_DELETE: NtDeleteFileFn,
                install: Required, group: File, flags: [],
                hook: delete_hook = delete_hook_body(
                    oa: *const ObjectAttributes,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtClose",
                stat: [Close = 6],
                tramp: TRAMP_CLOSE: NtCloseFn,
                install: Required, group: File, flags: [],
                hook: close_hook = close_hook_body(
                    handle: HANDLE,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtQueryInformationFile",
                stat: [QueryInfo = 8],
                tramp: TRAMP_QIF: NtQueryInformationFileFn,
                install: Required, group: File, flags: [],
                hook: qif_hook = qif_hook_body(
                    handle: HANDLE,
                    iosb: *mut c_void,
                    info: *mut c_void,
                    length: u32,
                    class: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtSetInformationFile",
                stat: [SetInfo = 13],
                tramp: TRAMP_SETINFO: NtSetInformationFileFn,
                install: Required, group: File, flags: [],
                hook: setinfo_hook = setinfo_hook_body(
                    handle: HANDLE,
                    iosb: *mut c_void,
                    info: *mut c_void,
                    length: u32,
                    class: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtReadFile",
                stat: [Read = 4],
                tramp: TRAMP_READ: NtReadFileFn,
                install: Required, group: File, flags: [],
                hook: read_hook = read_hook_body(
                    handle: HANDLE,
                    event: HANDLE,
                    apc: *const c_void,
                    apc_ctx: *const c_void,
                    iosb: *mut c_void,
                    buffer: *mut c_void,
                    length: u32,
                    byte_offset: *const i64,
                    key: *const u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtWriteFile",
                stat: [Write = 5],
                tramp: TRAMP_WRITE: NtWriteFileFn,
                install: Required, group: File, flags: [],
                hook: write_hook = write_hook_body(
                    handle: HANDLE,
                    event: HANDLE,
                    apc: *const c_void,
                    apc_ctx: *const c_void,
                    iosb: *mut c_void,
                    buffer: *mut c_void,
                    length: u32,
                    byte_offset: *const i64,
                    key: *const u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtCreateSection",
                stat: [CreateSection = 9],
                tramp: TRAMP_CREATE_SECTION: NtCreateSectionFn,
                install: Required, group: File, flags: [],
                hook: create_section_hook = create_section_hook_body(
                    section_handle: *mut HANDLE,
                    access: u32,
                    oa: *const ObjectAttributes,
                    max_size: *mut i64,
                    page_prot: u32,
                    alloc_attrs: u32,
                    file_handle: HANDLE,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtMapViewOfSection",
                stat: [MapView = 10],
                tramp: TRAMP_MAP_VIEW: NtMapViewOfSectionFn,
                install: Required, group: File, flags: [],
                hook: map_view_hook = map_view_hook_body(
                    section: HANDLE,
                    process: HANDLE,
                    base_address: *mut *mut c_void,
                    zero_bits: usize,
                    commit_size: usize,
                    section_offset: *mut i64,
                    view_size: *mut usize,
                    inherit: u32,
                    alloc_type: u32,
                    protect: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtUnmapViewOfSection",
                stat: [UnmapView = 57],
                tramp: TRAMP_UNMAP_VIEW: NtUnmapViewOfSectionFn,
                install: Required, group: File, flags: [],
                hook: unmap_view_hook = unmap_view_hook_body(
                    process: HANDLE,
                    base: *mut c_void,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtQueryVolumeInformationFile",
                stat: [QVol = 14],
                tramp: TRAMP_QVOL: NtQueryVolumeInformationFileFn,
                install: Required, group: File, flags: [],
                hook: qvol_hook = qvol_hook_body(
                    handle: HANDLE,
                    iosb: *mut c_void,
                    info: *mut c_void,
                    length: u32,
                    class: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            // The lock/flush trio. Without these a synthetic handle is not merely missing a feature: the
            // *next* call after a successful open fails, and the caller abandons the file entirely. See
            // `lock_hook`.
            {
                export: "NtLockFile",
                stat: [Lock = 15],
                tramp: TRAMP_LOCK: NtLockFileFn,
                install: Required, group: File, flags: [],
                hook: lock_hook = lock_hook_body(
                    handle: HANDLE,
                    event: HANDLE,
                    apc: *const c_void,
                    apc_ctx: *const c_void,
                    iosb: *mut c_void,
                    byte_offset: *const i64,
                    length: *const i64,
                    key: u32,
                    fail_immediately: u8,
                    exclusive: u8,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtUnlockFile",
                stat: [Unlock = 16],
                tramp: TRAMP_UNLOCK: NtUnlockFileFn,
                install: Required, group: File, flags: [],
                hook: unlock_hook = unlock_hook_body(
                    handle: HANDLE,
                    iosb: *mut c_void,
                    byte_offset: *const i64,
                    length: *const i64,
                    key: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtFlushBuffersFile",
                stat: [FlushBuffers = 17],
                tramp: TRAMP_FLUSH: NtFlushBuffersFileFn,
                install: Required, group: File, flags: [],
                hook: flush_hook = flush_hook_body(
                    handle: HANDLE,
                    iosb: *mut c_void,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            // Present since Win10 1709. Optional so an older host still installs.
            {
                export: "NtQueryInformationByName",
                stat: [QByName = 12],
                tramp: TRAMP_QIBN: NtQueryInformationByNameFn,
                install: Optional, group: File, flags: [],
                hook: qibn_hook = qibn_hook_body(
                    oa: *const ObjectAttributes,
                    iosb: *mut c_void,
                    info: *mut c_void,
                    length: u32,
                    class_raw: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            // The other half of handle identity, beside `NtQueryInformationFile`'s class-48 spoof:
            // `GetFinalPathNameByHandleW` reaches `NtQueryInformationFile` on Windows and `NtQueryObject`
            // here on Wine, so leaving this one unhooked leaks the backing path on whichever host takes the
            // other route. Optional in the same style as the two above only because a host might not
            // export it: both hosts measured here do, so `skipped_detours()` stays empty and
            // `tests/diagnostics/hook_coverage.rs` asserts it.
            // `NeededByRegistry`: the registry overlay needs it (names and types of synthetic keys, the
            // access of pre-hook handles), so its absence is recorded in the registry's `missing` set.
            {
                export: "NtQueryObject",
                stat: [QObj = 19],
                tramp: TRAMP_QOBJ: NtQueryObjectFn,
                install: Optional, group: File, flags: [NeededByRegistry],
                hook: qobj_hook = qobj_hook_body(
                    handle: HANDLE,
                    class: u32,
                    info: *mut c_void,
                    length: u32,
                    ret_len: *mut u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            // --- registry hooks (group Registry): installed only when `VFS_REGISTRY` is set ---
            // Registry virtualisation is all or nothing (`regclient::enabled`): every row below that is not
            // installed lands in `missing`, and the outcome is recorded once, after the last row.
            // `Optional` rows are noted in `skipped_detours()` when absent; `IfPresent` rows (from
            // `NtNotifyChangeKey` on) are skipped silently when ntdll has no such export (Wine has no
            // `NtCompressKey`, `NtLockRegistryKey`, `NtSaveKeyEx`, ...), since an export that does not exist
            // cannot be called.
            // `RawFallback`: this slot is also read by `regkeys` for key names, so it holds ntdll's own
            // export whenever the detour is not installed.
            {
                export: "NtQueryKey",
                stat: [QueryKey = 24],
                tramp: TRAMP_QUERY_KEY: NtQueryKeyFn,
                install: Optional, group: Registry, flags: [RawFallback],
                hook: query_key_hook = query_key_hook_body(
                    key: HANDLE,
                    class: u32,
                    info: *mut c_void,
                    length: u32,
                    ret_len: *mut u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtEnumerateKey",
                stat: [EnumerateKey = 25],
                tramp: TRAMP_ENUM_KEY: NtEnumerateKeyFn,
                install: Optional, group: Registry, flags: [],
                hook: enum_key_hook = enum_key_hook_body(
                    key: HANDLE,
                    index: u32,
                    class: u32,
                    info: *mut c_void,
                    length: u32,
                    ret_len: *mut u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtQueryValueKey",
                stat: [QueryValueKey = 26],
                tramp: TRAMP_QUERY_VALUE: NtQueryValueKeyFn,
                install: Optional, group: Registry, flags: [],
                hook: query_value_hook = query_value_hook_body(
                    key: HANDLE,
                    name: *const UnicodeString,
                    class: u32,
                    info: *mut c_void,
                    length: u32,
                    ret_len: *mut u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtEnumerateValueKey",
                stat: [EnumerateValueKey = 27],
                tramp: TRAMP_ENUM_VALUE: NtEnumerateValueKeyFn,
                install: Optional, group: Registry, flags: [],
                hook: enum_value_hook = enum_value_hook_body(
                    key: HANDLE,
                    index: u32,
                    class: u32,
                    info: *mut c_void,
                    length: u32,
                    ret_len: *mut u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtQueryMultipleValueKey",
                stat: [QueryMultipleValueKey = 28],
                tramp: TRAMP_QUERY_MULTIPLE: NtQueryMultipleValueKeyFn,
                install: Optional, group: Registry, flags: [],
                hook: query_multiple_hook = query_multiple_hook_body(
                    key: HANDLE,
                    entries: *mut c_void,
                    count: u32,
                    buffer: *mut c_void,
                    buffer_len: *mut u32,
                    required: *mut u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtSetValueKey",
                stat: [SetValueKey = 29],
                tramp: TRAMP_SET_VALUE: NtSetValueKeyFn,
                install: Optional, group: Registry, flags: [],
                hook: set_value_key_hook = set_value_key_hook_body(
                    key: HANDLE,
                    name: *const UnicodeString,
                    title_index: u32,
                    ty: u32,
                    data: *const c_void,
                    size: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtDeleteValueKey",
                stat: [DeleteValueKey = 30],
                tramp: TRAMP_DELETE_VALUE: NtDeleteValueKeyFn,
                install: Optional, group: Registry, flags: [],
                hook: delete_value_key_hook = delete_value_key_hook_body(
                    key: HANDLE,
                    name: *const UnicodeString,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtDeleteKey",
                stat: [DeleteKey = 31],
                tramp: TRAMP_DELETE_KEY: NtDeleteKeyFn,
                install: Optional, group: Registry, flags: [],
                hook: delete_key_hook = delete_key_hook_body(
                    key: HANDLE,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtRenameKey",
                stat: [RenameKey = 32],
                tramp: TRAMP_RENAME_KEY: NtRenameKeyFn,
                install: Optional, group: Registry, flags: [],
                hook: rename_key_hook = rename_key_hook_body(
                    key: HANDLE,
                    new_name: *const UnicodeString,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtSetInformationKey",
                stat: [SetInformationKey = 33],
                tramp: TRAMP_SET_INFO_KEY: NtSetInformationKeyFn,
                install: Optional, group: Registry, flags: [],
                hook: set_info_key_hook = set_info_key_hook_body(
                    key: HANDLE,
                    class: u32,
                    info: *const c_void,
                    length: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtFlushKey",
                stat: [FlushKey = 34],
                tramp: TRAMP_FLUSH_KEY: NtFlushKeyFn,
                install: Optional, group: Registry, flags: [],
                hook: flush_key_hook = flush_key_hook_body(
                    key: HANDLE,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtOpenKeyEx",
                stat: [OpenKeyEx = 21],
                tramp: TRAMP_OPEN_KEY_EX: NtOpenKeyExFn,
                install: Optional, group: Registry, flags: [],
                hook: open_key_ex_hook = open_key_ex_hook_body(
                    key: *mut HANDLE,
                    access: u32,
                    oa: *const ObjectAttributes,
                    options: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtOpenKey",
                stat: [OpenKey = 20],
                tramp: TRAMP_OPEN_KEY: NtOpenKeyFn,
                install: Optional, group: Registry, flags: [],
                hook: open_key_hook = open_key_hook_body(
                    key: *mut HANDLE,
                    access: u32,
                    oa: *const ObjectAttributes,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtCreateKey",
                stat: [CreateKey = 22],
                tramp: TRAMP_CREATE_KEY: NtCreateKeyFn,
                install: Optional, group: Registry, flags: [],
                hook: create_key_hook = create_key_hook_body(
                    key: *mut HANDLE,
                    access: u32,
                    oa: *const ObjectAttributes,
                    title_index: u32,
                    class: *const UnicodeString,
                    options: u32,
                    disposition: *mut u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtDuplicateObject",
                stat: [DuplicateObject = 23],
                tramp: TRAMP_DUP: NtDuplicateObjectFn,
                install: Optional, group: Registry, flags: [],
                hook: dup_hook = dup_hook_body(
                    src_process: HANDLE,
                    src: HANDLE,
                    dst_process: HANDLE,
                    dst: *mut HANDLE,
                    access: u32,
                    attributes: u32,
                    options: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            // Notifications, security, handle flags and the out-of-scope calls (spec 3.4, 3.6). Each export
            // this ntdll has must be detoured like the rest; one it lacks is simply skipped.
            {
                export: "NtNotifyChangeKey",
                stat: [NotifyChangeKey = 35],
                tramp: TRAMP_NOTIFY_KEY: NtNotifyChangeKeyFn,
                install: IfPresent, group: Registry, flags: [],
                hook: notify_key_hook = notify_key_hook_body(
                    key: HANDLE,
                    event: HANDLE,
                    apc: *const c_void,
                    apc_ctx: *const c_void,
                    iosb: *mut c_void,
                    filter: u32,
                    subtree: u8,
                    buffer: *mut c_void,
                    buffer_len: u32,
                    asynchronous: u8,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtNotifyChangeMultipleKeys",
                stat: [NotifyChangeMultipleKeys = 36],
                tramp: TRAMP_NOTIFY_MULTIPLE: NtNotifyChangeMultipleKeysFn,
                install: IfPresent, group: Registry, flags: [],
                hook: notify_multiple_hook = notify_multiple_hook_body(
                    key: HANDLE,
                    count: u32,
                    subordinates: *const ObjectAttributes,
                    event: HANDLE,
                    apc: *const c_void,
                    apc_ctx: *const c_void,
                    iosb: *mut c_void,
                    filter: u32,
                    subtree: u8,
                    buffer: *mut c_void,
                    buffer_len: u32,
                    asynchronous: u8,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtQuerySecurityObject",
                stat: [QuerySecurityObject = 37],
                tramp: TRAMP_QUERY_SECURITY: NtQuerySecurityObjectFn,
                install: IfPresent, group: Registry, flags: [],
                hook: query_security_hook = query_security_hook_body(
                    handle: HANDLE,
                    info: u32,
                    sd: *mut c_void,
                    length: u32,
                    needed: *mut u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtSetSecurityObject",
                stat: [SetSecurityObject = 38],
                tramp: TRAMP_SET_SECURITY: NtSetSecurityObjectFn,
                install: IfPresent, group: Registry, flags: [],
                hook: set_security_hook = set_security_hook_body(
                    handle: HANDLE,
                    info: u32,
                    sd: *const c_void,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtSetInformationObject",
                stat: [SetInformationObject = 39],
                tramp: TRAMP_SET_INFO_OBJECT: NtSetInformationObjectFn,
                install: IfPresent, group: Registry, flags: [],
                hook: set_info_object_hook = set_info_object_hook_body(
                    handle: HANDLE,
                    class: u32,
                    info: *const c_void,
                    length: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtCreateKeyTransacted",
                stat: [CreateKeyTransacted = 40],
                tramp: TRAMP_CREATE_KEY_TX: NtCreateKeyTransactedFn,
                install: IfPresent, group: Registry, flags: [],
                hook: create_key_tx_hook = create_key_tx_hook_body(
                    key: *mut HANDLE,
                    access: u32,
                    oa: *const ObjectAttributes,
                    title_index: u32,
                    class: *const UnicodeString,
                    options: u32,
                    transaction: HANDLE,
                    disposition: *mut u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtOpenKeyTransacted",
                stat: [OpenKeyTransacted = 41],
                tramp: TRAMP_OPEN_KEY_TX: NtOpenKeyTransactedFn,
                install: IfPresent, group: Registry, flags: [],
                hook: open_key_tx_hook = open_key_tx_hook_body(
                    key: *mut HANDLE,
                    access: u32,
                    oa: *const ObjectAttributes,
                    transaction: HANDLE,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtOpenKeyTransactedEx",
                stat: [OpenKeyTransactedEx = 42],
                tramp: TRAMP_OPEN_KEY_TX_EX: NtOpenKeyTransactedExFn,
                install: IfPresent, group: Registry, flags: [],
                hook: open_key_tx_ex_hook = open_key_tx_ex_hook_body(
                    key: *mut HANDLE,
                    access: u32,
                    oa: *const ObjectAttributes,
                    options: u32,
                    transaction: HANDLE,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtLoadKey",
                stat: [LoadKey = 43],
                tramp: TRAMP_LOAD_KEY: NtLoadKeyFn,
                install: IfPresent, group: Registry, flags: [],
                hook: load_key_hook = load_key_hook_body(
                    target: *const ObjectAttributes,
                    source: *const ObjectAttributes,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtLoadKey2",
                stat: [LoadKey2 = 44],
                tramp: TRAMP_LOAD_KEY2: NtLoadKey2Fn,
                install: IfPresent, group: Registry, flags: [],
                hook: load_key2_hook = load_key2_hook_body(
                    target: *const ObjectAttributes,
                    source: *const ObjectAttributes,
                    flags: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtLoadKeyEx",
                stat: [LoadKeyEx = 45],
                tramp: TRAMP_LOAD_KEY_EX: NtLoadKey8Fn,
                install: IfPresent, group: Registry, flags: [],
                hook: load_key_ex_hook = load_key_ex_hook_body(
                    target: *const ObjectAttributes,
                    source: *const ObjectAttributes,
                    flags: u32,
                    a4: usize,
                    a5: usize,
                    a6: usize,
                    a7: usize,
                    a8: usize,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtLoadKey3",
                stat: [LoadKey3 = 46],
                tramp: TRAMP_LOAD_KEY3: NtLoadKey8Fn,
                install: IfPresent, group: Registry, flags: [],
                hook: load_key3_hook = load_key3_hook_body(
                    target: *const ObjectAttributes,
                    source: *const ObjectAttributes,
                    flags: u32,
                    a4: usize,
                    a5: usize,
                    a6: usize,
                    a7: usize,
                    a8: usize,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtUnloadKey",
                stat: [UnloadKey = 47],
                tramp: TRAMP_UNLOAD_KEY: NtUnloadKeyFn,
                install: IfPresent, group: Registry, flags: [],
                hook: unload_key_hook = unload_key_hook_body(
                    target: *const ObjectAttributes,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtUnloadKey2",
                stat: [UnloadKey2 = 48],
                tramp: TRAMP_UNLOAD_KEY2: NtUnloadKey2Fn,
                install: IfPresent, group: Registry, flags: [],
                hook: unload_key2_hook = unload_key2_hook_body(
                    target: *const ObjectAttributes,
                    a2: usize,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtUnloadKeyEx",
                stat: [UnloadKeyEx = 49],
                tramp: TRAMP_UNLOAD_KEY_EX: NtUnloadKey2Fn,
                install: IfPresent, group: Registry, flags: [],
                hook: unload_key_ex_hook = unload_key_ex_hook_body(
                    target: *const ObjectAttributes,
                    a2: usize,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtSaveKey",
                stat: [SaveKey = 50],
                tramp: TRAMP_SAVE_KEY: NtSaveKeyFn,
                install: IfPresent, group: Registry, flags: [],
                hook: save_key_hook = save_key_hook_body(
                    key: HANDLE,
                    file: HANDLE,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtSaveKeyEx",
                stat: [SaveKeyEx = 51],
                tramp: TRAMP_SAVE_KEY_EX: NtSaveKeyExFn,
                install: IfPresent, group: Registry, flags: [],
                hook: save_key_ex_hook = save_key_ex_hook_body(
                    key: HANDLE,
                    file: HANDLE,
                    format: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtSaveMergedKeys",
                stat: [SaveMergedKeys = 52],
                tramp: TRAMP_SAVE_MERGED: NtSaveMergedKeysFn,
                install: IfPresent, group: Registry, flags: [],
                hook: save_merged_hook = save_merged_hook_body(
                    high: HANDLE,
                    low: HANDLE,
                    file: HANDLE,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtReplaceKey",
                stat: [ReplaceKey = 53],
                tramp: TRAMP_REPLACE_KEY: NtReplaceKeyFn,
                install: IfPresent, group: Registry, flags: [],
                hook: replace_key_hook = replace_key_hook_body(
                    new_file: *const ObjectAttributes,
                    key: HANDLE,
                    old_file: *const ObjectAttributes,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtRestoreKey",
                stat: [RestoreKey = 54],
                tramp: TRAMP_RESTORE_KEY: NtRestoreKeyFn,
                install: IfPresent, group: Registry, flags: [],
                hook: restore_key_hook = restore_key_hook_body(
                    key: HANDLE,
                    file: HANDLE,
                    flags: u32,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtCompressKey",
                stat: [CompressKey = 55],
                tramp: TRAMP_COMPRESS_KEY: NtKeyOnlyFn,
                install: IfPresent, group: Registry, flags: [],
                hook: compress_key_hook = compress_key_hook_body(
                    key: HANDLE,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            {
                export: "NtLockRegistryKey",
                stat: [LockRegistryKey = 56],
                tramp: TRAMP_LOCK_REGISTRY_KEY: NtKeyOnlyFn,
                install: IfPresent, group: Registry, flags: [],
                hook: lock_registry_key_hook = lock_registry_key_hook_body(
                    key: HANDLE,
                ) -> NTSTATUS,
                on_panic: STATUS_HOOK_PANICKED,
            }
            // --- process creation (group Process) ---
            // `kernelbase!CreateProcessInternalW` (else kernel32), the funnel under all CreateProcess*.
            // Installed best-effort after the registry rows, and only when the shim's own DLL path is known.
            {
                export: "CreateProcessInternalW",
                stat: [Cpiw = 58],
                tramp: TRAMP_CPIW: CreateProcessInternalWFn,
                install: BestEffort, group: Process, flags: [],
                /// The one entry point here that is **not** an ntdll `NTSTATUS` call, and
                /// the one place a uniform `STATUS_UNSUCCESSFUL` would be actively
                /// dangerous. `CreateProcessInternalW` returns a Win32 `BOOL`, in which
                /// `STATUS_UNSUCCESSFUL`'s bit pattern is non-zero and therefore reads as
                /// **success** — the caller would then go on to use a `PROCESS_INFORMATION`
                /// nothing ever filled in, and close or wait on two garbage handles. `FALSE`
                /// is the failure value in this ABI, and `SetLastError` is part of the
                /// contract: a `BOOL`-returning Win32 function that fails without setting it
                /// leaves the caller reporting whatever error some unrelated earlier call
                /// happened to leave behind.
                hook: cpiw_hook = cpiw_hook_body(
                    token: HANDLE,
                    app: *const u16,
                    cmd: *mut u16,
                    proc_attr: *const c_void,
                    thread_attr: *const c_void,
                    inherit: i32,
                    flags: u32,
                    env: *const c_void,
                    cur_dir: *const u16,
                    si: *const STARTUPINFOW,
                    pi: *mut PROCESS_INFORMATION,
                    ptok: *mut HANDLE,
                ) -> i32,
                on_panic: {
                    // SAFETY: plain TLS write in the current process; no pointers involved.
                    unsafe { windows_sys::Win32::Foundation::SetLastError(ERROR_INTERNAL_ERROR) };
                    0
                },
            }

        }
    };
}
