// Prints the event digest and state hash the WebAssembly build produces for
// N synthetic commands with a given seed, optionally as trading days with auctions.
// Usage: node wasm-digest.cjs PKG_DIR N SEED [continuous|auctions]
const path = require('node:path');
const [pkgDir, count, seed, mode = 'continuous'] = process.argv.slice(2);
const { Playground } = require(path.resolve(pkgDir, 'crossbook_wasm.js'));
const playground = new Playground();
if (mode === 'auctions') playground.stageSession(Number(count), Number(seed));
else playground.stage(Number(count), Number(seed));
const summary = JSON.parse(playground.runStaged());
console.log(`${summary.eventDigest} ${summary.stateHash}`);
