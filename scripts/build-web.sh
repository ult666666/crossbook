#!/usr/bin/env bash
# Builds the WebAssembly engine and its JavaScript bindings into web/pkg.
# Needs the wasm32-unknown-unknown target and wasm-bindgen-cli 0.2.129
# (the same version as the wasm-bindgen crate in crates/crossbook-wasm).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --target wasm32-unknown-unknown -p crossbook-wasm
wasm-bindgen --target web --no-typescript --out-dir web/pkg \
  target/wasm32-unknown-unknown/release/crossbook_wasm.wasm
ls -l web/pkg
