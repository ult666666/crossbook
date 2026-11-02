#!/usr/bin/env bash
# Runs the same synthetic session through the native CLI and the WebAssembly build
# and fails unless both report identical event digests and state hashes. Also checks
# that the reference digests shown by the browser playground are still current.
# A third argument, "auctions", uses trading days with opening and closing auctions.
set -euo pipefail
cd "$(dirname "$0")/.."

count="${1:-1000000}"
seed="${2:-42}"
mode="${3:-continuous}"
gen_flags=()
if [[ "$mode" == auctions ]]; then
  gen_flags=(--auctions)
elif [[ "$mode" != continuous ]]; then
  echo "error: the third argument must be continuous or auctions, not $mode" >&2
  exit 2
fi
work="target/determinism"
mkdir -p "$work"

cargo build --release -p crossbook-cli
cargo build --release --target wasm32-unknown-unknown -p crossbook-wasm
wasm-bindgen --target nodejs --no-typescript --out-dir "$work/pkg" \
  target/wasm32-unknown-unknown/release/crossbook_wasm.wasm

./target/release/crossbook gen --orders "$count" --seed "$seed" ${gen_flags[@]+"${gen_flags[@]}"} --out "$work/flow.xbk" >/dev/null
native_report="$(./target/release/crossbook replay "$work/flow.xbk" --depth 0)"
native_events="$(awk '/^event digest/ {print $3}' <<<"$native_report")"
native_state="$(awk '/^state hash/ {print $3}' <<<"$native_report")"
read -r wasm_events wasm_state < <(node scripts/wasm-digest.cjs "$work/pkg" "$count" "$seed" "$mode")

echo "native ($(uname -m)): events $native_events  state $native_state"
echo "wasm32:          events $wasm_events  state $wasm_state"
if [[ "$native_events" != "$wasm_events" || "$native_state" != "$wasm_state" ]]; then
  echo "error: native and WebAssembly builds disagree" >&2
  exit 1
fi

if [[ "$count" == 1000000 && "$seed" == 42 ]]; then
  for digest in "$native_events" "$native_state"; do
    if ! grep -q "$digest" web/app.js; then
      echo "error: web/app.js does not show the current reference digest $digest" >&2
      exit 1
    fi
  done
fi
echo "ok: identical results for $count commands, seed $seed ($mode)"
