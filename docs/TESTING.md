# Testing

A matching engine fails quietly: a wrong queue order or an off-by-one quantity still
produces plausible-looking trades. crossbook is checked in layers, each aimed at a
different way to be wrong.

| Layer | Where | What it catches |
| --- | --- | --- |
| Scenario tests | `crates/crossbook/tests/semantics.rs` | Each rule in [SEMANTICS.md](SEMANTICS.md) pinned to exact events |
| Differential property tests | `crates/crossbook/tests/differential.rs` | Any disagreement between the fast engine and a simple reference model |
| Structural invariants | `Engine::check_invariants` | Broken links, stale cached totals, crossed books, leaked slots |
| Quantity conservation | `differential.rs` | Lots created or lost by matching |
| Long synthetic sessions | `crates/crossbook/tests/flow.rs`, `crossbook verify` | Bugs that need a deep, realistic book to show up |
| Golden digests | `flow.rs`, CI on three platforms, WebAssembly check | Any change in behavior, and any platform dependence |
| Codec properties | `crates/crossbook/tests/codec.rs` | Encoding round trips; malformed bytes never panic |
| Browser smoke test | `web/smoke.mjs` | The playground loads, runs scenarios and the stress test |
| Mutation testing | `cargo mutants` | Tests that pass no matter what the code does |

Run everything locally with `cargo test --workspace`. CI also runs 4,096 property cases,
a one-million-command differential check, the WebAssembly comparison and the browser
smoke test.

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
commands of seed 42. CI runs it on x86-64 Linux, ARM64 macOS and x86-64 Windows, so a
platform-dependent result fails the build. `scripts/check-determinism.sh` builds the
engine for `wasm32-unknown-unknown`, runs one million commands under Node.js, and
requires the same digests as the native CLI. The browser playground shows the same
reference digests and compares them live.

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

## Browser smoke test

`web/smoke.mjs` serves the playground, loads it in headless Chromium, fails on any
console error, runs a guided scenario, places and cancels an order through the UI, runs
the 100,000-command stress test and checks the native-match confirmation, and checks for
horizontal overflow at phone width.
