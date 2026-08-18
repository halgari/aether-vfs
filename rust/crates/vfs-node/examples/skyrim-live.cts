#!/usr/bin/env node

// Live launch of Skyrim SE from a Stored zip, composed and driven from
// TypeScript — the Node counterpart of `vfs-directord`'s `skyrim-live` binary.
//
// ```text
// pnpm build:release && node examples/skyrim-live.cts
// ```
//
// The claim it exists to make is narrow and worth stating exactly: **a host
// writing TypeScript can compose the same provider graph the Rust harness
// composes and launch the same game through it.** Nothing here reaches into the
// kernel; every line below is a call the `aethervfs` package already offers a
// consumer, plus the two it was missing until this file needed them (`zip` and
// `subdir`) and the write layer (`setWriteLayer`).
//
// ## What it mirrors, and the four things that are easy to get wrong
//
//  1. **The archive is flattened.** `skyrimse.zip`'s entries all sit inside a
//     single top-level `Skyrim Special Edition/` folder, so mounting the zip
//     directly leaves the game's own image at `Skyrim Special Edition/
//     SkyrimSE.exe` and a launch of `SkyrimSE.exe` resolves nothing. `subdir`
//     discards that level. The Rust harness detects the prefix at runtime; this
//     one names it and *asserts* the result, which is the same protection.
//  2. **The write layer is the root-scoped overlay directory.** The shim's local
//     overlay is per-root on disk, so a layer declared at its parent shows the
//     director an empty layer while the shim writes one level deeper — a write
//     that then reads back as missing. `overlayLayerDir(0)` is the path that
//     agrees with the shim.
//  3. **DirectX redistributables are static imports the archive does not
//     carry.** They ship with the DX runtime, so the staging walk cannot find
//     them in the graph and the loader fails before the shim exists.
//     `stageFallbackDirs` supplies them from real disk.
//  4. **Root 1 is declared by the junction's own spelling, not its target.** A
//     declared root's path is matched literally against what a real NT open
//     spells, and the game spells the junction path. Declaring the resolved
//     target instead builds a root the match can never satisfy, leaving the
//     save exactly as invisible as it was before the root existed — silently.
//     The *provider* behind it needs the resolved directory, so the two are
//     deliberately different strings here.
//
// ## What it does not do, on purpose
//
// The Rust harness also creates the `My Games` junctions, seeds profile INIs,
// stages the DX redistributables, ensures Steam is running and logged in, and
// disables the Steam overlay. Those are machine setup, they persist between
// runs, and reimplementing them here would be a second place for them to drift.
// This asserts each precondition and refuses to launch without it, because the
// failure this project keeps meeting is not a crash — it is a run that looks
// fine and measured nothing.
//
// `require` with type annotations rather than `import`: node runs this file
// directly and strips annotations without rewriting module syntax, exactly as
// the other examples here do.

import type { Provider, Session } from '../index.cjs';

const assert: typeof import('node:assert') = require('assert');
const cp: typeof import('node:child_process') = require('child_process');
const fs: typeof import('node:fs') = require('fs');
const path: typeof import('node:path') = require('path');

const mod: typeof import('../index.cjs') = require('../index.cjs');
const { Session: VfsSession, disk, subdir, zip } = mod;

// stdout is a pipe when this runs as a background task, and Node's writes to a
// pipe are asynchronous — a harness whose log lags behind the game is useless
// for deciding whether the game got anywhere. `writeSync` keeps the log current.
function log(line: string): void {
  fs.writeSync(1, `${line}\n`);
}

function envPath(key: string, fallback: string): string {
  const v = process.env[key];
  return v && v.length > 0 ? v : fallback;
}

/** Skyrim SE's Steam AppID (`appmanifest_489830.acf`). */
const APP_ID = '489830';

/** The single top-level folder inside the archive. Asserted below, not assumed. */
const ZIP_PREFIX = 'Skyrim Special Edition';

const zipPath = envPath('VFS_SKYRIM_ZIP', String.raw`C:\tmp\skyrimse.zip`);
const dataDir = envPath('VFS_SKYRIM_DATA', String.raw`C:\tmp\skyrim-data`);
const gameRoot = envPath('VFS_SKYRIM_ROOT', String.raw`C:\tmp\skyrim-runtime`);
const profiles = path.join(dataDir, 'profiles');
const dxRedist = path.join(dataDir, 'dx-redist');
const myGamesDocs = path.join(
  process.env.USERPROFILE ?? '',
  'Documents',
  'My Games',
  'Skyrim Special Edition'
);

// ---------------------------------------------------------------------------
// Preconditions. Each one is a thing that, missing, produces a run that looks
// like it worked.
// ---------------------------------------------------------------------------

function steamRunning(): boolean {
  // `tasklist` rather than a Node process API because there isn't one; the
  // filter makes the output a single line whether or not it matched.
  const out = cp.execFileSync('tasklist', ['/FI', 'IMAGENAME eq steam.exe', '/NH'], {
    encoding: 'utf8',
  });
  return /steam\.exe/i.test(out);
}

function preflight(): string {
  const problems: string[] = [];

  if (!fs.existsSync(zipPath) || !fs.statSync(zipPath).isFile()) {
    problems.push(`VFS_SKYRIM_ZIP is not a file: ${zipPath}`);
  }
  if (!fs.existsSync(dxRedist)) {
    problems.push(
      `no DX redistributable directory at ${dxRedist}. SkyrimSE.exe statically ` +
        'imports D3DCompiler_43.dll and friends, which ship with the DirectX ' +
        'runtime and are not in the game archive, so staging cannot resolve ' +
        'them from the graph and the loader fails before the shim exists.'
    );
  }
  if (!fs.existsSync(profiles)) {
    problems.push(`no profiles directory at ${profiles} (root 1's provider has nothing to serve)`);
  }
  if (!steamRunning()) {
    problems.push(
      'steam.exe is not running. steam_appid.txt lets SteamAPI_Init talk to a ' +
        'running client instead of bouncing through steam://run, but it still ' +
        'needs the client, and it must be logged in.'
    );
  }

  // The junction is what puts the game's saves and INIs somewhere a second
  // managed root can see. Resolve it the way the OS does rather than trusting
  // that it still points where this project configured it.
  let resolved = '';
  if (!fs.existsSync(myGamesDocs)) {
    problems.push(
      `no My Games directory at ${myGamesDocs}. The Rust harness creates it as ` +
        'a junction to the profiles directory; this harness does not, so set it ' +
        'up once with skyrim-live (or mklink /J) before running this.'
    );
  } else {
    resolved = fs.realpathSync(myGamesDocs);
    if (path.resolve(resolved).toLowerCase() !== path.resolve(profiles).toLowerCase()) {
      log(
        `  WARNING: ${myGamesDocs} resolves to ${resolved}, not the configured ` +
          `profiles directory ${profiles}. Root 1's numbers describe whatever it ` +
          'actually points at.'
      );
    }
  }

  if (problems.length > 0) {
    log('preflight failed:');
    for (const p of problems) log(`  - ${p}`);
    process.exit(1);
  }
  return resolved;
}

/**
 * Valve's documented dev override: with `steam_appid.txt` beside the image,
 * `SteamAPI_Init` verifies ownership against the running client instead of
 * calling `RestartAppIfNecessary`, which would hand off to `steam://run` and
 * relaunch the game outside our session entirely.
 *
 * Written to real disk under the root *and* into the shim's overlay layer,
 * because the shim answers this one filename out of its overlay before the
 * director is ever asked — so the two have to agree on the physical path.
 */
function writeSteamAppId(overlayDir: string): void {
  const body = `${APP_ID}\n`;
  fs.mkdirSync(gameRoot, { recursive: true });
  fs.mkdirSync(overlayDir, { recursive: true });
  fs.writeFileSync(path.join(gameRoot, 'steam_appid.txt'), body);
  fs.writeFileSync(path.join(overlayDir, 'steam_appid.txt'), body);
}

/**
 * Steam's own launch variables, if this process inherited them, make the game
 * think it was started by the client and take the `steam://run` path. The child
 * inherits this process's environment, so clearing them here clears them there.
 */
function clearSteamEnv(): void {
  for (const k of [
    'SteamAppId',
    'SteamGameId',
    'SteamOverlayGameId',
    'SteamClientLaunch',
    'SteamEnv',
    'SteamTenfoot',
    'SteamAppUser',
  ]) {
    delete process.env[k];
  }
}

function gameAlive(): boolean {
  const out = cp.execFileSync('tasklist', ['/FI', 'IMAGENAME eq SkyrimSE.exe', '/NH'], {
    encoding: 'utf8',
  });
  return /SkyrimSE\.exe/i.test(out);
}

async function main(): Promise<void> {
  const resolvedProfiles = preflight();
  clearSteamEnv();

  log('vfs (typescript): live Skyrim launch');
  log(`  zip:       ${zipPath}`);
  log(`  root 0:    ${gameRoot}`);
  log(`  root 1:    ${myGamesDocs}`);
  log(`  root 1 ->  ${resolvedProfiles}`);

  const session: Session = new VfsSession('skyrim-live-ts');
  session.addRoot(0, 'game', gameRoot);
  session.addRoot(1, 'my-games', myGamesDocs);

  const overlayDir = session.overlayLayerDir(0);
  writeSteamAppId(overlayDir);
  log(`  overlay:   ${overlayDir}`);

  // Lowest layer: the runtime root itself, so anything already staged there on
  // real disk (the DX redistributables the loader needs, steam_appid.txt) is
  // part of the graph rather than something the child reaches around it for.
  session.mount(0, disk(gameRoot));

  // The game, straight out of the archive at its stored offsets. This parses the
  // whole central directory before it returns.
  log('  opening zip index (one-time central-directory parse; ~30-90s on 16 GB)...');
  const t0 = Date.now();
  const archive: Provider = subdir(zip(zipPath), ZIP_PREFIX);
  log(`  zip index ready in ${((Date.now() - t0) / 1000).toFixed(1)}s`);
  session.mount(0, archive);

  // Copy-on-write for the whole graph. Not a sibling mount: an in-place edit of
  // a file the archive holds has to be copied up before it can be written, and
  // only an upper can do that.
  session.setWriteLayer(disk(overlayDir), 0);

  // Root 1's content is one directory, so one provider covering it is the whole
  // composition rather than a shortcut hiding a negative result.
  session.mount(1, disk(resolvedProfiles));

  // **The flattening check, before launching anything.** `subdir` cannot fail on
  // a wrong prefix — a provider may legitimately serve paths that did not exist
  // when the graph was built — so a typo here would produce a graph serving
  // nothing, and `launch` would report only that it could not resolve an image.
  // One `getattr` says which of the two happened.
  const exeStat = session.getattr('SkyrimSE.exe', 0);
  assert.ok(
    exeStat !== null,
    `root 0's graph does not serve SkyrimSE.exe. The archive's contents sit ` +
      `inside "${ZIP_PREFIX}/" and subdir() is what flattens that away, so either ` +
      'that folder name has changed or the mount order is wrong. Nothing below ' +
      'this line can work.'
  );
  const master = session.getattr('Data/Skyrim.esm', 0);
  assert.ok(
    master !== null,
    "root 0's graph serves SkyrimSE.exe but not Data/Skyrim.esm. The image would " +
      'launch into a main menu with an empty load order, which is the exact ' +
      'symptom this project spent a week on — refusing here instead.'
  );
  log(`  graph serves SkyrimSE.exe (${exeStat.size} bytes) and Data/Skyrim.esm (${master.size} bytes)`);

  const roots = session.roots();
  for (const r of roots) log(`  declared root ${r.id}: ${r.path}`);

  // `wait: false`, so this thread is free to report while the game runs. The
  // session must outlive the child — it owns the ring and the staged image — so
  // this process has to stay alive until the game exits. Killing this process
  // takes the game down with it.
  log('  launching SkyrimSE.exe (staging its import closure first)...');
  const tLaunch = Date.now();
  session.launch('SkyrimSE.exe', {
    wait: false,
    // The DX redistributables are static imports of the image that the archive
    // does not carry, so the staging walk has to find them on real disk.
    stageFallbackDirs: [dxRedist],
  });
  log(`  READY: launched in ${((Date.now() - tLaunch) / 1000).toFixed(1)}s; ring is serving`);

  // Report while it runs. The interesting number is the first one: opens that
  // reached the director at all. Zero of them, with a game plainly on screen,
  // means it is reading around us.
  const deadline = Date.now() + 20 * 60 * 1000;
  let sawGame = false;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 5000));
    const alive = gameAlive();
    if (alive) sawGame = true;
    const [ok, failed] = session.openTotals();
    log(`  [${new Date().toISOString()}] game=${alive ? 'alive' : 'gone'} director opens ok=${ok} err=${failed}`);
    if (sawGame && !alive) {
      log('  game exited');
      break;
    }
  }

  const rejected = session.rejectedWrites();
  if (rejected.length > 0) {
    log(`  writes refused because no read-write provider served them (${rejected.length}):`);
    for (const r of rejected.slice(0, 20)) log(`    ${r.path} x${r.count}`);
  } else {
    log('  no refused writes');
  }
  const [ok, failed] = session.openTotals();
  log(`  final: director opens ok=${ok} err=${failed}`);
  session.close();
  log('  session closed');
}

main().catch((e: unknown) => {
  log(`error: ${e instanceof Error ? (e.stack ?? e.message) : String(e)}`);
  process.exit(1);
});
