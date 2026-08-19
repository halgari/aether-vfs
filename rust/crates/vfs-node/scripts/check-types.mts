#!/usr/bin/env node

// **Does the package's declaration describe the package that is actually there?**
//
// ## What changed, and why this check still earns its place
//
// Before the TypeScript migration this script compared a *hand-written*
// `index.d.ts` against the runtime exports of a *hand-written* `index.cjs`. Both
// halves were asserted, and the drift it caught was real: `Session.readdir` and
// `Session.getattr` had been missing from the declaration entirely, which no type
// checker can notice, because nothing references what does not exist.
//
// `index.d.mts` is now emitted from `index.mts`, so that particular comparison is
// mostly a tautology — `tsc` cannot disagree with itself about a signature. The
// drift moved rather than disappeared, and it moved somewhere sharper:
//
//  1. **`native.mts` is still hand-written.** It declares a Rust binary. Nothing
//     in the JavaScript toolchain can derive it, because the thing that could —
//     `@napi-rs/cli` — is the network dependency this package refuses. A
//     `#[napi]` export renamed, added, or dropped on the Rust side is invisible
//     to `tsc`, and **this is the check that sees it**. That is the same class of
//     defect as the original `Session.readdir`, just now confined to one file.
//  2. **The declaration is a build output, so it can be stale.** That failure
//     mode did not exist when the file was authored by hand. §1 below compares
//     each output's mtime against its source.
//  3. **Ten names are shadowed on purpose**, and the mechanism that shadows them
//     is `tsc`'s CommonJS emit order: a local `export function memory` has to be
//     assigned to `exports` *before* `__exportStar` copies Rust's handle-taking
//     `memory` in. §4 asserts that every one of them is still the wrapper and not
//     the primitive. If that ever inverted, `readonly(providerObject)` would
//     reach a Rust function expecting an integer.
//
// It is **not** a type checker, and a real one still would not have been the fix
// for the two documentation defects this script was written alongside. Both lived
// inside JSDoc prose — an example that mounted a provider on root 1 and read it
// back from root 0, and a sentence claiming environment variables were
// child-only. `tsc` does not evaluate the code in a ``` fence and has no opinion
// about which root a comment mentions. `tsc --noEmit` is now a separate gate in
// `pnpm test`; it complements this and does not replace it.
//
// ## What it stopped being
//
// It is no longer dependency-free. It used to scrape the declaration with regular
// expressions specifically so it could run with nothing installed. It now asks
// the TypeScript compiler for the module's exported symbols, which is available
// because `typescript` is a devDependency as of task 1 and which is strictly
// better on a *generated* file: a regex over emitted output checks the emitter's
// formatting, while `getExportsOfModule` follows `export * from './native.mjs'`
// into the second declaration file for free and can tell a type-only export from
// a value.
//
// Run: `node scripts/check-types.mts`. Exits non-zero with the drift listed.

import { createRequire } from 'node:module';
import * as fs from 'node:fs';
import * as path from 'node:path';

const require = createRequire(import.meta.url);
const ts = require('typescript') as typeof import('typescript');

const pkgDir = path.resolve(import.meta.dirname, '..');
const problems: string[] = [];

// ---------------------------------------------------------------------------
// 1. The build outputs exist and are not older than their sources.
// ---------------------------------------------------------------------------

const pairs: Array<[string, string[]]> = [
  ['index.mts', ['index.mjs', 'index.d.mts']],
  ['native.mts', ['native.mjs', 'native.d.mts']],
  ['provider-host.mts', ['provider-host.mjs']],
];

let missing = false;
for (const [src, outs] of pairs) {
  const srcAt = fs.statSync(path.join(pkgDir, src)).mtimeMs;
  for (const out of outs) {
    const at = path.join(pkgDir, out);
    if (!fs.existsSync(at)) {
      missing = true;
      problems.push(`\`${out}\` does not exist. It is emitted from \`${src}\`; run \`pnpm build\`.`);
      continue;
    }
    if (fs.statSync(at).mtimeMs + 1 < srcAt) {
      problems.push(
        `\`${out}\` is older than \`${src}\`. The declaration is a build output as of ` +
          'the TypeScript migration, so a stale one is a new way to be wrong; run `pnpm build`.'
      );
    }
  }
}

if (missing) {
  report();
}

// ---------------------------------------------------------------------------
// 2. The addon's real exports against `native.mts`, which is hand-written.
//
//    One direction only. The other — a name `native.mts` declares that the addon
//    does not have — is already enforced at load: `native.mts` takes every export
//    through `fn(name)`, which throws a `TypeError` naming the addon and the
//    missing name. If `require` below succeeded, that direction is clean.
// ---------------------------------------------------------------------------

const runtime = require(path.join(pkgDir, 'index.mjs')) as Record<string, unknown>;
const nativeMod = require(path.join(pkgDir, 'native.mjs')) as Record<string, unknown>;
const addon = require(path.join(pkgDir, 'aethervfs.node')) as Record<string, unknown>;

const addonNames = Object.keys(addon);

for (const name of addonNames) {
  if (!(name in nativeMod)) {
    problems.push(
      `the addon exports \`${name}\` and \`native.mts\` does not declare it. A TypeScript ` +
        'host cannot call it, and nothing else would report that — `native.mts` is the one ' +
        'declaration in this package that is still written rather than emitted.'
    );
  }
}

// ---------------------------------------------------------------------------
// 3. Nothing the addon exports is unreachable through the package entry.
// ---------------------------------------------------------------------------

for (const name of addonNames) {
  if (!(name in runtime)) {
    problems.push(
      `the addon exports \`${name}\` and \`require('aethervfs').${name}\` is undefined. ` +
        "`index.mts` forwards the addon with `export *`, so this means the name was lost " +
        'between `native.mts` and here.'
    );
  }
}

// ---------------------------------------------------------------------------
// 4. The ten deliberate shadows are still shadows.
//
//    `index.mts` exports its own `memory`, `readonly`, … over the addon's, which
//    take integer handles. The mechanism is emit order — a local export is
//    assigned to `exports` before `__exportStar` runs — so it is worth asserting
//    rather than assuming.
// ---------------------------------------------------------------------------

const SHADOWED = [
  'memory',
  'readonly',
  'seekable',
  'subdir',
  'cached',
  'layered',
  'overlay',
  'router',
  'assertConformance',
  'registerProvider',
  'releaseProvider',
] as const;

for (const name of SHADOWED) {
  if (!(name in addon)) {
    problems.push(
      `\`${name}\` is listed as a deliberate shadow and the addon no longer exports it. ` +
        'Either the primitive was renamed in Rust or this list is stale.'
    );
    continue;
  }
  if (runtime[name] === addon[name]) {
    problems.push(
      `\`require('aethervfs').${name}\` is the addon's own function, not the wrapper in ` +
        '`index.mts`. The wrapper is what accepts a `Provider` where Rust wants an integer ' +
        "handle, so a host's `" +
        name +
        '(provider)` would now reach Rust with an object. This is an emit-order regression ' +
        'in `export *`, not a source change.'
    );
  }
}

// ---------------------------------------------------------------------------
// 5. The declared surface against the runtime surface, both directions.
//
//    Asked of the compiler rather than scraped, so `export * from './native.mjs'`
//    is followed and a type-only export is distinguishable from a value.
// ---------------------------------------------------------------------------

const entry = path.join(pkgDir, 'index.d.mts');
const program = ts.createProgram([entry], {
  module: ts.ModuleKind.NodeNext,
  moduleResolution: ts.ModuleResolutionKind.NodeNext,
  target: ts.ScriptTarget.ES2023,
  types: ['node'],
  strict: true,
  skipLibCheck: true,
  noEmit: true,
});
const checker = program.getTypeChecker();
const entrySource = program.getSourceFile(entry);
if (entrySource === undefined) {
  problems.push(`could not load \`index.d.mts\` as a TypeScript program.`);
  report();
}
const moduleSymbol = checker.getSymbolAtLocation(entrySource!);
if (moduleSymbol === undefined) {
  problems.push('`index.d.mts` does not look like a module — it declares no exports.');
  report();
}

/**
 * Name -> whether the export has a value meaning, and whether that value has to
 * be a `function` at runtime.
 *
 * "Has to be a function" is call signatures, construct signatures, **or** a
 * `prototype` property. The third is not padding: `Provider`'s constructor object
 * deliberately has no `new` — `Provider::from_handle` is `#[napi(factory)]`, so
 * the class has no JS constructor and `new Provider()` throws — yet it is a
 * function at runtime like every class. Without the `prototype` arm this check
 * reports that as drift, which is how it was first written and what the first run
 * caught.
 */
const declared = new Map<string, { value: boolean; fn: boolean }>();

for (const raw of checker.getExportsOfModule(moduleSymbol!)) {
  const sym = raw.flags & ts.SymbolFlags.Alias ? checker.getAliasedSymbol(raw) : raw;
  const value = (sym.flags & ts.SymbolFlags.Value) !== 0;
  let fn = false;
  if (value) {
    const decl = sym.valueDeclaration ?? sym.declarations?.[0];
    const type = decl
      ? checker.getTypeOfSymbolAtLocation(sym, decl)
      : checker.getDeclaredTypeOfSymbol(sym);
    fn =
      type.getCallSignatures().length > 0 ||
      type.getConstructSignatures().length > 0 ||
      type.getProperty('prototype') !== undefined;
  }
  declared.set(raw.getName(), { value, fn });
}

for (const [name, kind] of declared) {
  if (!kind.value) continue; // a type-only export has nothing to be at runtime
  const v = runtime[name];
  if (v === undefined) {
    problems.push(
      `the declaration exports the value \`${name}\` and \`require('aethervfs')\` has no ` +
        'such export.'
    );
    continue;
  }
  if (kind.fn && typeof v !== 'function') {
    problems.push(
      `the declaration types \`${name}\` as a function or class; at runtime it is a ${typeof v}.`
    );
  }
  if (!kind.fn && typeof v === 'function') {
    problems.push(
      `the declaration types \`${name}\` as a plain value; at runtime it is a function.`
    );
  }
}

for (const name of Object.keys(runtime)) {
  const kind = declared.get(name);
  if (kind === undefined) {
    problems.push(
      `\`require('aethervfs').${name}\` is exported and the declaration does not declare it. ` +
        'A TypeScript host cannot call it, and nothing else would report that.'
    );
  } else if (!kind.value) {
    problems.push(
      `\`${name}\` is exported at runtime and the declaration only declares it as a type.`
    );
  }
}

// ---------------------------------------------------------------------------
// 6. Declared members of the two addon classes must exist on their prototypes.
//    napi-derive names JS methods in camelCase from Rust snake_case, so a rename
//    on either side is exactly the drift this catches — and these two interfaces
//    are hand-written in `native.mts`.
// ---------------------------------------------------------------------------

for (const cls of ['Provider', 'Session'] as const) {
  const sym = checker
    .getExportsOfModule(moduleSymbol!)
    .find((s) => s.getName() === cls);
  if (sym === undefined) {
    problems.push(`the declaration no longer exports \`${cls}\`.`);
    continue;
  }
  const target = sym.flags & ts.SymbolFlags.Alias ? checker.getAliasedSymbol(sym) : sym;
  const ctor = runtime[cls];
  if (typeof ctor !== 'function') {
    problems.push(`\`require('aethervfs').${cls}\` is not a constructor.`);
    continue;
  }
  const proto = (ctor as { prototype: object }).prototype;

  // Instance side. Symbol-named members are checked explicitly below, because
  // they are installed by `index.mts` rather than by napi-derive.
  for (const member of checker.getPropertiesOfType(checker.getDeclaredTypeOfSymbol(target))) {
    const name = member.getName();
    if (name.startsWith('__@')) continue;
    if (!(name in proto)) {
      problems.push(
        `\`native.mts\` declares \`${cls}.${name}\` and the runtime prototype has no such ` +
          'member. napi-derive camelCases Rust names, so check the spelling on both sides.'
      );
    }
  }

  // Static side.
  const staticType = checker.getTypeOfSymbolAtLocation(
    target,
    target.valueDeclaration ?? target.declarations![0]!
  );
  for (const member of checker.getPropertiesOfType(staticType)) {
    const name = member.getName();
    if (name === 'prototype' || name.startsWith('__@')) continue;
    if (!(name in (ctor as object))) {
      problems.push(
        `\`native.mts\` declares the static \`${cls}.${name}\` and the addon's constructor ` +
          'has no such property.'
      );
    }
  }

  // `Symbol.dispose` is installed by `index.mts`, not by Rust. If that
  // assignment were ever dropped, `using` would silently stop releasing.
  if (!(Symbol.dispose in proto)) {
    problems.push(
      `\`${cls}.prototype[Symbol.dispose]\` is missing. \`index.mts\` installs it; without ` +
        'it a `using` declaration compiles and releases nothing.'
    );
  }
}

// ---------------------------------------------------------------------------

report();

function report(): never {
  if (problems.length > 0) {
    process.stderr.write(
      `the aethervfs declaration does not match the package (${problems.length} problem${
        problems.length === 1 ? '' : 's'
      }):\n\n` +
        problems.map((p) => `  * ${p}`).join('\n') +
        '\n\n`index.d.mts` is emitted from `index.mts`, but `native.mts` — the addon\'s own ' +
        'surface — is still written by hand. Nothing else keeps it in step with the addon.\n'
    );
    process.exit(1);
  }

  const values = [...declared.values()].filter((k) => k.value).length;
  process.stdout.write(
    `the declaration matches the package: ${values} declared value(s), ` +
      `${declared.size - values} type(s), ${Object.keys(runtime).length} runtime export(s), ` +
      `${addonNames.length} addon export(s), ${SHADOWED.length} deliberate shadow(s).\n`
  );
  process.exit(0);
}
