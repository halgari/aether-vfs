// The JS side of the bridge, shared by the main thread and by Workers so that
// both configurations run *identical* provider code and the only difference
// measured is which event loop services the call.
'use strict';

// A preallocated source of bytes. A real provider would hand back whatever it
// fetched; the point of the spike is the boundary cost, not the fetch.
const SOURCE = Buffer.alloc(1 << 20, 0xab);

// Subarray views are cached per length: creating one per call would put a JS
// allocation in the hot path that a real provider (which already has a Buffer)
// would not have.
const views = new Map();
function view(len) {
  let v = views.get(len);
  if (v === undefined) {
    v = len <= SOURCE.length ? SOURCE.subarray(0, len) : Buffer.alloc(len, 0xab);
    views.set(len, v);
  }
  return v;
}

// Counted on the JS side, independently of Rust's own call counter. If Rust
// reports N calls and this reports N, the boundary really was crossed N times;
// a fast throughput number with a zero here would mean it was not.
const counter = { invocations: 0, bytesReturned: 0 };

/**
 * @param {object} native the loaded addon
 * @param {'sync'|'microtask'|'macrotask'} mode how the result is delivered
 */
function makeHandler(native, mode) {
  return function onRead(slot, gen, offset, len) {
    counter.invocations += 1;
    counter.bytesReturned += len;
    // len === 0 is the latency-floor probe: settle with no data at all.
    const data = len === 0 ? null : view(len);
    switch (mode) {
      case 'sync':
        native.complete(slot, gen, 0, data);
        return;
      case 'microtask':
        // What an `async readAt` costs: the promise job queue, one turn.
        Promise.resolve(data).then((d) => native.complete(slot, gen, 0, d));
        return;
      case 'macrotask':
        // What an actual I/O-backed provider costs: a full loop turn.
        setImmediate(() => native.complete(slot, gen, 0, data));
        return;
      default:
        throw new Error(`unknown mode ${mode}`);
    }
  };
}

module.exports = { makeHandler, SOURCE, counter };
