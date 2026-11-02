#!/usr/bin/env bash
# Crash recovery end to end on a synthetic session. Writes a journal, replays it in
# full, takes a snapshot part of the way through and recovers from the snapshot plus
# the rest of the journal. Fails unless recovery ends in the same book and phase as the
# full replay, replays every command after the snapshot, and, for a journal cut in the
# middle of its last frame, ignores exactly that frame. A third argument, "auctions",
# uses trading days with auctions; then the snapshot falls inside a closing call phase.
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
work="target/recovery"
mkdir -p "$work"
crossbook=./target/release/crossbook

cargo build --release -p crossbook-cli
"$crossbook" gen --orders "$count" --seed "$seed" ${gen_flags[@]+"${gen_flags[@]}"} --out "$work/flow.xbk" >/dev/null
field() { awk -v key="$1" '$0 ~ "^" key {print $NF}'; }

phase() { awk '/^phase/ {$1 = ""; sub(/^ +/, ""); print}'; }

full="$("$crossbook" replay "$work/flow.xbk" --depth 0)"
full_events="$(field "event digest" <<<"$full")"
full_state="$(field "state hash" <<<"$full")"
full_phase="$(phase <<<"$full")"

at=$((count * 2 / 5))
taken="$("$crossbook" snapshot "$work/flow.xbk" --at "$at" --out "$work/flow.xbs")"
snapshot_phase="$(phase <<<"$taken")"
recovered="$("$crossbook" recover "$work/flow.xbk" --snapshot "$work/flow.xbs")"
recovered_state="$(field "state hash" <<<"$recovered")"
recovered_phase="$(phase <<<"$recovered")"
replayed="$(awk '/^replayed/ {gsub(",", "", $2); print $2}' <<<"$recovered")"

scratch="$("$crossbook" recover "$work/flow.xbk")"
scratch_events="$(field "tail digest" <<<"$scratch")"

# Cut 7 bytes off the last frame, as if the process died while appending it.
size="$(wc -c <"$work/flow.xbk")"
head -c "$((size - 7))" "$work/flow.xbk" >"$work/torn.xbk"
torn="$("$crossbook" recover "$work/torn.xbk" --snapshot "$work/flow.xbs")"
torn_replayed="$(awk '/^replayed/ {gsub(",", "", $2); print $2}' <<<"$torn")"

echo "full replay:          events $full_events  state $full_state  ($full_phase)"
echo "snapshot at $at ($snapshot_phase) + tail: state $recovered_state ($replayed commands replayed)"
echo "recovery from scratch: events $scratch_events"
echo "torn final frame:      $torn_replayed commands replayed"
if [[ "$recovered_state" != "$full_state" || "$recovered_phase" != "$full_phase" \
  || "$replayed" != "$((count - at))" ]]; then
  echo "error: snapshot recovery does not match the full replay" >&2
  exit 1
fi
if [[ "$mode" == auctions && "$snapshot_phase" != "call phase" ]]; then
  echo "error: the auction session's snapshot should fall inside a call phase" >&2
  exit 1
fi
if [[ "$scratch_events" != "$full_events" ]]; then
  echo "error: recovery without a snapshot emitted different events" >&2
  exit 1
fi
if [[ "$torn_replayed" != "$((count - at - 1))" ]] || ! grep -q "torn final frame of" <<<"$torn"; then
  echo "error: the torn final frame was not detected and ignored" >&2
  exit 1
fi
echo "ok: recovery reproduces the full replay and ignores a torn final frame ($mode)"
