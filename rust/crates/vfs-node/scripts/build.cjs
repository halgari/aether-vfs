#!/usr/bin/env node
'use strict';

// Build everything the package needs and put it in the package directory.
//
// Four cargo invocations, and each one is a separate invocation for a reason:
//
//  1. `-p vfs-node`            the addon. A cdylib, so a `--tests`-filtered
//                              build would skip it entirely.
//  2. `-p vfs-shim-dll`        the injected shim. Same cdylib filter applies —
//                              this is the trap `rust/Cargo.toml` documents.
//  3. `--manifest-path crates/vfs-payload/Cargo.toml`
//                              the payload. Its own workspace, because it is
//                              `#![no_std]` with `panic = "abort"` while the
//                              main workspace is `panic = "unwind"` (spec §9),
//                              so `cargo build --workspace` never builds it.
//                              CARGO_TARGET_DIR is pinned so the artifact lands
//                              in the usual place.
//  4. `-p vfs-inject --bin vfs-probe`
//                              the stand-in executable the demo launches. Not
//                              part of the shipped package; it goes in
//                              `fixtures/`.
//
// Every copy **overwrites unconditionally**, and prints size and mtime. A stale
// DLL that silently survived a rebuild has produced wrong results in this
// project before; "skip if the destination exists" is how that happens.

const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');

const pkgDir = path.resolve(__dirname, '..');
const workspace = path.resolve(pkgDir, '..', '..'); // rust/
const release = process.argv.includes('--release');
const profile = release ? 'release' : 'debug';
const profileFlag = release ? ['--release'] : [];
const targetRoot = path.join(workspace, 'target');
const targetDir = path.join(targetRoot, profile);
const cargo = process.env.CARGO || 'cargo';

function run(args, extraEnv) {
  process.stdout.write(`> cargo ${args.join(' ')}\n`);
  const r = spawnSync(cargo, args, {
    cwd: workspace,
    stdio: 'inherit',
    env: extraEnv ? { ...process.env, ...extraEnv } : process.env,
  });
  if (r.error) throw r.error;
  if (r.status !== 0) {
    throw new Error(`cargo ${args.join(' ')} failed with status ${r.status}`);
  }
}

function install(from, to) {
  if (!fs.existsSync(from)) {
    throw new Error(`build produced no ${from}`);
  }
  fs.mkdirSync(path.dirname(to), { recursive: true });
  fs.copyFileSync(from, to);
  const st = fs.statSync(to);
  process.stdout.write(
    `  ${path.relative(pkgDir, to).padEnd(28)} ${String(st.size).padStart(9)} bytes  ` +
      `${st.mtime.toISOString()}\n`
  );
}

run(['build', '-p', 'vfs-node', ...profileFlag]);
run(['build', '-p', 'vfs-shim-dll', ...profileFlag]);
run(
  ['build', '--manifest-path', 'crates/vfs-payload/Cargo.toml', ...profileFlag],
  { CARGO_TARGET_DIR: targetRoot }
);
run(['build', '-p', 'vfs-inject', '--bin', 'vfs-probe', ...profileFlag]);

process.stdout.write(`\ninstalling ${profile} artifacts into ${pkgDir}\n`);

// The cdylib is named `aethervfs` in Cargo.toml precisely so this is a rename
// and not a guess at which of several DLLs is the addon.
install(path.join(targetDir, 'aethervfs.dll'), path.join(pkgDir, 'aethervfs.node'));
install(path.join(targetDir, 'vfs_shim_dll.dll'), path.join(pkgDir, 'vfs_shim_dll.dll'));
install(path.join(targetDir, 'vfs_payload.dll'), path.join(pkgDir, 'vfs_payload.dll'));
install(
  path.join(targetDir, 'vfs-probe.exe'),
  path.join(pkgDir, 'fixtures', 'vfs-probe.exe')
);

process.stdout.write('\nok\n');
