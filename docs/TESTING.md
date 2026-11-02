# Testing

A matching engine fails quietly: a wrong queue order or an off-by-one quantity still
produces plausible-looking trades. crossbook is checked in layers, each aimed at a
different way to be wrong.

| Layer | Where | What it catches |
| --- | --- | --- |
| Scenario tests | `crates/crossbook/tests/semantics.rs` | Each rule in [SEMANTICS.md](SEMANTICS.md) pinned to exact events |
| Differential property tests | `crates/crossbook/tests/differential.rs` | Any disagreement between the fast engine and a simple reference model, with and without auctions |
| Auction price against brute force | `crates/crossbook/tests/auction.rs` | A clearing price, volume or surplus that differs from trying every candidate price; a tie rule that never fires |
| Uncross invariants | `tests/common/mod.rs` (`check_uncross`) | A fill beyond its limit or away from the clearing price, lost or invented lots, a skipped queue position, a book still crossed afterwards |
| Structural invariants | `Engine::check_invariants` | Broken links, stale cached totals, crossed books, leaked slots |
| Quantity conservation | `differential.rs` | Lots created or lost by matching |
| Long synthetic sessions | `crates/crossbook/tests/flow.rs`, `crossbook verify` | Bugs that need a deep, realistic book to show up |
| Golden digests | `flow.rs`, CI on three platforms, WebAssembly check | Any change in behavior, and any platform dependence |
| Codec properties | `crates/crossbook/tests/codec.rs` | Encoding round trips; malformed bytes never panic |
| Crash recovery | `crates/crossbook/tests/recovery.rs`, `scripts/check-recovery.sh` | Recovery that loses, repeats or changes a command, including inside a call phase; snapshots that misread damaged bytes; files from 0.2 that no longer load |
| Market data | `crates/crossbook/tests/market_data.rs` | A feed whose rebuilt book drifts from the engine; lost messages that go unnoticed |
| Shard independence | `crates/crossbook/tests/exchange.rs`, `crossbook exchange` | Any output that depends on the number of threads, the batch size or timing |
| Browser smoke test | `web/smoke.mjs` | The playground loads, runs scenarios and the stress test |
| Mutation testing | `cargo mutants` | Tests that pass no matter what the code does |

Run everything locally with `cargo test --workspace`. CI also runs 4,096 property cases,
a one-million-command differential check, the WebAssembly comparison, crash recovery on
one million commands, the shard check on three operating systems and the browser smoke
test, each also on ten synthetic trading days with opening and closing auctions
(`--auctions`). The generators for random command streams live in `tests/common/mod.rs` and are
shared by every property test.

## Two implementations of one spec

`ReferenceEngine` (`crates/crossbook/src/reference.rs`) implements the matching rules
with no clever data structures: one `Vec` of resting orders, linear scans to find the
best maker, and fill-or-kill decided by running the order as immediate-or-cancel on a
copy of the book. It is roughly ten times slower even on a small book, and that is the
point: it is easy to read and hard to get wrong in the same way as the fast engine.

The property tests generate random command streams and run them through both engines.
After every command they require:

1. identical event lists, in the same order;
2. identical books (`snapshot()` lists every resting order in canonical order);
3. `Engine::check_invariants()` passes: every level's linked list is consistent in both
   directions, cached totals and counts match the queue, every order sits in the right
   level, time priority increases along each queue, the id index matches the slab, no
   slot leaked, and the best bid is below the best ask;
4. the events of each new order account for its full quantity: the lots traded plus the
   lots booked or canceled equal the lots entered, and no trade breaks the limit price.

The generators are tuned to hit edge cases often: ids come from a range of 30 so
duplicate and unknown ids are common, prices sit in a 10-tick band so orders cross,
three accounts make self-trade prevention fire, and quantities include 0, `MAX_QTY` and
`MAX_QTY + 1`. When a case fails, proptest shrinks it to a minimal command list and
stores the seed in `tests/differential.proptest-regressions`, which is re-run first on
every later run.

Two more properties hold for every stream: a rejected command never changes the book,
and running the same commands twice gives the same events and state hash.

A second differential property runs sessions with auctions: about one command in eleven
starts a call, uncrosses or resumes, and call phases add GTC market orders and limit
orders that cross the book. After every command the two engines must also agree on the
phase, the reference and last trade prices, the waiting market orders and the indicative
uncross, and every uncross must pass `check_uncross`.

## Call auctions

The engine finds the clearing price in one pass over its price levels
(`crates/crossbook/src/auction.rs`). `reference::auction_price` does it the slow way: it
computes demand and supply at every distinct limit price by scanning every order, then
applies the five rules literally, keeping the tied candidates after each. The reference
model's uncross uses it and allocates by repeatedly picking the best remaining buy and sell
with linear scans.

`tests/auction.rs` compares the two on small books: up to 13 orders on five price levels,
one to four lots each, one in six a market order, with and without a reference price.
Small books tie often, so every rule gets exercised; a seeded run of 20,000 books checks
that it does. On those books the deciding rule was volume for 6,895, surplus for 4,762,
market pressure for 2,017, the reference price for 346 and the lowest price for 126; 593
books held only market orders and 5,261 could not trade (`cargo test -p crossbook --release
--test auction -- --nocapture`). The test fails if any rule decides fewer than 100.

After every uncross, in these tests and in the differential and flow tests,
`check_uncross` verifies against the book just before it:

1. the summary equals the brute-force price determination;
2. every fill is at the clearing price and no worse than the order's limit;
3. each order's fills never exceed its open quantity, and the reported remainders match;
4. the fills add up to the volume on both sides (volume is conserved);
5. each side fills a prefix of its priority order (market orders, then price, then time),
   so only the last order reached can fill partly;
6. the side without a surplus fills completely and the other keeps exactly the surplus;
7. waiting market orders are canceled for exactly their unfilled quantity, and the book is
   no longer crossed.

Scenario tests in `semantics.rs` pin each rule and each order type's behavior in a call
phase to exact events, including tie cases where a later rule decides.

**Hand-written mutants.** Seven bugs were injected one at a time into the auction code and
run against the auction, scenario and differential tests, which caught all seven: market
pressure choosing the wrong end, the final tie-break taking the higher price, the surplus
rule skipped, market orders filling after limit orders, orders at the clearing price unable
to fill on the sell side, market remainders left on the book, and `Resume` skipping the
uncross of a crossed book.

## Long sessions

Short random streams keep books shallow. `tests/flow.rs` runs 40,000 commands from the
synthetic flow generator for three seeds through both engines, comparing events after
every command and books every 500 commands. The CLI does the same at scale:

```bash
cargo run --release -p crossbook-cli -- verify --orders 1000000 --seed 7
```

The generator keeps its own shadow copy of the book, so its cancels and modifies target
orders that actually rest, and its cancel rate steers the book toward a target size.

## Determinism across platforms

`tests/flow.rs` pins the event digest, state hash and resting-order count after 100,000
commands of seed 42, and after 200,000 commands of seed 42 as trading days with auctions. CI runs it on x86-64 Linux, ARM64 macOS and x86-64 Windows, so a
platform-dependent result fails the build. `scripts/check-determinism.sh` builds the
engine for `wasm32-unknown-unknown`, runs one million commands under Node.js, and
requires the same digests as the native CLI; CI runs it for a continuous session and for
one with auctions. The browser playground shows the same
reference digests and compares them live.

## Crash recovery

`tests/recovery.rs` simulates a crash for every random command stream, using sessions with
auctions. The session runs
uninterrupted while its journal is written and a snapshot is taken every few commands.
Then the journal is cut at a random byte, as if the process died in the middle of an
append, and recovery starts from the newest snapshot the surviving journal covers. The
recovered engine must hold exactly the state the uninterrupted run held after the same
number of commands, pass `check_invariants`, report the torn bytes exactly, and the
replayed tail must emit the same events, in order. The state comparison is byte for byte on
a fresh snapshot of each engine, which covers the book, the time priorities, the clock, the
phase and both auction prices. A second property starts a call phase and then cuts the
journal at a random byte inside it, so recovery must restore a crossed call book with
waiting market orders and land in the same call.

Two files written by crossbook 0.2 (`tests/fixtures/v1-journal.bin` and
`v1-snapshot.bin`, format version 1) must still recover to the digests 0.2 printed for them.

Two more properties: an engine restored from a snapshot behaves exactly like the
original for any later commands, and its own next snapshot is byte-identical; and
flipping any byte of a snapshot makes decoding fail instead of producing a different
book. (FNV-1a turns a change to any one byte into a different checksum, so this holds
for every position.) Random bytes never make the decoder panic.

`scripts/check-recovery.sh` repeats the check through the CLI on one million commands:
a snapshot at 40% plus the rest of the journal must reach the full replay's state hash and
phase, recovery without a snapshot must emit the full replay's event digest, and a journal
cut inside its last frame must lose exactly that command. With `auctions` as its third
argument the session has trading days, and the snapshot at 40% falls inside the fourth
day's closing call; the script checks that it does.

## Market data

`tests/market_data.rs` runs random streams with auctions through the engine and the
publisher and feeds every message to a subscriber. After every command the subscriber's book must
equal `Engine::depth` level by level, the trade prints must mirror the trade events, and
sequence numbers must have no gaps; auction results and phase changes must mirror the
`Uncrossed`, `CallStarted` and `Resumed` events, and the subscriber's phase must equal the
engine's. A second property drops one random message and
requires the subscriber to notice by the end of that command, from a later message or
the heartbeat, and to hold the engine's book again after resynchronizing. A 40,000
command synthetic session and two synthetic trading days with auctions check deep books,
sweeps across many levels and uncrosses, and `crossbook feed` repeats the check on one
million commands, with and without `--auctions`.

## Shard independence

`tests/exchange.rs` runs random multi-instrument streams, including each instrument's own
call phases and uncrosses, through `exchange::run` with a random shard count, batch size
and queue length, and requires exactly the output of one
thread processing the commands in order: the same events, for the same instruments, in
the same order. A synthetic session over 24 instruments, each with its own opening and closing auctions,
must give the same digest with 1, 2, 3, 4 and 7 shards and batch sizes from 1 to 4,096. CI also runs
`crossbook exchange` on Linux, macOS and Windows, where thread scheduling differs, and
the command fails if any shard count changes the digest.

## Mutation testing

Passing tests prove little if they would also pass with a broken engine. Mutation testing
changes the code on purpose (flips a comparison, deletes a statement, returns a
constant) and checks that some test fails.

**Hand-written mutants.** Six realistic bugs were injected one at a time and run against
the scenario and differential suites. The suites failed for all six:

- fill-or-kill ignores the cancel-oldest skip;
- buy orders match the worst ask first;
- a same-quantity modify loses priority;
- self-trade prevention compares accounts the wrong way;
- a fill does not reduce the level's cached total;
- the post-only check looks at the wrong side of the book.

**cargo-mutants.** A full run over `engine.rs`, `types.rs` and `codec.rs` generated 168
mutants (hasher internals, capacity hints, `Display` text and the invariant checker
itself were excluded). 20 did not compile. Of the other 148, the tests caught 140, two of
them by timing out because the mutant looped forever. The 8 survivors:

- `is_empty` returning `true`, `level_count` returning a constant, and quantity
  `MAX_QTY` itself being rejected: real gaps, now covered by new assertions;
- two link fields set in `modify` that `link_back` always overwrites: redundant code,
  now deleted;
- two capacity hints in `with_capacity`: removing them changes allocation, not behavior.

Rerun it with:

```bash
cargo install cargo-mutants
cargo mutants -p crossbook --file crates/crossbook/src/engine.rs \
  --file crates/crossbook/src/types.rs --file crates/crossbook/src/codec.rs \
  --exclude-re 'IdHasher|with_capacity|check_invariants|fmt'
```

**The 0.2 modules.** A second run covered `snapshot.rs`, `recovery.rs`,
`market_data.rs` and `exchange.rs`: 182 mutants, 7 of which did not compile. The first
pass caught 151 of the other 175. The 24 survivors pointed at 14 real gaps, among them
routing every instrument to one shard (correct output, no parallelism), a publisher
check weakened so that a trade could hit an order on the wrong side or at another price,
the subscriber's sequence and sync accessors, the cause chained by recovery errors, and
exact snapshot length boundaries. Tests now cover all 14, and the rerun caught 165 of
175. The 10 survivors:

- 7 in `exchange::run` change only buffer recycling or the capacity of the queues that
  return spare buffers: the output is identical, only allocation changes;
- 3 in `snapshot::decode` turn `<` into `<=` (or `>` into `>=`) where equality was
  already rejected by an earlier check, so they cannot change a result.

```bash
cargo mutants -p crossbook --file crates/crossbook/src/snapshot.rs \
  --file crates/crossbook/src/recovery.rs --file crates/crossbook/src/market_data.rs \
  --file crates/crossbook/src/exchange.rs --exclude-re 'fmt|Display|SymbolHasher'
```

**The 0.3 auction code.** A third run covered only the code added for call auctions: all of
`auction.rs`, the phase, uncross and allocation functions in `engine.rs`, `static_reject`,
the new codec frames, the journal and snapshot version handling, the publisher's auction
handling and the session schedule. It generated 325 mutants, 2 of which did not compile.
The tests caught 307, and 3 more by timing out because the mutant looped forever. The 13
survivors:

- 5 showed code that could change without any visible effect, or an untested case, and
  are fixed: the allocation walk kept collecting zero-lot fills after the volume was covered
  (it now stops), the schedule found the first day by division (now a comparison), a
  snapshot whose last trade price is 0 with its flag set was not tested, and two in the
  snapshot decoder's best-bid bookkeeping, which is now simpler and tested with an ask that
  lies between two bids;
- 2 turn `|` into `^` when combining bit flags that never overlap, so the result is the same;
- 4 turn `<` into `<=` (or `>` into `>=`) in the snapshot's queue-order checks, where
  equality is already rejected by the duplicate-priority check or by the equal-price arm,
  like the 3 survivors of the 0.2 run;
- 2 delete the capacity hints in `Engine::with_capacity`, which change allocation, not
  behavior (the first run found the same).

A rerun of the 126 mutants in the changed functions caught 118 and left exactly the 8
equivalent ones listed above. The reported runs limited `snapshot::decode` to the lines
that changed (by line number); the command below selects whole functions, so it also
repeats the 0.2 checks in `decode`. The lower case counts keep a run near 20 minutes on
a 4-vCPU machine.

```bash
PROPTEST_CASES=32 CROSSBOOK_FLOW_COMMANDS=12000 cargo mutants -p crossbook --jobs 2 --re \
  'auction\.rs:|engine\.rs:.* in (Engine::(start_call|resume|uncross|allocate|collect_fills|fill|indicative_uncross|market_orders|session_state|phase|reference_price|last_trade_price|rest|detach)|market_queue)$|types\.rs:.* in (static_reject|OrderView::is_market)$|codec\.rs:.* in (frame_len|decode|encode)$|journal\.rs:.* in Reader.*::(new|next_frame|version)$|snapshot\.rs:.* in (encode|decode|session_state)$|market_data\.rs:.* in (Publisher::(auction_fill|status|publish|snapshot)|BookBuilder::(apply|phase|last_auction|resync|from_snapshot))$|flow\.rs:.* in (Flow::(scheduled|generate_call)|Schedule::day_len)$'
```

## Browser smoke test

`web/smoke.mjs` serves the playground, loads it in headless Chromium, fails on any
console error, runs a guided scenario, runs the opening-auction scenario (the indicative
uncross, five auction trades at 101 and the return to continuous trading), places and
cancels an order through the UI, runs the 100,000-command stress test and checks the
native-match confirmation, and checks for horizontal overflow at phone width.
