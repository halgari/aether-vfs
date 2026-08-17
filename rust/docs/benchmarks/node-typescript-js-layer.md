# The TypeScript migration cost nothing measurable

**Verdict: no runtime regression.** Every call that crosses into Rust is within
2% of the hand-written layer it replaced, and a read through the graph is 0.98x —
i.e. noise. The one real change is a **3.9 ns** getter on forwarded property
reads, which is 0.02% of the cheapest real call this binding makes.

Reproduce with `pnpm bench:ab` (see the caveats below before you read anything
into a re-run).

## The question, and why it was worth asking

Stage 4's TypeScript migration replaced a hand-written `index.cjs` with one `tsc`
emits from `index.cts`. The JS layer is on the path of every host call, so
"emitted instead of hand-written" is a performance question, and it has a
specific mechanism behind it rather than a vague worry.

`index.cts` forwards the addon with `export * from './native.cjs'`. `tsc` emits
that as `__exportStar`, which defines an **accessor** per forwarded name. The
hand-written file used `module.exports = { ...native, ...overrides }`, which is
plain data properties. So the migration silently turned most of the package's
exports from a property read into a getter call.

That is not speculation — it is visible, and the A/B asserts it:

```
exports:   old 33, new 33
accessors: old 0, new 15      <- __exportStar installs one getter per forwarded name
disk:      old own value, new getter
memory:    old own value, new own value    (a deliberate shadow, so it stayed a data property)
```

Fifteen, not twenty-five: the ten names `index.cts` deliberately shadows
(`memory`, `readonly`, `seekable`, `cached`, `layered`, `overlay`, `router`,
`assertConformance`, `registerProvider`, `releaseProvider`) are declared locally
and stay data properties. Those ten are the wrappers a host is most likely to
call in a loop, so the getter landed on the *less* hot half of the surface by
accident of design.

## Method

The baseline is the hand-written layer at `a14dfac`, checked out into a temp
directory **beside a copy of the current `aethervfs.node`**. Both layers are then
loaded into one process, seconds apart, against the same release addon. Node
treats the two `.node` copies as separate modules because their paths differ,
which is what makes a same-process comparison possible at all.

So the Rust is identical, the machine is identical, the process is identical, and
the only variable is the JavaScript.

## Numbers

AMD Ryzen 9 8945HS, 16 logical CPUs, Windows 11 26200, node v24.19.0, release
build, 2026-08-17.

| case | hand-written | emitted | ratio |
|---|---|---|---|
| `vfs.disk` — property read | 0.6 ns | 4.5 ns | 7.5x |
| `vfs.memory` — property read (shadow, data property both sides) | 4.7 ns | 3.1 ns | 0.66x |
| `version()` | 62.4 ns | 63.5 ns | **1.02x** |
| `statusName(2)` | 375.1 ns | 374.6 ns | **1.00x** |
| `getattr` through `disk()` | 17.88 µs | 17.75 µs | **0.99x** |
| `readFile` 64 B through `disk()` | 346.53 µs | 339.43 µs | **0.98x** |

**Getter overhead: 3.9 ns per access.**

## Reading the 7.5x honestly

A 7.5x ratio on property access is the largest number in the table and the least
important one, so it is worth saying why rather than burying it.

It is 7.5x of **0.6 ns**. The absolute cost is 3.9 ns. The cheapest real thing
this binding does — `getattr` against a Rust `memory()` leaf, no I/O, no bridge —
is ~1.1 µs, so one getter is **0.35%** of that; against the 17.75 µs `getattr`
through `disk()` measured above it is 0.02%. A host would have to read a
forwarded export roughly 4,500 times to pay for a single `getattr`.

The ratio is also flattered by V8: a monomorphic data-property read on a hot
object is about as fast as JavaScript gets, so anything at all looks like a large
multiple of it. Note that `vfs.memory` — a data property on *both* sides — came
out at 4.7 ns old versus 3.1 ns new, which is the measurement noise floor for
this kind of access and is larger than the getter's own cost.

**Conclusion: the mechanism is real, the cost is not.** No action taken, and none
recommended. It is recorded so that nobody re-derives the concern from first
principles and "fixes" it by hand-maintaining the declaration again, which is the
duplication the migration removed.

## Caveats

* **This is a one-shot, not a gate.** It pins a historical commit, which is
  meaningful while the migration is recent and meaningless afterwards.
  `pnpm bench` (the durable gate) does not run it. See
  [node-binding-surface.md](./node-binding-surface.md).
* **It is HEAD versus the last hand-written layer, not the migration commit
  versus its parent.** The JS layer has since taken behavioural fixes (a
  `releaseProvider` handle guard, a `ProviderWorker.close()` fix). Neither touches
  the paths measured here, but the comparison is not surgically the migration.
* `bench/ab-js-layer.mts --baseline <rev>` re-points it at any commit, which is
  the useful form next time this layer changes shape.
* The run leaks one temp directory. Its copy of `aethervfs.node` is mapped into
  the process, and Windows does not delete a loaded DLL; the script says so
  rather than crashing on it.
