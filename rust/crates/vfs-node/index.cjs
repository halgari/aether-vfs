'use strict';

// The package entry point. It exists to do one thing the addon cannot do for
// itself: tell Rust where the addon was loaded from.
//
// `vfs_embed::LaunchOpts::shim_dll` falls back to searching next to
// `std::env::current_exe()`, which inside an addon is `node.exe` — wherever the
// user happens to have installed Node, and nowhere near the DLLs this package
// ships. `__dirname` is the answer and only JS has it, so it is handed over at
// load time. Requiring `aethervfs.node` directly skips this and produces a
// "vfs_shim_dll.dll not found" that names no candidate directories.

const fs = require('fs');
const path = require('path');

const addonPath = path.join(__dirname, 'aethervfs.node');

if (!fs.existsSync(addonPath)) {
  throw new Error(
    `aethervfs: native addon not found at ${addonPath}. ` +
      'Build it with `npm run build` (or `npm run build:release`) in ' +
      `${__dirname}. That builds the addon, the shim DLL and the ` +
      'separate-workspace payload DLL, and places all three here.'
  );
}

const native = require(addonPath);

native.setPackageDir(__dirname);

module.exports = native;
