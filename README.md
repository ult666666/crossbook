# crossbook

A limit order book and matching engine in Rust: price-time priority, the order types
real venues use, and a deterministic event stream, checked against a second
implementation and compiled to WebAssembly so it runs in your browser.

[![CI](https://github.com/ult666666/crossbook/actions/workflows/ci.yml/badge.svg)](https://github.com/ult666666/crossbook/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](Cargo.toml)

**[Open the playground](https://ult666666.github.io/crossbook/)**: place orders against a
live book, step through guided scenarios, and run a million synthetic orders in your
browser. The [case study](https://ult666666.github.io/projects/crossbook.html) walks through
the design, the testing and the results.

![The crossbook playground: order ticket, order book ladder with per-level queues, and event log](docs/playground.png)

## What it does

- **Price-time priority.** Better prices trade first; at one price, earlier orders trade
  first. Every trade prints at the resting order's price.
- **Order types.** Limit and market; good-till-canceled, immediate-or-cancel and
  fill-or-kill; post-only; cancel; modify with explicit queue-priority rules; four
  self-trade prevention modes.
- **Events, not state.** Each command emits a precise, ordered list of events: accepted,
  trade, booked, canceled, rejected, modified. The full contract is in
  [docs/SEMANTICS.md](docs/SEMANTICS.md).
- **Deterministic.** The same commands produce the same events on x86-64 Linux, ARM64
  macOS, Windows and WebAssembly. CI checks all four.
- **Crash recovery.** Checksummed snapshots plus a write-ahead journal rebuild the book
  exactly after a crash, and a half-written final command is detected and ignored.
- **Market data.** Trade prints and price-level updates are derived from the event
  stream alone, with sequence numbers, gap detection and resynchronization for
  subscribers.
- **Many instruments.** A sequencer, worker shards and an ordered merge spread
  instruments across threads, with output identical to one thread for any shard count.
- **Small.** The engine crate has no dependencies and no `unsafe` code.

## Results

| | |
| --- | --- |
| Throughput, one core, synthetic flow | 15 to 16 million commands per second |
| Latency per command, p50 / p99 / p99.9 | 64 ns / 196 ns / 477 ns |
| Same session compiled to WebAssembly (Node.js) | about 8.3 million commands per second, with the same event digest and state hash as the native build |
| Differential tests against the reference model | 1,024 random command streams per local run, 4,096 in CI, plus a one-million-command check |
| Mutation testing (cargo-mutants) | Engine: 140 of 148 compiling mutants caught; of the 8 survivors, 4 are now covered by new assertions, 2 pointed at redundant code that was deleted, and 2 only drop a capacity hint. Recovery, market data and sharding: 165 of 175 caught after closing 14 gaps; the 10 survivors only change buffer recycling or comparisons where equality is already ruled out |
| Crash recovery, one million commands | A snapshot at 400,000 commands plus the journal tail reaches the full replay's state hash; loading it and replaying 600,000 commands takes about 0.15 s |
| Market data, one million commands | The book rebuilt from the feed alone equals the engine's; 1.16 messages per command |
| 64 instruments on a 4-core VM | 2.6 to 3.4 times the single-thread rate with 3 or 4 shards, with identical output for every shard count |

Single-book figures: a 2.1 GHz Intel Xeon cloud VM, one pinned core, Rust 1.97 (medians of three seeds, 5 million commands each). Multi-instrument figures: a 2.8 GHz 4-vCPU cloud VM. The commands are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).
The matching core only: no network, parsing or persistence on the timed path.

## Quick start

Needs Rust 1.88 or newer.

```bash
git clone https://github.com/ult666666/crossbook
cd crossbook
cargo run --release -p crossbook-cli -- demo
```

The demo walks through a short session:

```text
# A buy for 8 @ 101 takes the better price first, then the older order at 101
  accepted #4 buy 8 @ 101
  trade 5 @ 100 (taker #4, maker #3)
  trade 3 @ 101 (taker #4, maker #1)

# Fill-or-kill for 20 cannot fill in full, so it is rejected and nothing changes
  rejected #5: fill-or-kill order cannot fill in full
```

Other commands:

```bash
cargo run --release -p crossbook-cli -- bench --orders 5000000    # throughput and latency
cargo run --release -p crossbook-cli -- verify --orders 1000000   # fast engine vs reference model
cargo run --release -p crossbook-cli -- gen --out flow.xbk        # write a binary command journal
cargo run --release -p crossbook-cli -- replay flow.xbk           # replay it and print fingerprints
cargo run --release -p crossbook-cli -- snapshot flow.xbk --at 400000 --out flow.xbs
cargo run --release -p crossbook-cli -- recover flow.xbk --snapshot flow.xbs   # rebuild after a crash
cargo run --release -p crossbook-cli -- feed                      # market data, checked against the book
cargo run --release -p crossbook-cli -- exchange                  # 64 instruments on 1 to 4 shard threads
cargo test --workspace                                            # all tests
```

## Use it as a library

```rust
use crossbook::{Command, Engine, Event, NewOrder, Side, TimeInForce};

let mut engine = Engine::new();
let mut events = Vec::new();

engine.process(&Command::New(NewOrder::limit(1, Side::Sell, 100, 10)), &mut events);
engine.process(
    &Command::New(NewOrder::limit(2, Side::Buy, 100, 15).tif(TimeInForce::Ioc)),
    &mut events,
);

// The IOC buy fills 10 lots and cancels the other 5 instead of resting.
assert!(engine.is_empty());
for event in &events {
    println!("{event}");
}
```

## How it is checked

- **Scenario tests** pin every rule in the spec to exact events.
- **Differential testing.** A second, deliberately naive engine implements the same rules
  with linear scans. Property tests feed random command streams to both and require
  identical events and books after every command, plus structural invariants and
  quantity conservation.
- **Golden digests.** A 64-bit fingerprint of a 100,000-command session is pinned in the
  tests and must match on every CI platform and in WebAssembly.
- **Mutation testing** confirms the tests fail when the engine is broken on purpose.
- **Crash simulation.** For random sessions, the journal is cut at a random byte and
  recovery from the newest snapshot must reach exactly the uninterrupted run's book and
  replay the same events; damaged snapshots must be rejected.
- **Market data.** After every command, a subscriber's book rebuilt from the feed must
  equal the engine's, and a lost message must be detected and repaired.
- **Shard independence.** Random multi-instrument streams must produce exactly the
  single-thread output for any shard count, batch size and queue length.

Details: [docs/TESTING.md](docs/TESTING.md).

## How it works

Price levels sit in one `BTreeMap` per side, so the best price is the first or last key.
Each level is a FIFO queue threaded through a single slab of 48-byte order nodes with
`u32` links, which makes cancel O(1) after the id lookup and reuses memory as orders come
and go. The engine never reads a clock or a random number generator and never iterates a
hash map, which is what makes it deterministic. That determinism carries the rest:
recovery replays a journal from a snapshot, the market-data feed is a function of the
events, and the sharded runner restores one global order with sequence numbers. Design
notes, costs and the alternatives considered: [docs/DESIGN.md](docs/DESIGN.md).

## Project layout

| Path | Contents |
| --- | --- |
| `crates/crossbook` | The engine, the reference model, codec, journal, snapshots and recovery, market data, the sharded runner, synthetic flow, tests and benchmarks |
| `crates/crossbook-cli` | The `crossbook` command: `demo`, `bench`, `verify`, `gen`, `replay`, `snapshot`, `recover`, `feed`, `exchange` |
| `crates/crossbook-wasm` | WebAssembly bindings used by the playground |
| `web/` | The browser playground (static HTML, CSS and JavaScript) |
| `docs/` | Semantics, design, testing and benchmark notes |

## Scope

crossbook is a learning and benchmarking project, not an exchange. Each book holds one
instrument in memory, and the sharded runner hosts many books in one process. There is
no network gateway, the CLI writes journals without `fsync`, and everything runs on
synthetic data. It is not connected to any market. Developed with AI assistance; the tests, CI checks and
benchmark commands above are the evidence for each claim in this README.

## License

[MIT](LICENSE)
