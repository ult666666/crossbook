# Benchmarks

All numbers come from commands in this repository, so anyone can rerun them. They
measure the matching core only: commands are already in memory, there is no network,
no parsing and no persistence on the timed path. Treat them as a property of this code on
one machine, not as a claim about exchange performance.

## Machine

A cloud VM with an Intel Xeon processor at 2.1 GHz (4 vCPUs, 15 GB RAM), Linux 6.18,
Rust 1.97.0 and Node.js 22.22. Each benchmark ran pinned to one core (`taskset -c 2`)
while the machine was otherwise idle.

## Workload

`crossbook bench` replays the synthetic flow from `crossbook::flow`. Measured over one
million commands of seed 42: 40% cancels, 6% modifies, 6.5% marketable orders
(aggressive GTC limit 2.5%, IOC 2.5%, market 1%, FOK 0.5%) and 47.5% passive limit
orders a few ticks from a drifting fair value, about one in seven of them post-only. The book holds about 10,000
resting orders across 30 to 45 price levels per side. The generator runs before timing
starts. The flow is synthetic and is not calibrated to any real market.

## End-to-end throughput and latency

```bash
cargo run --release -p crossbook-cli -- bench --orders 5000000 --seed 42
```

- **Throughput:** one untimed warm-up pass, then the best of three timed passes over the
  same commands, each on a fresh engine.
- **Latency:** a fourth pass times every command with `std::time::Instant` and records it
  in an HDR histogram. The median cost of reading the clock twice is measured first and
  subtracted, so very short commands are reported close to their true cost. Tail values
  (p99.99 and max) include operating-system interrupts and page faults; they show what a
  plain process on a shared machine sees, not a tuned, isolated core.

Five million commands per run:

| Seed | Throughput (commands/s) | p50 | p90 | p99 | p99.9 | p99.99 | max |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 42 | 15.3 M | 64 ns | 116 ns | 207 ns | 486 ns | 16.0 µs | 2.2 ms |
| 1 | 16.0 M | 63 ns | 109 ns | 191 ns | 454 ns | 11.6 µs | 0.56 ms |
| 2 | 15.7 M | 65 ns | 114 ns | 196 ns | 477 ns | 12.1 µs | 2.1 ms |

Timer overhead was 24 ns per measurement and is already subtracted. Each run emitted
about 8.2 million events and ended with about 10,000 resting orders.

## Micro-benchmarks

```bash
cargo bench -p crossbook --bench engine
```

Criterion measures single operations against a realistic book (the state after 100,000
commands of seed 42, about 9,000 resting orders):

| Benchmark | Time |
| --- | --- |
| Add a passive order and cancel it | 58 ns |
| Cancel an order in the middle of the book | 127 ns |
| Aggressive limit order that fills one resting order | 82 ns |
| Market order sweeping the best 10 ask levels (3,861 orders, 73,756 lots) | 58.6 µs, about 15 ns per fill |
| Fast engine, 100,000-command session | 6.5 ms (15.5 M commands/s) |
| Fast engine, 10,000 commands on a 500-order book | 0.55 ms (18.1 M commands/s) |
| Reference model, the same 10,000 commands | 5.96 ms (1.7 M commands/s) |

Single-operation benchmarks that need a fresh book clone the warm book outside the timed
region for every iteration, so they include cold-cache effects.

The reference model is the linear-scan implementation used for differential testing.
Even on a 500-order book it is about ten times slower; on the 10,000-order book above it
would be far slower, which is why the CLI's `verify` command defaults to a smaller book.

## WebAssembly

The same engine compiled to `wasm32-unknown-unknown` and run under Node.js 22 on the
same machine processes 8.2 to 8.5 million commands per second (engine time only; generating the
commands takes another 230 ms per million), and produces the same event digest and state hash
as the native build. Open the [playground](https://ult666666.github.io/crossbook/) to
measure your own browser.

## Reproducing on GitHub's runners

The `Benchmark` workflow (`.github/workflows/bench.yml`, run manually) builds the CLI and
the Criterion suite on an `ubuntu-24.04` runner and uploads the results as an artifact.
