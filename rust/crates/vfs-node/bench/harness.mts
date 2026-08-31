// The measurement core for `pnpm bench`.
//
// **This file is run, not compiled** — `.mts` for the same reason
// `scripts/build.mts` is: node's type stripping erases annotations but does not
// rewrite module syntax, and an `.mts` is ESM, so `import`/`export` work
// natively. A `.cts` here would have to use `module.exports` — no fixture does
// that any more, now that the package is ESM-only. `benchRequire` below still
// goes through `createRequire`, but only for the one case that is really
// CommonJS: `ab-js-layer.mts`'s A/B comparison against the historical
// `index.cjs`.
//
// ## What this measures, and the four ways a benchmark lies
//
// Everything here is wall clock, so it is contended, and the failure modes are
// addressed rather than hoped away:
//
//  1. **Dead-code elimination.** V8 will delete a loop whose result nothing
//     reads, and the first thing to vanish would be the cheapest case here —
//     `vfs.disk` as a property access. Every case feeds `sink()`, and
//     `drainSink()` is read at the end so the optimiser cannot prove it dead.
//  2. **A single sample is noise.** Each case is calibrated to a target sample
//     duration, then run for several samples. Both **median** and **minimum** are
//     reported: on a shared machine the minimum is the better estimator of the
//     real cost, because interference only ever adds time. Ceilings are asserted
//     against the median, which is the conservative direction.
//  3. **Measuring the wrong binary.** `scripts/build.mts` copies
//     `target/{debug,release}/aethervfs.dll` over `aethervfs.node`, and the
//     default `pnpm build` is a **debug** build. Absolute numbers off a debug
//     cdylib are not worth recording, and a stale copy is worse than a slow one:
//     it reports a number for code that is not running. `assertReleaseAddon()`
//     refuses to run against anything that is not byte-identical to the current
//     `target/release` artifact. This is not theoretical — it fired the first
//     time it ran, on an addon a concurrent debug build had overwritten three
//     seconds after the release build installed it.
//  4. **A busy machine.** Nothing here can detect that, so `run.mts` prints the
//     load-bearing caveat and the tiering exists precisely so that a contended
//     run fails only tier 3, which is the tier with the most headroom.

import { createRequire } from 'node:module';
import * as crypto from 'node:crypto';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);

export const BENCH_DIR = path.dirname(fileURLToPath(import.meta.url));
export const PKG_DIR = path.resolve(BENCH_DIR, '..');
export const REPO_RUST_DIR = path.resolve(PKG_DIR, '..', '..');

// ---------------------------------------------------------------------------
// The sink. See failure mode 1.
// ---------------------------------------------------------------------------

let sinkAcc = 0;

/** Feed every benchmarked value through this so V8 cannot delete the loop. */
export function sink(v: unknown): void {
  if (typeof v === 'number') sinkAcc += v;
  else if (typeof v === 'string') sinkAcc += v.length;
  else if (typeof v === 'boolean') sinkAcc += v ? 1 : 0;
  else if (v && typeof v === 'object') sinkAcc += 1;
}

/** Read once at the end, so the accumulation is observable. */
export function drainSink(): number {
  const v = sinkAcc;
  sinkAcc = 0;
  return v;
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

export interface Sample {
  name: string;
  /** Median of the per-sample means. Ceilings are asserted against this. */
  nsPerOp: number;
  /** Best observed. Interference only adds time, so this is the cleanest read. */
  minNsPerOp: number;
  opsPerSec: number;
  iterations: number;
  samples: number;
}

export interface MeasureOptions {
  /** Fixed iteration count per sample. Omit to calibrate automatically. */
  iterations?: number;
  samples?: number;
  /** Calibration target for one sample, in milliseconds. */
  targetMs?: number;
  warmup?: number;
}

const results: Sample[] = [];

function calibrate(fn: () => void, targetMs: number): number {
  let k = 1;
  for (let guard = 0; guard < 40; guard += 1) {
    const t0 = process.hrtime.bigint();
    for (let i = 0; i < k; i += 1) fn();
    const ms = Number(process.hrtime.bigint() - t0) / 1e6;
    if (ms >= targetMs) return k;
    // Grow toward the target, but never by more than 8x in one step, so a case
    // whose first iterations are dominated by lazy compilation cannot explode.
    const grow = Math.min(8, Math.max(2, targetMs / Math.max(ms, 0.0005)));
    k = Math.ceil(k * grow);
    if (k > 5e8) return k;
  }
  return k;
}

/** Measure a synchronous operation. */
export function measure(name: string, fn: () => void, opts: MeasureOptions = {}): Sample {
  const samples = opts.samples ?? 7;
  const warmup = opts.warmup ?? 50;
  for (let i = 0; i < warmup; i += 1) fn();
  const iterations = opts.iterations ?? calibrate(fn, opts.targetMs ?? 25);

  const per: number[] = [];
  for (let s = 0; s < samples; s += 1) {
    const t0 = process.hrtime.bigint();
    for (let i = 0; i < iterations; i += 1) fn();
    per.push(Number(process.hrtime.bigint() - t0) / iterations);
  }
  per.sort((a, b) => a - b);
  const row: Sample = {
    name,
    nsPerOp: per[Math.floor(per.length / 2)]!,
    minNsPerOp: per[0]!,
    opsPerSec: 1e9 / per[Math.floor(per.length / 2)]!,
    iterations,
    samples,
  };
  results.push(row);
  return row;
}

/** Measure an asynchronous operation. Same shape, awaited per iteration. */
export async function measureAsync(
  name: string,
  fn: () => Promise<unknown>,
  opts: MeasureOptions = {}
): Promise<Sample> {
  const samples = opts.samples ?? 5;
  const warmup = opts.warmup ?? 5;
  const iterations = opts.iterations ?? 50;
  for (let i = 0; i < warmup; i += 1) sink(await fn());

  const per: number[] = [];
  for (let s = 0; s < samples; s += 1) {
    const t0 = process.hrtime.bigint();
    for (let i = 0; i < iterations; i += 1) sink(await fn());
    per.push(Number(process.hrtime.bigint() - t0) / iterations);
  }
  per.sort((a, b) => a - b);
  const row: Sample = {
    name,
    nsPerOp: per[Math.floor(per.length / 2)]!,
    minNsPerOp: per[0]!,
    opsPerSec: 1e9 / per[Math.floor(per.length / 2)]!,
    iterations,
    samples,
  };
  results.push(row);
  return row;
}

// ---------------------------------------------------------------------------
// The three assertion tiers. Why they are separate — and which of them are
// allowed to fail a run — is `gatingTiers()` below; the numbers they hold are
// documented in rust/docs/benchmarks/node-binding-surface.md. (This pointed at
// `bench/README.md`, which does not exist in this tree.)
// ---------------------------------------------------------------------------

export interface Check {
  tier: 1 | 2 | 3;
  label: string;
  ok: boolean;
  detail: string;
}

const checks: Check[] = [];

function record(tier: 1 | 2 | 3, label: string, ok: boolean, detail: string): void {
  checks.push({ tier, label, ok, detail });
}

/**
 * Which tiers may fail the run, as opposed to merely reporting.
 *
 * `BENCH_GATE_TIERS` is a comma-separated list ("1", "1,3", "all"). Default is
 * all three, which is what a developer running `pnpm bench` on a quiet machine
 * wants. CI sets `1`.
 *
 * ## Why CI does not gate on timing
 *
 * The tiering was built on the theory that a ratio (tier 2) is load-immune
 * because machine speed cancels between two measurements taken seconds apart.
 * That holds for *steady-state* speed and not for transient interference: a
 * scheduling storm that lands on one of the two measurements and not the other
 * moves the ratio, and on a shared 4-vCPU runner that is normal. The theory was
 * contradicted in practice — "a JS provider is not slower than a real Rust
 * provider" reported 2.54x against a 2x ceiling on GitHub Actions while passing
 * locally, and the neighbouring tier-2 check sat at 1.38x against 1.5x, i.e.
 * 8% from red. Two checks, both hovering at their ceilings, on hardware whose
 * noise the project does not control.
 *
 * A gate that goes red for reasons unrelated to the change under test does not
 * report a regression; it teaches people that red means nothing. So CI keeps
 * running every tier and keeps **printing** every number — the step is still
 * there, and the log is still the evidence — but only tier 1, the deterministic
 * counters, decides the exit code. Crossing counts and cache hits are the checks
 * that actually encode the invariants worth defending (a readFile is exactly
 * open + readAt + close; a warm re-read never misses), and no amount of load can
 * move an integer.
 *
 * Timing regressions are therefore caught by running `pnpm bench` on a machine
 * whose noise floor is known, which is the only place the numbers meant anything
 * in the first place.
 */
function gatingTiers(): ReadonlySet<number> {
  const raw = process.env.BENCH_GATE_TIERS?.trim();
  if (!raw || raw === 'all') return new Set([1, 2, 3]);
  const tiers = raw
    .split(',')
    .map((s) => Number(s.trim()))
    .filter((n) => n === 1 || n === 2 || n === 3);
  if (tiers.length === 0) {
    throw new Error(
      `bench: BENCH_GATE_TIERS='${raw}' names no valid tier. Use a comma-separated ` +
        "list of 1, 2, 3, or 'all'."
    );
  }
  return new Set(tiers);
}

/**
 * Tier 1 — a deterministic counter: bridge crossings, cache hits, export counts.
 * These do not vary with machine load, so they are asserted **exactly**, and this
 * is the only tier safe to tighten.
 */
export function assertExact(label: string, actual: number, expected: number, why: string): void {
  record(1, label, actual === expected, `${actual} (expected exactly ${expected}) — ${why}`);
}

/** Tier 1, for a counter with a floor rather than an exact value. */
export function assertAtLeast(label: string, actual: number, floor: number, why: string): void {
  record(1, label, actual >= floor, `${actual} (expected >= ${floor}) — ${why}`);
}

/**
 * Tier 2 — a ratio between two things measured in the same run, on the same
 * machine, seconds apart. Machine speed cancels, so a ratio holds where an
 * absolute number would not.
 */
export function assertAtMostRatio(label: string, ratio: number, ceiling: number, why: string): void {
  record(2, label, ratio <= ceiling, `${ratio.toFixed(2)}x (ceiling ${ceiling}x) — ${why}`);
}

/**
 * Tier 3 — an absolute wall-clock ceiling. Always recorded, asserted only where
 * headroom is large enough that a loaded runner cannot trip it. The convention
 * this project already follows: `provider.test.mts` asserts 500 µs against ~63 µs
 * observed.
 */
export function assertAtMostNs(label: string, ns: number, ceilingNs: number, why: string): void {
  record(3, label, ns <= ceilingNs, `${fmtNs(ns)} (ceiling ${fmtNs(ceilingNs)}) — ${why}`);
}

// ---------------------------------------------------------------------------
// Guards against measuring the wrong thing
// ---------------------------------------------------------------------------

function sha256(file: string): string {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

/**
 * Refuse to benchmark a debug or stale addon.
 *
 * `scripts/build.mts` copies the cargo artifact over `aethervfs.node`, and the
 * default `pnpm build` copies the **debug** one. `pnpm bench` builds release
 * first; this checks that what sits in the package directory is that build, byte
 * for byte. It has already earned its place: a concurrent debug build in this
 * repository replaced the release addon three seconds after it was installed, and
 * without this the suite would have reported debug numbers as release ones.
 */
export function assertReleaseAddon(): void {
  const packaged = path.join(PKG_DIR, 'aethervfs.node');
  const release = path.join(REPO_RUST_DIR, 'target', 'release', 'aethervfs.dll');
  if (!fs.existsSync(packaged)) {
    throw new Error(`bench: ${packaged} is missing — run \`pnpm bench\`, which builds it.`);
  }
  if (!fs.existsSync(release)) {
    throw new Error(
      `bench: ${release} is missing. These benchmarks are meaningless against a debug ` +
        'addon, so they refuse to run without a release build. Use `pnpm bench`.'
    );
  }
  const a = sha256(packaged);
  const b = sha256(release);
  if (a !== b) {
    throw new Error(
      'bench: `aethervfs.node` is not the current release build.\n' +
        `  packaged  ${a.slice(0, 16)}…  ${fs.statSync(packaged).mtime.toISOString()}\n` +
        `  release   ${b.slice(0, 16)}…  ${fs.statSync(release).mtime.toISOString()}\n` +
        'Something rebuilt or replaced the addon after the release build — a concurrent\n' +
        '`pnpm test` (which builds debug) will do exactly this. Re-run `pnpm bench`.'
    );
  }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

export function fmtNs(ns: number): string {
  if (ns < 1_000) return `${ns.toFixed(1)} ns`;
  if (ns < 1_000_000) return `${(ns / 1_000).toFixed(2)} µs`;
  return `${(ns / 1_000_000).toFixed(2)} ms`;
}

export function heading(title: string): void {
  process.stdout.write(`\n${title}\n${'-'.repeat(title.length)}\n`);
}

export function environment(): string {
  const cpus = os.cpus();
  return (
    `node ${process.version}   ${cpus.length} x ${cpus[0]?.model.trim() ?? 'unknown'}\n` +
    `${os.type()} ${os.release()}   addon: release, sha256-verified against target/release`
  );
}

/** Print the samples taken so far as a table, then clear them. */
export function table(): void {
  if (results.length === 0) return;
  const w = Math.max(...results.map((r) => r.name.length));
  process.stdout.write(
    `  ${'case'.padEnd(w)}  ${'median'.padStart(10)}  ${'min'.padStart(10)}  ${'ops/s'.padStart(13)}  iters\n`
  );
  for (const r of results) {
    process.stdout.write(
      `  ${r.name.padEnd(w)}  ${fmtNs(r.nsPerOp).padStart(10)}  ${fmtNs(r.minNsPerOp).padStart(10)}  ` +
        `${Math.round(r.opsPerSec).toLocaleString('en-US').padStart(13)}  ${r.iterations}\n`
    );
  }
  results.length = 0;
}

/**
 * Print every check and return the process exit code.
 *
 * A benchmark suite that cannot fail is a report. This one is a gate, so the exit
 * code is the product and the tables above it are the evidence.
 *
 * Only the tiers named by `BENCH_GATE_TIERS` reach the exit code — see
 * `gatingTiers()` for why CI narrows that to tier 1. Every check is printed
 * either way; a failing check in a non-gating tier prints as `WARN` and is
 * counted separately, so the number is never silently dropped. The distinction
 * is deliberately visible in the output: a run that says "gating tiers: 1" is
 * making a weaker claim than one that says "1,2,3", and the log should not let
 * a reader confuse the two.
 */
export function verdict(): number {
  const gating = gatingTiers();
  heading('checks');

  const failed = checks.filter((c) => !c.ok && gating.has(c.tier));
  const warned = checks.filter((c) => !c.ok && !gating.has(c.tier));

  for (const c of checks) {
    const mark = c.ok ? 'ok  ' : gating.has(c.tier) ? 'FAIL' : 'WARN';
    process.stdout.write(`  ${mark}  [tier ${c.tier}]  ${c.label}\n          ${c.detail}\n`);
  }

  const gateList = [...gating].sort((a, b) => a - b).join(',');
  process.stdout.write(
    `\ngating tiers: ${gateList}` +
      (gateList === '1,2,3' ? '' : ' (timing tiers report only — set BENCH_GATE_TIERS=all to enforce)') +
      '\n'
  );
  process.stdout.write(
    `${checks.length - failed.length - warned.length}/${checks.length} checks passed` +
      (failed.length ? `, ${failed.length} FAILED` : '') +
      (warned.length ? `, ${warned.length} over ceiling but not gating` : '') +
      '\n'
  );
  if (warned.length) {
    process.stdout.write(
      '\n  A WARN is a real number over its ceiling, not a pass. It does not fail this\n' +
        '  run because this machine\'s noise floor is unknown (see gatingTiers). Re-run\n' +
        '  `pnpm bench` somewhere quiet before concluding either way.\n'
    );
  }
  return failed.length === 0 ? 0 : 1;
}

export { require as benchRequire };
