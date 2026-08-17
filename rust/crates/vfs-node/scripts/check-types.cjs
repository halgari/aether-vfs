#!/usr/bin/env node
'use strict';

// **Does `index.d.ts` describe the package that is actually there?**
//
// `index.d.ts` is hand-written and says so at the top: `@napi-rs/cli` would
// generate it, and using it would put a network npm install into the build path
// of a package whose whole build is four `cargo` calls and four file copies. The
// file's own comment admits the risk — "a drifting hand-written .d.ts is worse
// than a dependency" — and nothing was checking for drift. `Session.readdir` and
// `Session.getattr` were missing from it entirely.
//
// ## What this checks, and what it deliberately does not
//
// It compares the **declared surface** against the **runtime surface**, in both
// directions: a declaration with nothing behind it, and an export nothing
// declares. That is the drift class, and it is the one a hand-written file
// actually suffers from.
//
// It is **not** a type checker, and a real one would not have been the fix for
// the two documentation defects this script was written alongside. Both lived
// inside JSDoc prose — an example that mounted a provider on root 1 and read it
// back from root 0, and a sentence claiming environment variables were
// child-only. `tsc` does not evaluate the code in a ``` fence and has no opinion
// about which root a comment mentions. Adding `tsc` would mean `npx typescript`
// plus `@types/node` (the file references `import('worker_threads').Worker`),
// i.e. a network install, for a check that would have caught neither. That trade
// is recorded rather than taken; if this package ever gains an npm dependency for
// another reason, add `tsc --noEmit` at the same time.
//
// Run: `node scripts/check-types.cjs`. Exits non-zero with the drift listed.

const fs = require('fs');
const path = require('path');

const pkgDir = path.resolve(__dirname, '..');
const dts = fs.readFileSync(path.join(pkgDir, 'index.d.ts'), 'utf8');
const runtime = require(path.join(pkgDir, 'index.cjs'));

const problems = [];

/** Strip block comments, so a name inside prose is never mistaken for a declaration. */
function withoutComments(src) {
  return src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^[ \t]*\/\/.*$/gm, '');
}

const code = withoutComments(dts);

// ---------------------------------------------------------------------------
// 1. Top-level declared values (not types) must exist at runtime.
// ---------------------------------------------------------------------------

const declared = new Map(); // name -> 'function' | 'class' | 'const'

for (const m of code.matchAll(/^export\s+function\s+([A-Za-z_$][\w$]*)\s*[<(]/gm)) {
  declared.set(m[1], 'function');
}
for (const m of code.matchAll(/^export\s+class\s+([A-Za-z_$][\w$]*)/gm)) {
  declared.set(m[1], 'class');
}
for (const m of code.matchAll(/^export\s+const\s+([A-Za-z_$][\w$]*)\s*:/gm)) {
  declared.set(m[1], 'const');
}

// Type-only declarations. Listed so the reverse check below can tell "this name
// is a type, not a value" from "this name is undeclared".
const declaredTypes = new Set();
for (const m of code.matchAll(/^export\s+(?:interface|type)\s+([A-Za-z_$][\w$]*)/gm)) {
  declaredTypes.add(m[1]);
}

for (const [name, kind] of declared) {
  const v = runtime[name];
  if (v === undefined) {
    problems.push(
      `index.d.ts declares \`${name}\` (${kind}) and \`require('aethervfs')\` has no such ` +
        `export. Either the addon lost it or the declaration is stale.`
    );
    continue;
  }
  if ((kind === 'function' || kind === 'class') && typeof v !== 'function') {
    problems.push(
      `index.d.ts declares \`${name}\` as a ${kind}; at runtime it is a ${typeof v}.`
    );
  }
  if (kind === 'const' && typeof v === 'function') {
    problems.push(`index.d.ts declares \`${name}\` as a const; at runtime it is a function.`);
  }
}

// ---------------------------------------------------------------------------
// 2. Every runtime export must be declared. This is the direction that caught
//    `Session.readdir` being absent — a missing declaration is invisible to a
//    type checker, because nothing references what does not exist.
// ---------------------------------------------------------------------------

for (const name of Object.keys(runtime)) {
  if (declared.has(name) || declaredTypes.has(name)) continue;
  problems.push(
    `\`require('aethervfs').${name}\` is exported and \`index.d.ts\` does not declare it. ` +
      `A TypeScript host cannot call it, and nothing else would report that.`
  );
}

// ---------------------------------------------------------------------------
// 3. Declared members of the two classes must exist on their prototypes.
//    napi-derive names JS methods in camelCase from Rust snake_case, so a
//    rename on either side is exactly the drift this catches.
// ---------------------------------------------------------------------------

/** The body of `export class NAME { … }`, by brace matching. */
function classBody(src, name) {
  const head = new RegExp(`^export\\s+class\\s+${name}\\b[^{]*\\{`, 'm').exec(src);
  if (!head) return null;
  let depth = 0;
  const start = head.index + head[0].length - 1;
  for (let i = start; i < src.length; i++) {
    if (src[i] === '{') depth++;
    else if (src[i] === '}') {
      depth--;
      if (depth === 0) return src.slice(start + 1, i);
    }
  }
  return null;
}

for (const cls of ['Provider', 'Session']) {
  const body = classBody(code, cls);
  if (body === null) {
    problems.push(`index.d.ts no longer declares \`class ${cls}\`.`);
    continue;
  }
  const ctor = runtime[cls];
  if (typeof ctor !== 'function') continue;
  const proto = ctor.prototype;
  const own = Object.getOwnPropertyNames(proto);

  const members = new Set();
  // `foo(...)`, `get foo()`, `static foo(...)`, `readonly foo: T`, `foo: T`.
  for (const m of body.matchAll(/^\s*(?:static\s+)?(?:get\s+|set\s+)?([A-Za-z_$][\w$]*)\s*[(:]/gm)) {
    members.add(m[1]);
  }
  members.delete('constructor');

  for (const name of members) {
    if (own.includes(name) || name in proto || typeof ctor[name] === 'function') continue;
    problems.push(
      `index.d.ts declares \`${cls}.${name}\` and the runtime prototype has no such member. ` +
        `napi-derive camelCases Rust names, so check the spelling on both sides.`
    );
  }
}

// ---------------------------------------------------------------------------

if (problems.length > 0) {
  process.stderr.write(
    `index.d.ts does not match the package (${problems.length} problem${
      problems.length === 1 ? '' : 's'
    }):\n\n` +
      problems.map((p) => `  * ${p}`).join('\n') +
      '\n\nThis file is hand-written on purpose (see its header). Nothing else keeps it in ' +
      'step with the addon.\n'
  );
  process.exit(1);
}

process.stdout.write(
  `index.d.ts matches the package: ${declared.size} declared value(s), ` +
    `${declaredTypes.size} type(s), ${Object.keys(runtime).length} runtime export(s).\n`
);
