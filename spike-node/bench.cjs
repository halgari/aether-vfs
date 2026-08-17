// Stage-4 task 5 harness.
//
//   cargo build --release            (in spike-node/)
//   node bench.cjs [--json out.json]
//
// `spike.node` is a copy of target/release/vfs_node_spike.dll and is gitignored.
// This project has been bitten twice by a stale DLL silently producing wrong
// results, so the copy is done here, on every run, with both timestamps printed
// — never trust a staged artifact you did not just stage.
'use strict';

const path = require('path');
const os = require('os');
const fs = require('fs');
const { Worker } = require('worker_threads');

const ADDON = path.join(__dirname, 'spike.node');
const BUILT = path.join(__dirname, 'target', 'release', 'vfs_node_spike.dll');

function stageAddon() {
  if (!fs.existsSync(BUILT)) {
    throw new Error(`${BUILT} does not exist — run \`cargo build --release\` first`);
  }
  const src = fs.statSync(BUILT);
  fs.copyFileSync(BUILT, ADDON);
  const dst = fs.statSync(ADDON);
  console.log(
    `addon: ${src.size} bytes built ${src.mtime.toISOString()} -> staged ${dst.mtime.toISOString()}`,
  );
}
stageAddon();

const native = require(ADDON);
const { makeHandler, counter } = require('./provider.cjs');

// A main event loop that is not idle. Every real Node/Electron host has one:
// the "main-loop provider" numbers below are a best case unless this is on.
function startMainLoopLoad(dutyMs) {
  let stop = false;
  const turn = () => {
    if (stop) return;
    const t0 = process.hrtime.bigint();
    while (Number(process.hrtime.bigint() - t0) < dutyMs * 1e6) {
      /* burn */
    }
    setImmediate(turn);
  };
  setImmediate(turn);
  return () => {
    stop = true;
  };
}

const FILE_SIZE = 64 * 1024 * 1024;
const BLOCK = 1024 * 1024; // vfs-cache DEFAULT_BLOCK_SIZE
const NOP_ITERS = 20000;

const results = { host: {}, contextAware: {}, latency: [], throughput: [], deadlock: {} };

function spawnProvider(mode) {
  return new Promise((resolve, reject) => {
    const w = new Worker(path.join(__dirname, 'provider-worker.cjs'), {
      workerData: { addon: ADDON, mode },
    });
    w.once('error', reject);
    w.once('message', (m) => {
      if (m.ready) resolve({ worker: w, info: m });
    });
  });
}

function ask(worker, msg, key) {
  return new Promise((resolve) => {
    const on = (m) => {
      if (m && key in m) {
        worker.off('message', on);
        resolve(m[key]);
      }
    };
    worker.on('message', on);
    worker.postMessage(msg);
  });
}

function row(r) {
  return [
    r.label.padEnd(34),
    String(r.threads).padStart(2),
    r.mibPerSec > 0 ? r.mibPerSec.toFixed(1).padStart(8) : '       -',
    r.p50Us.toFixed(2).padStart(8),
    r.p99Us.toFixed(2).padStart(9),
    r.maxUs.toFixed(0).padStart(7),
    String(Math.round(r.reads)).padStart(9),
    String(Math.round(r.jsCalls)).padStart(9),
    r.error ? ` ERR:${r.error}` : '',
  ].join(' ');
}

const HEAD =
  'label'.padEnd(34) +
  ' th ' +
  '  MiB/s ' +
  '  p50 us ' +
  '  p99 us ' +
  ' max us ' +
  '    reads ' +
  ' jsCalls';

async function main() {
  results.host = {
    node: process.version,
    napiVersion: process.versions.napi,
    platform: `${os.platform()} ${os.release()}`,
    cpus: os.cpus().length,
    cpuModel: os.cpus()[0].model,
  };
  console.log(`node ${process.version}  N-API ${process.versions.napi}  ${os.cpus().length} logical CPUs`);
  console.log(`${os.cpus()[0].model}`);
  console.log('');

  // ---- 0. Is the addon loadable in a Worker, and are Rust statics shared? ---
  const mainLoads = native.noteIsolateLoad();
  const mainSync = native.registerProvider(makeHandler(native, 'sync'));
  const mainMicro = native.registerProvider(makeHandler(native, 'microtask'));
  const mainMacro = native.registerProvider(makeHandler(native, 'macrotask'));

  const w1 = await spawnProvider('sync');
  results.contextAware = {
    loadedInWorker: true,
    mainIsolateLoadOrdinal: mainLoads,
    workerIsolateLoadOrdinal: w1.info.isolateLoadsSeenInWorker,
    bridgeCountSeenInWorker: w1.info.bridgeCountSeenInWorker,
    bridgeCountSeenInMain: native.bridgeCount(),
    workerBridgeId: w1.info.bridgeId,
  };
  console.log('--- 0. context awareness / isolate sharing ---');
  console.log(JSON.stringify(results.contextAware, null, 2));
  console.log('');

  // ---- 1. Latency floor: no data, one round trip at a time ----------------
  console.log('--- 1. round-trip latency floor (benchNop, no data) ---');
  console.log(HEAD);
  const nop = async (label, bridges, threads) => {
    const r = await native.benchNop({ bridges, threads, iters: NOP_ITERS, label });
    console.log(row(r));
    results.latency.push(r);
    return r;
  };
  await nop('main-loop sync', [mainSync], 1);
  await nop('main-loop microtask (promise)', [mainMicro], 1);
  await nop('main-loop macrotask (setImmediate)', [mainMacro], 1);
  await nop('1-worker sync', [w1.info.bridgeId], 1);
  console.log('');

  // ---- 2. Throughput -------------------------------------------------------
  const microWorker = await spawnProvider('microtask');
  const extra = [];
  const WORKERS = 8;
  for (let i = 1; i < WORKERS; i++) extra.push(await spawnProvider('sync'));
  const workerBridges = [w1.info.bridgeId, ...extra.map((e) => e.info.bridgeId)];

  await nop(`${WORKERS}-worker sync (concurrent)`, workerBridges, WORKERS);
  await nop('1-worker microtask (promise)', [microWorker.info.bridgeId], 1);
  console.log('');

  console.log('--- 2. sequential read throughput, 64 MiB per thread ---');
  console.log(HEAD);
  const read = async (label, bridges, threads, readSize, cached) => {
    const r = await native.benchRead({
      bridges,
      threads,
      fileSize: FILE_SIZE,
      readSize,
      cached,
      blockSize: BLOCK,
      label,
    });
    console.log(row(r));
    results.throughput.push({ ...r, readSize, cached, config: label });
    return r;
  };

  for (const readSize of [4096, 65536]) {
    const k = readSize === 4096 ? '4K' : '64K';
    for (const cached of [false, true]) {
      const c = cached ? 'cached' : 'raw   ';
      await read(`main-loop     ${k} ${c}`, [mainSync], 1, readSize, cached);
      await read(`1-worker      ${k} ${c}`, [w1.info.bridgeId], 1, readSize, cached);
      await read(`2-worker      ${k} ${c}`, workerBridges.slice(0, 2), 2, readSize, cached);
      await read(`4-worker      ${k} ${c}`, workerBridges.slice(0, 4), 4, readSize, cached);
      await read(`8-worker      ${k} ${c}`, workerBridges, 8, readSize, cached);
    }
  }
  // What an async (promise-returning) provider costs at the same shape.
  await read('1-worker  4K  raw    async', [microWorker.info.bridgeId], 1, 4096, false);
  await read('1-worker  4K  cached async', [microWorker.info.bridgeId], 1, 4096, true);
  console.log('');

  // ---- 2a. Control: is the `cached` collapse the block size or the boundary?
  // If throughput scales inversely with block size at a fixed 4 KiB read, the
  // cost is per-hit work proportional to the block, not anything to do with JS.
  console.log('--- 2a. cached, 4 KiB reads, block size swept (control) ---');
  console.log(HEAD);
  for (const bs of [4096, 16384, 65536, 262144, 1048576]) {
    const r = await native.benchRead({
      bridges: [w1.info.bridgeId],
      threads: 1,
      fileSize: FILE_SIZE,
      readSize: 4096,
      cached: true,
      blockSize: bs,
      label: `1-worker 4K cached blk=${(bs / 1024).toFixed(0)}K`,
    });
    console.log(row(r));
    results.throughput.push({ ...r, readSize: 4096, cached: true, blockSize: bs, control: true });
  }
  console.log('');

  // ---- 2b. The same thing with a main loop that is doing something --------
  console.log('--- 2b. main loop under ~1 ms/turn CPU load ---');
  console.log(HEAD);
  const stopLoad = startMainLoopLoad(1);
  await read('LOADED main-loop 4K  raw   ', [mainSync], 1, 4096, false);
  await read('LOADED main-loop 4K  cached', [mainSync], 1, 4096, true);
  await read('LOADED 1-worker  4K  raw   ', [w1.info.bridgeId], 1, 4096, false);
  await read('LOADED 1-worker  4K  cached', [w1.info.bridgeId], 1, 4096, true);
  stopLoad();
  console.log('');

  // ---- 2c. Cross-check: did JS actually run as many times as Rust says? ---
  const workerStats = [];
  for (const e of [w1, microWorker, ...extra]) {
    workerStats.push(await ask(e.worker, { cmd: 'stats' }, 'stats'));
  }
  const rustJsCalls =
    results.latency.reduce((a, r) => a + r.jsCalls, 0) +
    results.throughput.reduce((a, r) => a + r.jsCalls, 0);
  const jsInvocations =
    counter.invocations + workerStats.reduce((a, s) => a + s.invocations, 0);
  results.crossCheck = {
    rustCountedJsCalls: rustJsCalls,
    jsSideInvocations: jsInvocations,
    match: rustJsCalls === jsInvocations,
    mainThreadInvocations: counter.invocations,
    perWorker: workerStats,
    badPayloadReads: results.throughput.reduce((a, r) => a + r.badPayloadReads, 0),
  };
  console.log('--- 2c. cross-check (Rust call count vs JS invocation count) ---');
  console.log(JSON.stringify(results.crossCheck, null, 2));
  console.log('');

  // ---- 3. The deadlock ----------------------------------------------------
  console.log('--- 3. deadlock probes (2 s timeout; settled:false == deadlock) ---');
  const TIMEOUT = 2000;
  const d = {};

  d.mainThreadIntoMainLoopProvider = native.probeBlockingRead(mainSync, TIMEOUT, 4096);
  console.log('main thread  -> main-loop provider  :', JSON.stringify(d.mainThreadIntoMainLoopProvider));

  d.mainThreadIntoWorkerProvider = native.probeBlockingRead(w1.info.bridgeId, TIMEOUT, 4096);
  console.log('main thread  -> worker provider     :', JSON.stringify(d.mainThreadIntoWorkerProvider));

  d.workerIntoOwnLoopProvider = await ask(
    w1.worker,
    { cmd: 'probeSelf', bridge: w1.info.bridgeId, timeoutMs: TIMEOUT, len: 4096 },
    'probeSelf',
  );
  console.log('worker A     -> its own provider    :', JSON.stringify(d.workerIntoOwnLoopProvider));

  d.workerIntoOtherWorkerProvider = await ask(
    w1.worker,
    { cmd: 'probeOther', bridge: extra[0].info.bridgeId, timeoutMs: TIMEOUT, len: 4096 },
    'probeOther',
  );
  console.log('worker A     -> worker B provider   :', JSON.stringify(d.workerIntoOtherWorkerProvider));
  results.deadlock = d;
  console.log('');

  // ---- 4. Is the zero-copy door open? -------------------------------------
  console.log('--- 4. SharedArrayBuffer backing store, observed only ---');
  const sab = new SharedArrayBuffer(1 << 16);
  new Uint8Array(sab).fill(0xcd);
  const mainPin = native.pinSharedBuffer(new Uint8Array(sab));
  const mainPointer = native.pinnedPointer(mainPin);
  const seenByRustThread = native.countPinnedFromRustThread(mainPin, 0xcd);
  const workerPin = await ask(w1.worker, { cmd: 'pinSab', sab }, 'pinSab');
  await ask(w1.worker, { cmd: 'writeSab', sab, value: 0x5a }, 'writeSab');
  const afterWorkerWrite = native.countPinnedFromRustThread(mainPin, 0x5a);
  results.sharedArrayBuffer = {
    bytes: sab.byteLength,
    mainPointer,
    workerPointer: workerPin.pointer,
    sameAddressAcrossIsolates: mainPointer === workerPin.pointer,
    bytesRustThreadSawAfterMainWrote: seenByRustThread,
    bytesRustThreadSawAfterWorkerWrote: afterWorkerWrite,
  };
  console.log(JSON.stringify(results.sharedArrayBuffer, null, 2));
  console.log('');

  const outIdx = process.argv.indexOf('--json');
  if (outIdx > 0 && process.argv[outIdx + 1]) {
    fs.writeFileSync(process.argv[outIdx + 1], JSON.stringify(results, null, 2));
    console.log(`wrote ${process.argv[outIdx + 1]}`);
  }

  for (const e of [w1, microWorker, ...extra]) e.worker.postMessage({ cmd: 'exit' });
  // The registered threadsafe functions keep the main loop referenced; nothing
  // in the spike releases them, so exit explicitly.
  setTimeout(() => process.exit(0), 200);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
