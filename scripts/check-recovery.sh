#!/usr/bin/env bash
# Crash recovery end to end on a synthetic session. Writes a journal, replays it in
# full, takes a snapshot part of the way through and recovers from the snapshot plus
# the rest of the journal. Fails unless recovery ends in the same book as the full
# replay, replays every command after the snapshot, and, for a journal cut in the
# middle of its last frame, ignores exactly that frame.
set -euo pipefail
cd "$(dirname "$0")/.."

count="${1:-1000000}"
seed="${2:-42}"
work="target/recovery"
mkdir -p "$work"
crossbook=./target/release/crossbook

cargo build --release -p crossbook-cli
"$crossbook" gen --orders "$count" --seed "$seed" --out "$work/flow.xbk" >/dev/null
field() { awk -v key="$1" '$0 ~ "^" key {print $NF}'; }

full="$("$crossbook" replay "$work/flow.xbk" --depth 0)"
full_events="$(field "event digest" <<<"$full")"
full_state="$(field "state hash" <<<"$full")"

at=$((count * 2 / 5))
"$crossbook" snapshot "$work/flow.xbk" --at "$at" --out "$work/flow.xbs" >/dev/null
recovered="$("$crossbook" recover "$work/flow.xbk" --snapshot "$work/flow.xbs")"
recovered_state="$(field "state hash" <<<"$recovered")"
replayed="$(awk '/^replayed/ {gsub(",", "", $2); print $2}' <<<"$recovered")"

scratch="$("$crossbook" recover "$work/flow.xbk")"
scratch_events="$(field "tail digest" <<<"$scratch")"

# Cut 7 bytes off the last frame, as if the process died while appending it.
size="$(wc -c <"$work/flow.xbk")"
head -c "$((size - 7))" "$work/flow.xbk" >"$work/torn.xbk"
torn="$("$crossbook" recover "$work/torn.xbk" --snapshot "$work/flow.xbs")"
torn_replayed="$(awk '/^replayed/ {gsub(",", "", $2); print $2}' <<<"$torn")"

echo "full replay:          events $full_events  state $full_state"
echo "snapshot at $at + tail: state $recovered_state ($replayed commands replayed)"
echo "recovery from scratch: events $scratch_events"
echo "torn final frame:      $torn_replayed commands replayed"
if [[ "$recovered_state" != "$full_state" || "$replayed" != "$((count - at))" ]]; then
  echo "error: snapshot recovery does not match the full replay" >&2
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
echo "ok: recovery reproduces the full replay and ignores a torn final frame"
