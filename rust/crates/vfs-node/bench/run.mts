#!/usr/bin/env node

// `pnpm bench` — the durable performance gate for the Node binding.
//
// Three sections, in cost order: the JavaScript layer, a graph of Rust
// primitives, and the JavaScript provider bridge. Each prints a table and
// registers checks; the exit code is the product.
//
// **This is a gate, not a report.** A benchmark nobody re-runs stops being true,
// which is exactly how `docs/benchmarks/node-ffi-round-trip.md` ended up carrying
// a "historical — nothing reproduces this" banner. So the numbers here are held
// by assertions, tiered by how much a loaded machine can move them:
//
//   tier 1  deterministic counters — crossings, cache hits, export counts.
//           Asserted exactly. Load cannot move these, so they are the real gate.
//   tier 2  ratios between two things measured seconds apart in this same run.
//           Machine speed cancels — transient interference does not.
//   tier 3  absolute wall clock. Recorded always; asserted only with large
//           headroom, following provider.test.mts (500 µs ceiling, ~63 µs seen).
//
// Which tiers can fail the run is set by `BENCH_GATE_TIERS` (default: all).
// CI sets it to `1`, so on shared runners tiers 2 and 3 print as WARN rather than
// failing the job — see `gatingTiers()` in harness.mts for the incident that
// motivated it. Run this locally, where the default applies, to gate on timing.
//
// It is deliberately **not** part of `pnpm test`. That chain already builds,
// typechecks, runs the drift check, three injected-process examples and vitest;
// adding timing assertions to it would make an already long gate flakier. CI runs
// this as its own step.
//
// The A/B against the pre-migration JavaScript layer is `bench/ab-js-layer.mts`,
// which is a one-shot and not part of this gate — see its header.

import { assertReleaseAddon, drainSink, environment, heading, verdict } from './harness.mts';
import { run as jsLayer } from './cases/js-layer.mts';
import { run as graph } from './cases/graph.mts';
import { run as jsProvider } from './cases/js-provider.mts';

async function main(): Promise<number> {
  process.stdout.write('aethervfs — Node binding benchmarks\n');

  // Before anything: refuse to measure a debug or stale addon. `pnpm bench`
  // builds release first, but a concurrent `pnpm test` in the same tree builds
  // debug and overwrites it, which has already happened here once.
  assertReleaseAddon();
  process.stdout.write(`${environment()}\n`);

  await jsLayer();
  await graph();
  await jsProvider();

  heading('notes');
  process.stdout.write(
    '  Wall-clock numbers are contended. Tier 1 is the part that holds regardless;\n' +
      '  tiers 2 and 3 carry deliberate headroom. Do not tighten a tier-3 ceiling to\n' +
      '  just above what one run reported — that is how a gate becomes a flake.\n' +
      `  (sink ${drainSink()})\n`
  );

  return verdict();
}

main().then(
  (code) => {
    process.exitCode = code;
  },
  (err: unknown) => {
    console.error(err);
    process.exitCode = 1;
  }
);
