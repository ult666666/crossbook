// Prints the event digest and state hash the WebAssembly build produces for
// N synthetic commands with a given seed. Usage: node wasm-digest.cjs PKG_DIR N SEED
const path = require('node:path');
const [pkgDir, count, seed] = process.argv.slice(2);
const { Playground } = require(path.resolve(pkgDir, 'crossbook_wasm.js'));
const playground = new Playground();
playground.stage(Number(count), Number(seed));
const summary = JSON.parse(playground.runStaged());
console.log(`${summary.eventDigest} ${summary.stateHash}`);
