# Read into game buffers — bulk only

**Date:** 2026-07-15 (updated: remote WPM removed)

## Production data path

| Mode | Role |
|------|------|
| **Inline** | Small READs: data in ring slot payload |
| **Bulk** | Large READs: disk/zip → shared arena bank → client `copy_to` into `NtReadFile` buffer |

The shim never opens layer files. Director owns zip/disk I/O.

**Director `WriteProcessMemory` / `FLAG_READ_REMOTE` was prototyped and removed** — bulk arena is simpler and faster on disk (~3× in same-process benches).

## Phase 1 (kept)

`NtReadFile` hook passes the game buffer into `FuseClient::read_fragmented` (no intermediate `tmp` Vec). Bulk responses land with `SharedSeg::copy_to` straight into that buffer.

## Reproduce

```text
cargo run -p vfs-launch --bin vfs-fuse-bench --release
cargo run -p vfs-launch --bin vfs-fuse-bench --release -- --zip
```

See `docs/benchmarks/fuse-rpc-latest.md` for current numbers.
