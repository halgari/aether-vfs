# Pre-init Injection — Implementation Plan

> Production design: `docs/superpowers/specs/2026-07-14-preinit-injection-design.md`

**Goal:** Land reflective-map + RIP-redirect as the pre-init injection vehicle
and a zero-import early payload that virtualizes EXE static imports.

## Tasks

- [x] Shelf Spike B docs
- [x] Design doc (this + design spec)
- [x] `crates/vfs-payload` from validated shim_min
- [x] `vfs-inject` map + stub + SetThreadContext path
- [x] Static-import fixture + automated test (`tests/static_import.rs`)
- [x] Child inject: documented follow-up (LoadLibrary retained to avoid double-patch)
- [x] Verify vfs-inject suite green

## Build / test

```
cargo build -p vfs-payload
cargo test -p vfs-inject --test static_import
cargo test -p vfs-inject
```

## Port sources

Temp preinit (validated selftest):
`AppData/Local/Temp/claude/C--oss-vfs/8b1805fb-…/scratchpad/preinit/`
